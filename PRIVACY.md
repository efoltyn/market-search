# Privacy

Market Search (the `market-search` binary, its Claude/Codex/Gemini plugins and its Mac app) collects no data about you. Nothing reaches the developer unless you or your AI send feedback on purpose.

- It runs on your own computer. There is no Market Search server, account or analytics.
- When your AI calls a tool, the request goes from your machine directly to the public data source it names (for example Kalshi, Polymarket, Yahoo Finance, FRED, SEC EDGAR, the US Treasury, the NY Fed, the ECB). Those sources see an ordinary request from your IP address, under their own privacy policies.
- `finance_log` keeps a hash-chained record of your tool calls and responses in a local folder on your machine, so you can audit what your AI looked at. It never leaves your machine. Delete the folder to clear it.
- Optional settings you provide (an SEC contact string, an EIA API key) stay in your local config and are sent only to that source.
- The plugin launcher downloads the release binary from GitHub on first run and checks its SHA-256 before running it.
- Feedback is sent only on purpose: your AI calls the `report_issue` tool, or you run `market-search feedback` or fill in the form. The note goes to the developer's inbox on tell.dev, a service the same developer runs. It carries the text of the note, the Market Search version, your OS and CPU type, the name of the AI app, and the tool name, arguments and error if the sender included them. It carries a contact only if you add one with `--contact` or type one in the form. tell.dev sees the request's IP address like any web server and keeps request logs for 7 days ([its policy](https://tell.dev/privacy)). A copy of each note sent by the tool or the command is kept in `finance_log` on your machine. Set `MARKET_SEARCH_FEEDBACK=off` to remove the tool and turn off the command.
- If you choose to run `market-search mcp share` or `mcp service install`, your tool traffic goes through the tunnel provider you picked (Cloudflare, ngrok or tunnelmole) under their terms. This is off unless you turn it on.

Questions: open an issue at https://github.com/efoltyn/market-search/issues.
