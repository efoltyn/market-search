# ELI CLI Agent Improvement Plan: The Financial Thinker
## Transform eli into an AI Financial Researcher & Instinct-Builder

---

## The Vision

**eli** is no longer just a coder. It is a **Financial Intelligence Harness** designed to:
- **Skip the Noise**: Disable web search to avoid consensus bias.
- **Analyze Raw Data**: Fetch time-series data (granularity/range) for stocks, commodities, and macro indicators.
- **Build Instincts**: Use self-reflection loops to Compare predictions vs. outcomes and store "patterns of change" in specialized `.md` files.
- **Exploit the Knowledge Cutoff**: Use the period between the model's training and "now" as a sandbox to prove its reasoning.

---

## Phase 1: The Financial Foundation

### 1.1 Finance Tool Implementation
**Target**: A high-performance Rust tool to fetch OHLCV and macro data.
- [ ] Implement `FinanceTool` in `eli-core`.
- [ ] Fields: `ticker`, `granularity` (1m, 1h, 1d, 1y), `range` (1d, 1mo, 10y).
- [ ] Suppress "summarization" layers; allow the model to ingest raw data.

### 1.2 "Zoom In / Zoom Out" Logic
**Target**: System prompts that guide the model to explore data recursively.
- [ ] Prompt the agent to look at 50 years of Gold vs. 50 years of Silver.
- [ ] Enable the "instinct" to look at correlated assets (Bonds, USD, Oil).

---

## Phase 2: The Self-Reflection Harness

### 2.1 The Prediction Sandbox
**Target**: A workflow for historical replay.
- [ ] "Pick a Day": Fetch data up to a historical point.
- [ ] "Predict": The model makes a binary or probabilistic bet (Kalshi-style).
- [ ] "Verify": Fetch the actual outcome.
- [ ] "Reflect": Write the self-reflection (reasoning vs. reality) to a dedicated `.md` file.

### 2.2 Instinct Storage (Institutional Memory)
**Target**: Specialized files that track "Dynamics of Change".
- [ ] Files like `instincts/gold.md`, `instincts/macro_usd.md`.
- [ ] Content focus: Explanations of *why* reasoning failed or succeeded, not just lists of facts.

---

## Phase 3: Scaling & Distributed Intelligence

### 3.1 Distributed Training Harness
**Target**: Collecting high-quality synthetic data from CLI usage.
- [ ] Securely store binary prediction outcomes and reflections.
- [ ] Use this data to fine-tune smaller, cheaper models to mimic the "instincts" of the larger ones.

### 3.2 Web Interface (Hybrid Model)
**Target**: A Next.js frontend for the B2B financial services market.
- [ ] Project the terminal "thinking" process into a clean UI.
- [ ] Private context/instinct layers for institutional users.

---

## Technical Recommendations

### Rust Core
- Integrate `finance` module with `eli-core::agent` for direct context injection.
- Enhance `Persistence` to handle specialized reflection documents.

### System Prompt Refactor
- Disable web search capabilities.
- Focus on "Quantitative Intuition" and "Cross-Asset Correlation".

---

## Success Metrics
- **Novelty**: Ability to surface insights not found in standard financial media.
- **Accuracy**: Improvement in binary prediction sets over 100+ iterations.
- **Cost**: Efficiency of "instinct-driven" reasoning vs. brute-force context windows.

