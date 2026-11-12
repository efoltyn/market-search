# Market Search plugin

Gives Claude, Codex and other agents live market data through the [market-search](https://github.com/efoltyn/market-search) MCP server: Kalshi and Polymarket odds, stocks, options, futures curves, corporate and government bonds, FRED macro, Treasury auctions, SEC filings, and Fed, ECB, BOJ, BOE and BIS data, all on one timeseries axis.

It runs on your own machine and fetches from your own connection. No API keys, no account. Data requests go straight from your machine to the public sources.

## What it installs

- An MCP server, started by `scripts/launch.sh`. If `market-search` is already installed it uses that binary. Otherwise it downloads the pinned release binary for your OS from GitHub, checks it against the SHA-256 written in the script, and caches it under `~/.cache/market-search/`.
- A skill that tells the model which of the 25 data tools to use for which question, and to report a broken tool with `report_issue`.

macOS and Linux (x86_64). On Windows, install with `irm https://eliterminal.com/install.ps1 | iex`.

## Install

Claude Code:

```
claude plugin marketplace add efoltyn/market-search
claude plugin install market-search@market-search
```

Codex:

```
codex plugin marketplace add efoltyn/market-search
```

## Privacy

The plugin collects nothing about you. Requests go from your machine directly to the public data sources.

One tool, `report_issue`, lets your agent send the developer a short note when a tool errors or returns a wrong number. The note goes to the developer's inbox on [tell.dev](https://tell.dev). Nothing is sent unless the agent calls it, a copy is kept in the local activity trail, and setting `MARKET_SEARCH_FEEDBACK=off` removes the tool. See the [privacy policy](https://github.com/efoltyn/market-search/blob/main/PRIVACY.md).
