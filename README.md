# market-search

Your AI has web search. Ask it about recession risk or oil, and it summarizes articles.

This gives it market search: the live odds of recession on Kalshi and Polymarket, and the price of oil, as structured data it can read and line up on one axis.

```
> finance_odds --search "recession" --live

  kalshi        36.5%  vol=82,231,200   Will there be a recession in 2026?
  polymarket    36.5%  vol=1,824,634    US recession by end of 2026
  kalshi        22.5%  vol=2,965,000    Will the IMF declare a global recession before 2027?
```

```
> finance_timeseries --tickers SPY,CL=F,^VIX,KXRECSSNBER-26 --range 30d

  SPY                          22 candles   $682 → $655   -4.0%
  CL=F                         22 candles    $66 → $89   +33.8%
  ^VIX                         22 candles     21 → 26    +22.5%
  KALSHI:KXRECSSNBER-26:YES    30 candles    22% → 36%   +63.6%
```

25 tools, one timeseries axis. Stocks, options, futures curves, corporate bonds, government bond yields, crypto, forex, prediction markets, FRED macro, Treasury auctions, SEC filings, and central banks (Fed, ECB, BOJ, BOE, BIS). Runs on your machine. No API keys for the core data.

---

## Install

**Mac:** download [Market Search](https://github.com/efoltyn/market-search/releases/latest/download/MarketSearch.dmg), open it, and click Connect. It finds the AI apps on your Mac and connects each one. No terminal.

**Mac or Linux, in a terminal:**

```bash
curl -fsSL https://eliterminal.com/install.sh | sh
```

**Windows, in PowerShell:**

```powershell
irm https://eliterminal.com/install.ps1 | iex
```

Both install one binary and run `market-search setup`, which connects it to Claude Desktop, Claude Code, Cursor, VS Code, Windsurf, Codex, Gemini CLI, LM Studio and Zed. Restart the Claude app if it was open, then ask: "Plot the odds of a recession against the S&P 500 and oil this year."

**As a plugin** (macOS and Linux; adds the server plus a skill that tells the model which tool fits which question):

```bash
claude plugin marketplace add efoltyn/market-search && claude plugin install market-search@market-search   # Claude Code
codex plugin marketplace add efoltyn/market-search && codex plugin add market-search@market-search         # Codex
gemini extensions install https://github.com/efoltyn/market-search                                         # Gemini CLI
```

**Claude app only:** open [market-search.mcpb](https://github.com/efoltyn/market-search/releases/latest/download/market-search.mcpb) to install it as a Claude Desktop extension (Mac and Windows).

**Or let your AI set it up.** Paste this into Claude Code, Codex or another coding agent:

> Set up market-search for me: follow https://eliterminal.com/setup

**From source** (Rust):

```bash
cargo install --git https://github.com/efoltyn/market-search market-search
```

The source is open Rust, so an agent can also read it, change tools, and add providers.

---

## Connect it

`market-search setup` lists the AI apps it found, asks once, and adds a `market-search` entry to each. Nothing else in your configs changes, and each file gets a backup before its first edit. `--dry-run` shows the changes first, `--list` shows what is connected, and `--remove` takes it out again.

Or do it by hand with the binary's full path (`which market-search`); apps like Claude Desktop don't see your shell's PATH. One line in Claude Code:

```bash
claude mcp add --scope user market-search -- /full/path/to/market-search mcp
```

Or add it to your client config (`.mcp.json`, Claude Desktop, Cursor, Gemini CLI):

```json
{ "mcpServers": { "market-search": { "command": "/full/path/to/market-search", "args": ["mcp"] } } }
```

**Self-host** (optional): the server is stateless, so serving it from your own machine is one command, `market-search mcp --http`, behind your own TLS. `market-search mcp --check` shows which tools your keys unlock. See [SELFHOST.md](SELFHOST.md).

---

## Use it from your phone

claude.ai, on the web and in the phone app, reaches tools through a public HTTPS link. On a Mac, click **Set up phone access** in the Market Search app. Or run one command on a Mac or Linux computer that stays on:

```bash
market-search mcp service install
```

It runs market-search in the background behind a free ngrok tunnel, and starts it again after a reboot. The first time, it opens the ngrok page where you copy your free authtoken. Then it prints your link and copies it:

```
https://your-name.ngrok-free.dev/mcp/<token>
```

Add it to claude.ai once:

1. Open [Settings → Connectors](https://claude.ai/settings/connectors).
2. Click **Add custom connector**.
3. Paste the link and click **Add**.

It shows up in the Claude app on your phone too. The link has a secret token in it, so keep it private.

- `market-search mcp service status` checks on it.
- `market-search mcp service url` prints the link again.
- `market-search mcp service uninstall` stops it.

Just trying it? `market-search mcp share` prints a temporary link that works while that command runs.

---

## Tools

### Market data

| Tool | What it does | Example |
|------|-------------|---------|
| `finance_timeseries` | OHLCV on one axis across stocks, FRED macro, futures, crypto, and prediction markets, auto-routed by ticker. 20 presets pull a whole domain in one call. | `--preset macro --tickers NVDA,SMH --range 90d` |
| `finance_movers` | Largest day gainers / losers / most-active, or screen a list by % move, price, volume, or market cap | `--direction gainers --min-cap 10B` |
| `finance_options` | Full options chain: IV, max pain, put/call ratio, skew. `--all` for every expiration | `--ticker SPY --all` |
| `finance_fundamentals` | Income statement, P/E, margins, ROE, debt/equity | `--ticker NVDA` |
| `finance_curve` | Futures forward curve: every contract month, front/back spread. Oil, brent, gold, silver, natgas, copper, more | `--commodity wti` |
| `finance_search` | Ticker lookup + FRED macro series discovery | `--query "semiconductor"` |
| `finance_insider` | Insider buys and sells parsed from SEC Form 4: who, role, shares, price, net summary | `--ticker NVDA --days 90` |
| `finance_short` | FINRA short interest (days-to-cover, change vs prior print) + daily per-venue short volume | `--ticker GME` |
| `finance_bonds` | A company's outstanding bonds from FINRA TRACE: CUSIP, coupon, maturity, last price and yield. Feed a CUSIP to timeseries as `BOND:<CUSIP>:YLD` | `--issuer "ORACLE CORP"` |
| `finance_log` | Local, hash-chained trail of every tool call with archived responses; verify or export it | `--stats` |

### Prediction markets

| Tool | What it does | Example |
|------|-------------|---------|
| `finance_odds` | Search Kalshi + Polymarket at once: probabilities, volume, both venues merged | `--search "taiwan" --live` |
| `finance_rate_path` | Per-meeting Fed hold / cut / hike probabilities from live Kalshi markets | (no args) |

### Macro & government

| Tool | What it does | Example |
|------|-------------|---------|
| `finance_schedule` | Earnings calendar + macro release dates | `--kind macro --from 2026-04-01 --to 2026-04-30 --major` |
| `finance_cot` | CFTC Commitments of Traders: speculator vs commercial positioning | `--query crude --weeks 12` |
| `finance_auctions` | US Treasury auctions: bid-to-cover, high yield, bidder breakdown | `--security-type note` |
| `finance_nyfed` | NY Fed: SOFR, EFFR, reverse repo, SOMA holdings, dealer positions | `--kind rates` |
| `finance_fiscal` | National debt, Treasury cash balance, average interest rates | `--kind debt` |
| `finance_stress` | OFR Financial Stress Index, decomposed into credit / equity / funding / vol | `--range 90` |
| `finance_volsurface` | CBOE vol indices: VIX, VIX9D, VIX3M, VIX6M, VIX1Y, VVIX, OVX, GVZ, SKEW | `--symbols VIX,VVIX,SKEW` |
| `finance_filings` | SEC filings by ticker. `--toc` lists every section with its size; `--section` returns exactly the ones you name (cash flow, MD&A, risk factors, any note); `--xbrl` gives the reported figures with quarter-over-quarter and year-over-year changes | `--ticker AMZN --forms 10-Q --section cash_flow,note:debt` |

### Central banks

| Tool | What it does | Example |
|------|-------------|---------|
| `finance_ecb` | EUR rates, Euro STR, M3, EURIBOR, euro yield curve, balance sheet | `--preset yield_curve` |
| `finance_bis` | Global policy rates, total assets, credit-to-GDP gaps | `--preset policy_rates --countries US,XM,JP,GB` |
| `finance_boj` | Monetary base, TANKAN, BOJ balance sheet, call rate, USD/JPY | `--preset tankan` |
| `finance_boe` | Bank Rate, SONIA, gilt yields, M4, GBP rates | `--preset all` |
| `finance_eia` | US crude, gasoline, distillate, natural gas storage | `--preset crude` |

---

## Timeseries provider routing

`finance_timeseries` picks the source from the ticker:

| Ticker | Provider | Coverage |
|--------|----------|----------|
| `AAPL`, `SPY`, `CL=F`, `BTC-USD` | Yahoo Finance | Stocks, ETFs, futures, crypto, FX |
| `UNRATE`, `DGS10`, `CPIAUCSL` | FRED | 800,000+ macro series, no key |
| `PYTH:BTC`, `PYTH:OIL` | Pyth Network | 24/7 crypto & commodity oracles |
| `BN:BTCUSDT`, `BN:BTC`, `BN:BTCUSD` | Binance (plain-USD pairs: Binance.US) | Crypto OHLCV since 2017 (Binance.US since 2019) |
| `KXRECSSNBER-26` | Kalshi | Probability + volume candles |
| `609655` | Polymarket | Probability + volume candles |
| `GOVT:US10Y`, `GOVT:DE10Y`, `GOVT:JP30Y` | CNBC | Government bond yields by country and tenor, daily back to 2000 |
| `BOND:21873SAG3:YLD` | FINRA TRACE | Corporate bond yield or price by CUSIP |
| `NAV:OBDC`, `PNAV:FSK` | SEC XBRL | Net asset value per share, and price over NAV |
| `NYFED:SOFR_P99`, `NYFED:SRF` | NY Fed | Repo rates, standing repo facility, reverse repo |
| `OFR:FSI` | OFR | Financial stress index and repo market volumes |
| `BUYBACK:20-30Y:COVER` | US Treasury | Treasury buyback operations |
| `ETF:JNK:FLOW` | Fund issuers | ETF flows, shares outstanding, NAV |
| `CBOE:VXCONTANGO`, `CBOE:PCR_EQUITY` | CBOE | VIX futures curve, put/call ratios |
| `HL:BTC:FUNDING_APR` | Hyperliquid | Perpetual funding and open interest |
| `DERIBIT:DVOL:BTC` | Deribit | Crypto implied volatility, funding, skew |

Mix them in one call:

```bash
market-search finance timeseries --tickers SPY,DGS10,BN:BTC,KXRECSSNBER-26,609655 --range 30d
```

---

## Data sources

Core tools need no API keys.

| Provider | Key |
|----------|-----|
| Yahoo Finance | None |
| Kalshi | None |
| Polymarket | None |
| FRED | None |
| CFTC | None |
| SEC EDGAR | None (set `ELI_SEC_USER_AGENT` to a contact, e.g. `"Jane Doe jane@example.com"`) |
| FINRA | None |
| NY Fed | None |
| US Treasury | None |
| OFR | None |
| CBOE | None |
| Pyth Network | None |
| Binance · Hyperliquid · Deribit | None |
| CNBC | None |
| ECB · BIS · BOJ · BOE | None |
| EIA | Free key ([register](https://www.eia.gov/opendata/register.php)) |

---

## Feedback

Agents use Market Search more than people do, so the MCP server has one extra tool, `report_issue`. When a tool errors, returns a wrong or stale number, or can't do what was needed, the agent can send a short note to the developer through [tell.dev](https://tell.dev). It is told to leave out your private data and to tell you when it has sent one.

People can use [this form](https://tell.dev/f/tell_pk_sndQpXc7X_l7y9gOQNEZDocO) or the CLI:

```bash
market-search feedback "finance_curve CL settles look a day old" --kind bug
```

Add `--contact you@example.com` if you want an answer, or `--dry-run` to check a note without sending it.

Nothing is sent unless an agent calls the tool or you run the command. A note carries its text, the Market Search version, your OS and CPU type, the name of the AI app that sent it, and the tool name, arguments and error if the sender included them. A copy goes in the local activity trail (`finance_log`), so you can read exactly what left your machine. The key in the binary is a public tell.dev key that can only add notes to this inbox. Set `MARKET_SEARCH_FEEDBACK=off` in the server's environment to remove the tool and turn off the command.

---

Runs on your own machine and IP, making the same requests your browser would, at human rates. That is why it is a local binary and not a hosted service.

Market Search fetches data for you; it does not license or redistribute it. Government and central-bank data (SEC, Treasury, NY Fed, OFR, CFTC, EIA, ECB, BIS, BOJ, BOE) is public. Other sources (Yahoo Finance, Kalshi, Polymarket, FRED, CBOE, Binance and others) publish terms that limit use to personal, non-commercial purposes; you are responsible for using their data within those terms. Market Search is not affiliated with any of them, and nothing it returns is investment advice. It collects nothing about you; the only thing that reaches the developer is feedback you or your agent choose to send. See [PRIVACY.md](PRIVACY.md).

[eliterminal.com](https://eliterminal.com) · Licensed under [AGPL-3.0](https://www.gnu.org/licenses/agpl-3.0.html).
