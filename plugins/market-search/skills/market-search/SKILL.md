---
name: market-search
description: Use when the user asks about markets, the economy or the odds of an event - stock, ETF, futures, FX or crypto prices; recession, Fed, election or geopolitical odds on Kalshi and Polymarket; Fed rate-cut probabilities; macro data like CPI, jobs, yields or credit spreads; options, fundamentals, insider trades, short interest or SEC filings; Treasury auctions, COT positioning, or ECB/BOJ/BOE/BIS data. Pulls live structured data with the market-search tools instead of summarizing news articles.
---

# Market Search

The market-search tools return live, structured market data. Prefer them over web search whenever the question has a number in its answer.

## Pick the tool

- A price, rate or probability over time: `finance_timeseries`. It puts stocks, FRED macro, futures, crypto and prediction-market odds on one time axis, so mix them in one call (`tickers: "SPY,CL=F,DGS10,KXRECSSNBER-26"`). Reach for a `preset` (`macro`, `recession`, `credit`, `yield_curve`, `inflation`, `liquidity`, `sovereigns`, ...) before hand-listing tickers.
- A price while US markets are closed (nights, weekends): `KF:` tickers in `finance_timeseries` (`KF:BTC`, `KF:GOLD`, `KF:OIL`, `KF:US100`). They are Kraken Futures index candles and keep printing all week.
- "What are the odds of X": `finance_odds` with a short `search` term. It searches Kalshi and Polymarket together. Feed a market's `ticker` from the result into `finance_timeseries` to chart how the odds moved.
- Fed meetings: `finance_rate_path` (no arguments) for hold/cut/hike odds per meeting.
- A company: `finance_fundamentals`, `finance_options`, `finance_insider`, `finance_short`, `finance_bonds`, and `finance_filings` (use `toc` first, then `section` to read only the parts you need).
- Don't know the ticker or FRED series id: `finance_search`.
- Calendar: `finance_schedule` for earnings and macro release dates.
- Plumbing and positioning: `finance_auctions`, `finance_nyfed`, `finance_fiscal`, `finance_stress`, `finance_cot`, `finance_volsurface`, `finance_curve`.
- Other central banks and energy: `finance_ecb`, `finance_boj`, `finance_boe`, `finance_bis`, `finance_eia`.

## Answer well

- Quote the number, its source and its timestamp. Check each candle's `t` and the `last_at` stat for staleness before calling something "current".
- When two series answer the question together (odds vs. the asset that should react), fetch them in one `finance_timeseries` call and compare on the shared axis.
- Prediction-market odds are market prices, not forecasts from an authority. Say which venue and the volume behind them.
- This is data, not investment advice. Don't tell the user to buy or sell.

## When a tool lets you down

If a market-search tool errors, returns a number you have reason to believe is wrong or stale, or cannot do what the user needed, call `report_issue` once. Say which tool, the arguments, what you expected and what came back. Leave out the user's private data, positions and credentials. It goes to the developer, who fixes the tools from these notes. Tell the user in one line that you reported it, then answer as well as you can with what you have.
