# Code Manual Refactor Plan

## 1) Baseline (Measured)

- `crates/eli-cli/src/lib.rs`: `13,375` LOC, `228` top-level functions (`syn_map`)
- `crates/eli-core/src/finance/mod.rs`: `6,936` LOC, `70` top-level functions (`syn_map`)
- Goal: no single file above `~500` LOC in hot paths, most files `150-400` LOC.

## 2) Refactor Rule (No Rebuild Trap)

1. Do **filesystem moves first**.
2. Parse-check files with `target/debug/syn_map <file>` after each move.
3. Run **one** `cargo check` only after modules are fully wired.

## 3) Finance Manual Layout

Current: `crates/eli-core/src/finance/mod.rs` is still a mixed mega-file.

Target:

`crates/eli-core/src/finance/`

- `mod.rs`  
  Purpose: thin facade only (`mod ...`, `pub use ...`).

- `types.rs`  
  Purpose: all public request/response/types (already started).

- `sync/`
  - `mod.rs`  
  - `run_sync.rs`  
  - `analysis.rs`  
  - `csv_cache_writer.rs`

- `providers/`
  - `mod.rs`
  - `kalshi_sync.rs`
  - `polymarket_sync.rs`
  - `odds_kalshi.rs`  
    Purpose: Kalshi odds fetch/list/orderbook logic.
  - `odds_polymarket.rs`  
    Purpose: Polymarket odds fetch/list/book/ws logic.
  - `yahoo.rs`

- `timeseries/`
  - `mod.rs`
  - `fetch.rs`  
  - `analytics.rs`
  - `resample.rs`
  - `mock.rs`
  - `yahoo.rs`
  - `fred.rs`

- `prices/`
  - `mod.rs`
  - `fetch.rs`

- `fundamentals/`
  - `mod.rs`
  - `fetch.rs`

- `search/`
  - `mod.rs`
  - `fetch.rs`

- `snapshot/`
  - `mod.rs`
  - `fetch.rs`
  - `analytics.rs`

- `options/`
  - `mod.rs`
  - `fetch.rs`
  - `model.rs`

- `filings/`
  - `mod.rs`
  - `sec_client.rs`
  - `sec_fetch.rs`
  - `excerpt.rs`
  - `cache.rs`

- `insider/`
  - `mod.rs`
  - `fetch.rs`
  - `form4_parse.rs`
  - `xml_extract.rs`

- `news/`
  - `mod.rs`
  - `fetch.rs`

- `macro_data/`
  - `mod.rs`
  - `fetch.rs`

- `schedule/`
  - `mod.rs`
  - `fetch.rs`
  - `nasdaq.rs`

- `util/`
  - `mod.rs`
  - `json.rs`
  - `strings.rs`
  - `cache_debug.rs`

## 4) CLI Manual Layout

Current: `crates/eli-cli/src/lib.rs` is command routing + implementation + TUI + summaries.

Target:

`crates/eli-cli/src/`

- `lib.rs`  
  Purpose: minimal entry and top-level dispatch wiring.

- `commands/`
  - `mod.rs`
  - `finance.rs`
  - `web.rs`
  - `tools.rs`
  - `chat.rs`
  - `agent.rs`
  - `setup.rs`
  - `config.rs`

- `agent_runtime/`
  - `mod.rs`
  - `direct_route.rs`
  - `worker.rs`
  - `steps.rs`
  - `swarm.rs`
  - `persistence.rs`

- `output/`
  - `mod.rs`
  - `summary_finance.rs`
  - `summary_web.rs`
  - `summary_tools.rs`
  - `json_out.rs`
  - `digest.rs`

- `tui/`
  - `mod.rs`
  - `chat.rs`
  - `agent.rs`
  - `render.rs`
  - `events.rs`
  - `widgets.rs`

- `prompt/`
  - `mod.rs`
  - `modes.rs`
  - `history.rs`
  - `menu.rs`

- `paths/`
  - `mod.rs`
  - `research.rs`
  - `artifacts.rs`

## 5) Move Order (Deterministic)

1. `finance`: move odds provider logic from `mod.rs` -> `providers/odds_kalshi.rs`, `providers/odds_polymarket.rs`.
2. `finance`: move helper clusters into `util/json.rs`, `util/strings.rs`, `util/cache_debug.rs`.
3. `finance`: move feature clusters (`timeseries`, `options`, `filings`, `insider`, `schedule`).
4. Keep `finance/mod.rs` as facade.
5. `cli`: split command handlers (`cmd_finance`, `cmd_web`, `cmd_tools`, `cmd_chat`, `cmd_agent`) first.
6. `cli`: split agent runtime (`run_agent_steps`, `try_agent_direct_route`) next.
7. `cli`: split output summary functions by domain.
8. Keep `cli/lib.rs` as facade + bootstrapping only.

## 6) Acceptance Targets

- `finance/mod.rs` target: `<700 LOC`
- `eli-cli/src/lib.rs` target: `<1200 LOC`
- Every extracted file parseable via `syn_map`
- One final `cargo check` only after full wiring is complete
