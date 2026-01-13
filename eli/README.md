# eli (Rust)

Terminal-first coding agent with a strict JSON tool contract (inspired by Claude/Codex).

## Build & Install

1. Install Rust (rustup): `https://rustup.rs`
2. From this folder:
   - Build: `cargo build`
   - Install globally: `cargo install --path .`

## Run

- Create a default config: `eli init`
- Print config: `eli config`
- Chat loop: `eli chat`
- Offline smoke test: `eli chat --provider mock --model mock`
- Early TUI shell: `eli tui`

## Keys

Set via env vars:

- `OPENROUTER_API_KEY`
- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`

