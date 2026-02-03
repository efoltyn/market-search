# eli-code — Repo Guide (What this is + how it works)

This repo contains **Eli**, a Rust CLI that drives an LLM via a strict JSON “tool contract”, plus a static website with an optional local “live terminal” demo.

Eli’s core idea: the model does not just answer. It emits a structured plan plus **commands to run** and **diffs to apply**. The CLI executes them, feeds back the results, and repeats until the model says `DONE` (or hits safety limits).

## Repo Layout

- `eli/` — Rust workspace (the `eli` binary + internal crates)
- `eli website/` — Static landing page + local demo server for an in-page terminal
- `eli_research/` — Generated research reports (created at runtime)
- `instincts/` — Optional reflection files (created at runtime)

## How Eli Uses Tools (The Agent Loop)

When you run `eli` in chat/research mode:

1. The CLI builds a prompt that includes a system contract (JSON schema + rules).
2. The LLM replies with **strict JSON** (`eli_core::contract::ModelResponse`) containing:
   - `commands`: shell commands to run (e.g., generate and run Python)
   - `diffs`: file edits (create/replace/patch/delete)
   - optional `subagents`: parallel helper tasks
   - `status`: `KEEP_WORKING` or `DONE`
3. The CLI validates the JSON, applies safety/policy checks, runs the commands, applies diffs, then sends the results back to the model as “tool output” for the next step.
4. In research mode, the CLI also enforces finance discipline (no web scraping; fetch raw data with built-in commands first).

Key implementations:

- Tool contract + system prompt: `eli/crates/eli-core/src/contract/mod.rs`
- Main agent loop, approvals, command execution, diff application: `eli/crates/eli-cli/src/lib.rs`
- Provider adapters (OpenRouter/OpenAI/Anthropic/Ollama/mock): `eli/crates/eli-adapters/`

### Modes / Profiles

- **Chat** (`eli` / `eli chat`): coding-agent profile (shell commands + diffs for general local work).
- **Research** (`eli research "<question>"`): finance profile with extra safety checks:
  - requires at least one successful market data fetch via `eli finance timeseries` or `eli finance snapshot` before the model can finish
  - blocks common web-fetch commands and URLs in shell commands (so the model uses the finance tools instead)
- **TUI** (`eli tui`): early ratatui UI shell.

## “Tools” In This Codebase

Eli has two kinds of tools:

### 1) Built-in Finance Tools (Native Rust subcommands)

These are first-party CLI commands that return structured JSON. They exist so the model can fetch real inputs instead of inventing numbers.

- `eli finance timeseries` — OHLCV candles for tickers across a range/granularity.
  - Providers: `yahoo` (prices), `fred` (macro series IDs), `mock` (offline deterministic data).
  - Caching: keyed by SHA256 under the cache dir (`--cache-dir` override; writes JSON cache files).
  - Output: prints JSON to stdout or writes `--out <file.json>` and prints `{"ok":true,"path":...}`.
- `eli finance snapshot` — Point-in-time snapshot for tickers (price + market cap/shares/EV when available).
- `eli finance fundamentals` — Quarterly financial statements (income statement, balance sheet, cash flow).
- `eli finance filings` — Recent SEC filings (8‑K/10‑K/10‑Q). With `--include-text`, downloads primary docs, converts to text, and saves under the cache dir.
  - Requires a user agent: set `ELI_SEC_USER_AGENT` (SEC blocks anonymous/default clients).
- `eli finance news` — News context for a ticker on a date (Google News RSS search around that day).
- `eli finance search` — Finds ticker symbols (Yahoo search) and macro series IDs.
- `eli finance macro` — Pulls a small fixed set of macro indicators and computes recent changes (built on FRED series fetches).

Implementation: `eli/crates/eli-core/src/finance/mod.rs` and wrappers in `eli/crates/eli-cli/src/lib.rs`.

### 2) Shell Commands + File Diffs (The “coding agent” tools)

The model can ask Eli to run shell commands and edit files via diffs. Common pattern for quant work:

- Fetch raw data with `eli finance ... --out ...`
- Write a Python script (via a heredoc) to compute returns/correlation/divergence
- Run `python3 script.py` and read the real output

Research-mode safety is enforced in the CLI (examples):

- `curl`/`wget`/URL fetching is denied in research mode
- URLs embedded in commands are denied unless the command is `eli finance ...`
- Attempts to fetch network data via `python`/`node` (e.g., `requests`, `urllib`, `fetch(...)`) are denied

Policy enforcement lives in `eli/crates/eli-cli/src/lib.rs` (see `deny_reason_for_research_command`).

### 3) Subagents (Optional parallel helpers)

The model can request subagents for small, narrow tasks (repo mapping, quick reviews, test planning). Subagents return short text that is fed back into the main loop.

## Defaults / Configuration

Config is stored in the platform config directory (created by `eli init` / `eli setup`).

Defaults (see `eli/crates/eli-core/src/config/mod.rs`):

- Provider: `openrouter`
- Model: `arcee-ai/trinity-large-preview:free`
- `auto = true`, `max_auto = 50` (step cap)
- `follow_cwd = true`

API keys via env vars:

- `OPENROUTER_API_KEY`
- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`

Useful config commands:

- Print current config: `eli config`
- Set provider/model: `eli config --set provider --value openrouter` and `eli config --set model --value arcee-ai/trinity-large-preview:free`

## Dev Commands (Rust CLI)

### Fast local build (required after Rust changes)

Use this flow instead of `cargo install`. It rebuilds fast and keeps a single build output in `eli/target_local`, with a stable symlink at `bin/eli`.

One-time fetch (needed once per cache reset):

```
cd ~/Desktop/eli-code/eli
CARGO_HOME=$(pwd)/.cargo_local_local CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse cargo fetch
```

Every time Rust changes:

```
cd ~/Desktop/eli-code/eli
CARGO_HOME=$(pwd)/.cargo_local_local \
CARGO_TARGET_DIR=$(pwd)/target_local \
CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse \
cargo build -p eli-cli --bin eli

ln -sf $(pwd)/target_local/debug/eli ../bin/eli
```

Run:

```
eli chat
```

### Other dev commands

- Run help: `cd eli && cargo run -- --help`
- Run chat (slow vs. symlinked binary): `cd eli && cargo run -- chat`
- Run research: `cd eli && cargo run -- research \"<question>\"`
- Finance tools (raw data): `cd eli && cargo run -- finance --help`

## Website Demo (Terminal in HTML)

Browsers cannot execute the Rust CLI directly; the “real demo terminal” uses a tiny local server.

- Start: `cd \"eli website\" && python3 demo_server.py`
- Open: `http://127.0.0.1:8000`

`eli website/demo_server.py` is intentionally restrictive:

- Allowlist: only `eli --help`/`--version` and `eli finance timeseries ...`
- Forces output/cache under `eli website/.demo/` so results are fetchable by the page
- Defaults to `--provider mock` for offline demos
- To allow real network-backed data (Yahoo/FRED), explicitly opt in:
  - `ELI_WEB_DEMO_ALLOW_NET=1 python3 demo_server.py`

Do not broaden server command execution without an explicit allowlist and clear intent.

## Conventions (Editing This Repo)

- Keep changes focused; avoid drive-by refactors.
- Rust: prefer extending `eli-core` for data/contracts and `eli-cli` for UX/loop logic; avoid `unsafe`.
- Website: keep dependencies minimal; preserve keyboard/accessibility for the terminal UI.
