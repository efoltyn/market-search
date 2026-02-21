// MCP (Model Context Protocol) stdio server.
// Exposes eli finance and web tools as native Claude Code tools via JSON-RPC 2.0.
// Usage: eli mcp   ← Claude Code connects automatically via .mcp.json

async fn cmd_mcp() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = tokio::io::BufReader::new(stdin);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    while let Some(request) = mcp_read_request(&mut reader).await? {

        // Notifications have no id and require no response.
        let method = match request.get("method").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None => continue,
        };
        if method.starts_with("notifications/") {
            continue;
        }

        let id = request.get("id").cloned().unwrap_or(serde_json::Value::Null);

        let response = match method.as_str() {
            "initialize" => mcp_initialize(id),
            "tools/list" => mcp_tools_list(id),
            "tools/call" => mcp_tools_call(id, &request).await,
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Method not found" }
            }),
        };

        mcp_write_response(&mut out, &response)?;
    }

    Ok(())
}

async fn mcp_read_request<R>(reader: &mut R) -> Result<Option<serde_json::Value>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

    let mut content_length: Option<usize> = None;
    let mut line = Vec::new();

    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).await?;
        if n == 0 {
            return Ok(None);
        }

        if line == b"\r\n" || line == b"\n" {
            break;
        }

        let header = std::str::from_utf8(&line)
            .context("invalid mcp header utf8")?
            .trim();
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                let len = value
                    .trim()
                    .parse::<usize>()
                    .context("invalid content-length")?;
                content_length = Some(len);
            }
        }
    }

    let len = content_length.context("missing content-length header")?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    let request = serde_json::from_slice(&payload).context("parse mcp request json")?;
    Ok(Some(request))
}

fn mcp_write_response<W: std::io::Write>(
    out: &mut W,
    response: &serde_json::Value,
) -> Result<()> {
    let body = serde_json::to_vec(response).context("serialize response")?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

fn mcp_initialize(id: serde_json::Value) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "eli", "version": "0.1.0" }
        }
    })
}

fn mcp_tools_list(id: serde_json::Value) -> serde_json::Value {
    let tools = json!([
        {
            "name": "finance_macro",
            "description": "Fetch 31 FRED macro indicators in parallel: CPI, Core CPI, Core PCE, PPI, unemployment, payrolls, jobless claims, JOLTS, real GDP, industrial production, Fed funds rate, 2Y/10Y/30Y Treasury yields, TIPS real yield, mortgage rate, debt-to-GDP, total federal debt, M2, Fed balance sheet, consumer sentiment (UMich), retail sales, savings rate, Case-Shiller, housing starts, vehicle sales, HY credit spread, WTI oil, trade-weighted dollar index.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "range": {
                        "type": "string",
                        "description": "Lookback range for YoY changes (default: 1y)"
                    }
                }
            }
        },
        {
            "name": "finance_snapshot",
            "description": "Point-in-time market snapshot: price, market cap, shares outstanding, daily/trailing returns, relative strength. Works for stocks and ETFs (SPY, QQQ, GLD, etc.).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tickers": {
                        "type": "string",
                        "description": "Comma-separated ticker symbols (e.g. NVDA,AAPL,SPY,GLD)"
                    }
                },
                "required": ["tickers"]
            }
        },
        {
            "name": "finance_timeseries",
            "description": "OHLCV time series for stocks (Yahoo Finance) or FRED macro series (e.g. UNRATE, T10Y2Y, GFDEGDQ188S). Auto-detects provider — mix stocks and FRED in one call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tickers": {
                        "type": "string",
                        "description": "Comma-separated ticker symbols or FRED series IDs"
                    },
                    "range": {
                        "type": "string",
                        "description": "Lookback range: 1d, 5d, 1mo, 3mo, 6mo, 1y, 2y, 5y (default: 1y)"
                    }
                },
                "required": ["tickers"]
            }
        },
        {
            "name": "finance_yield_curve",
            "description": "US Treasury yield curve (1mo through 30y) with key spread calculations: 2s10s, 3mo10y. Optionally compare to prior periods.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "compare": {
                        "type": "string",
                        "description": "Comma-separated comparison windows: 3mo,1y"
                    }
                }
            }
        },
        {
            "name": "finance_rate_path",
            "description": "Implied Fed policy trajectory from Kalshi prediction market cache. Returns hold/cut/hike probabilities and implied rate per FOMC meeting through 2028.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "finance_odds",
            "description": "Search prediction markets (Kalshi + Polymarket) by keyword. Returns live bid/ask prices, probabilities, and volume. Use for recession odds, election odds, Fed decisions, tariffs, any macro event.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "search": {
                        "type": "string",
                        "description": "Search query (e.g. recession, fed rate, tariff, bitcoin, election)"
                    },
                    "live": {
                        "type": "boolean",
                        "description": "Fetch fresh prices from exchange APIs (default: true)"
                    }
                },
                "required": ["search"]
            }
        },
        {
            "name": "finance_options",
            "description": "Options chain with IV, put/call ratio, max pain, and vol skew for a ticker. Defaults to summary=true and near_money=10 (±10% strikes). Pass near_money=100 for the full chain — large outputs auto-save to file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticker": {
                        "type": "string",
                        "description": "Underlying ticker (e.g. SPY, NVDA, AAPL, QQQ)"
                    },
                    "summary": {
                        "type": "boolean",
                        "description": "Return summary metrics only, no full chain (default: true)"
                    },
                    "near_money": {
                        "type": "number",
                        "description": "Only return strikes within this % of the underlying (e.g. 5)"
                    }
                },
                "required": ["ticker"]
            }
        },
        {
            "name": "finance_news",
            "description": "Fetch news headlines for a ticker on a specific date. Direct API call, no websearch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticker": {
                        "type": "string",
                        "description": "Ticker symbol (e.g. NVDA, AAPL, MSFT)"
                    },
                    "date": {
                        "type": "string",
                        "description": "Date in YYYY-MM-DD format"
                    }
                },
                "required": ["ticker", "date"]
            }
        },
        {
            "name": "finance_prices",
            "description": "Latest spot prices from Pyth Hermes for crypto, commodities, FX, and rates (BTC, ETH, SOL, gold, silver, oil, etc.). With no filter returns all 500+ Pyth feeds (auto-saved to file). Use query or asset_type for inline results. If query is ambiguous, returns disambiguation candidates — use exact symbol (e.g. 'Crypto.BTC/USD') on retry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Filter by asset name (e.g. BTC/USD, gold, ethereum, solana, EUR/USD)"
                    },
                    "asset_type": {
                        "type": "string",
                        "description": "Filter by asset type: crypto, equity, fx, metal, rates"
                    }
                }
            }
        },
        {
            "name": "finance_fundamentals",
            "description": "Quarterly financial statements: income statement, balance sheet, cash flow. Not for ETFs — use finance_snapshot instead. Accepts multiple comma-separated tickers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticker": {
                        "type": "string",
                        "description": "Stock ticker(s), comma-separated (e.g. NVDA or NVDA,AAPL,MSFT) — not for ETFs"
                    }
                },
                "required": ["ticker"]
            }
        },
        {
            "name": "finance_sync",
            "description": "Bulk-sync all Kalshi + Polymarket prediction markets (~22,500) to local CSV cache. Takes ~10 seconds. Run once to enable fast finance_odds searches.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "web_search",
            "description": "Search the web using DuckDuckGo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "web_read",
            "description": "Fetch and extract content from a single URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to fetch and extract content from"
                    }
                },
                "required": ["url"]
            }
        },
        {
            "name": "web_crawl",
            "description": "Crawl a website and extract content from all discovered pages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to start crawling from"
                    },
                    "max_pages": {
                        "type": "integer",
                        "description": "Maximum pages to crawl (default: 50)"
                    },
                    "smart": {
                        "type": "boolean",
                        "description": "HTTP first, render JS only when needed"
                    },
                    "sitemap": {
                        "type": "boolean",
                        "description": "Discover pages via sitemap.xml"
                    }
                },
                "required": ["url"]
            }
        },
        {
            "name": "web_extract",
            "description": "Extract key facts from a URL, local file, or inline text. Returns concise bullet points.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to fetch and extract from"
                    },
                    "text": {
                        "type": "string",
                        "description": "Inline text to extract from"
                    },
                    "bullets": {
                        "type": "integer",
                        "description": "Number of bullet points to extract (default: 10)"
                    },
                    "focus": {
                        "type": "string",
                        "description": "Focus extraction on a specific topic"
                    }
                }
            }
        },
        {
            "name": "finance_search",
            "description": "Search for ticker symbols or FRED macro series IDs by name (e.g. 'tesla', 'unemployment', 'S&P 500').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query (e.g. tesla, unemployment, S&P 500)"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "finance_filings",
            "description": "Fetch recent SEC filings (8-K, 10-K, 10-Q) for a ticker. Can download and inline document text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticker": {
                        "type": "string",
                        "description": "Stock ticker (e.g. TSLA, NVDA, AAPL)"
                    },
                    "forms": {
                        "type": "string",
                        "description": "Comma-separated form types to filter (e.g. 10-K,10-Q). Default: 8-K,10-K,10-Q"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of filings to return (default: 5)"
                    },
                    "include_text": {
                        "type": "boolean",
                        "description": "Download primary documents and include text excerpt inline"
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Max chars for inline excerpt when include_text is true"
                    }
                },
                "required": ["ticker"]
            }
        },
        {
            "name": "finance_schedule",
            "description": "Earnings calendar and macro release schedule (FRED). Shows upcoming CPI, PCE, GDP, jobs, FOMC, and earnings dates. Defaults to macro-only for the next 30 days. Use kind='all' or kind='earnings' for full breadth — large outputs auto-save to file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": "Schedule type: all | earnings | macro (default: macro)"
                    },
                    "from": {
                        "type": "string",
                        "description": "Start date YYYY-MM-DD"
                    },
                    "to": {
                        "type": "string",
                        "description": "End date YYYY-MM-DD"
                    },
                    "date": {
                        "type": "string",
                        "description": "Single date YYYY-MM-DD (overrides from/to)"
                    },
                    "major": {
                        "type": "boolean",
                        "description": "Macro only: keep just major US releases (CPI, PCE, GDP, jobs, FOMC, claims)"
                    }
                }
            }
        },
        {
            "name": "finance_dashboard",
            "description": "Run a preset aggregate tool that combines multiple data sources in one call. Presets: 'recession' (macro + yield curve + rate path + SPY options + odds), 'tech_megacap' (snapshot of NVDA/AAPL/MSFT/GOOGL/META/AMZN/TSLA/semis + AI/tariff odds). New presets can be added by Claude Code without changing response types.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "preset": {
                        "type": "string",
                        "description": "Preset name: recession | tech_megacap"
                    }
                },
                "required": ["preset"]
            }
        },
        {
            "name": "code_analyze",
            "description": "Analyze Rust source code. Three modes: (1) default — structural map of a single file (function signatures with full types, struct fields, impl methods, enums); (2) --pub_api — complete public API surface of a directory (every pub fn/struct/enum/trait/impl grouped by file, ideal before writing new tools); (3) --find — multi-symbol search across all .rs files using aho-corasick (returns every line containing any symbol with file path + line number).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to a Rust source file or directory (e.g. eli/crates/eli-core/src/finance/)"
                    },
                    "pub_api": {
                        "type": "boolean",
                        "description": "Emit full public API surface grouped by file (best for directories)"
                    },
                    "find": {
                        "type": "string",
                        "description": "Comma-separated symbols to search for across all .rs files (e.g. fetch_snapshot,SnapshotRequest)"
                    },
                    "include_files": {
                        "type": "boolean",
                        "description": "Include per-file metrics in directory hotspot mode"
                    },
                    "top": {
                        "type": "integer",
                        "description": "Rows per hotspot ranking in directory mode (default: 20)"
                    }
                },
                "required": ["path"]
            }
        },
        {
            "name": "agent_run",
            "description": "Run a single autonomous Eli research worker from a natural-language task. The worker iterates, fetches data, writes code, and synthesizes a report.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Natural-language task for the worker (e.g. 'Analyze AMD vs INTC correlation over 30 days')"
                    },
                    "max_ms": {
                        "type": "integer",
                        "description": "Max runtime budget in milliseconds (default: 45000)"
                    }
                },
                "required": ["task"]
            }
        }
    ]);

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "tools": tools }
    })
}

async fn mcp_tools_call(id: serde_json::Value, request: &serde_json::Value) -> serde_json::Value {
    let params = match request.get("params") {
        Some(p) => p,
        None => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": "Missing params" }
            })
        }
    };

    let tool_name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": "Missing tool name" }
            })
        }
    };

    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

    let cli_args = match mcp_build_cli_args(&tool_name, &args) {
        Ok(a) => a,
        Err(e) => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": format!("Invalid arguments: {e}") }
            })
        }
    };

    match mcp_run_subprocess(cli_args).await {
        Ok(output) => {
            // Smart context management: large outputs auto-save to file.
            // This protects context WITHOUT removing breadth — the full data is always
            // accessible at the returned path for file reads, Python analysis, etc.
            const MAX_INLINE_CHARS: usize = 50_000;
            let response_text = if output.len() > MAX_INLINE_CHARS {
                let ts = chrono::Utc::now().timestamp_millis();
                let path = format!("/tmp/eli_{tool_name}_{ts}.json");
                let saved = std::fs::write(&path, &output).is_ok();
                let preview = &output[..500.min(output.len())];
                // Return compact summary + file pointer so agent can cat/read/analyze
                if saved {
                    format!(
                        "{{\
                        \"_mcp_note\":\"Output ({chars} chars) exceeds inline limit. Full data saved to file — use Read, Bash cat, or Python to process it.\",\
                        \"_saved_to\":\"{path}\",\
                        \"_char_count\":{chars},\
                        \"_tool\":\"{tool_name}\",\
                        \"_preview\":{preview_json}\
                        }}",
                        chars = output.len(),
                        path = path,
                        tool_name = tool_name,
                        preview_json = serde_json::to_string(preview)
                            .unwrap_or_else(|_| "\"\"".to_string()),
                    )
                } else {
                    // Fallback: truncate if write failed
                    format!(
                        "{{\
                        \"_mcp_note\":\"Output ({chars} chars) exceeds inline limit and could not be saved. Showing truncated preview.\",\
                        \"_char_count\":{chars},\
                        \"_tool\":\"{tool_name}\",\
                        \"_preview\":{preview_json}\
                        }}",
                        chars = output.len(),
                        tool_name = tool_name,
                        preview_json = serde_json::to_string(preview)
                            .unwrap_or_else(|_| "\"\"".to_string()),
                    )
                }
            } else {
                output
            };
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": response_text }]
                }
            })
        }
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": format!("Tool execution failed: {e}") }
        }),
    }
}

fn mcp_build_cli_args(
    tool: &str,
    args: &serde_json::Value,
) -> anyhow::Result<Vec<String>> {
    let s = |v: &str| v.to_string();
    match tool {
        "finance_macro" => {
            let mut v = vec![s("finance"), s("macro")];
            if let Some(range) = args.get("range").and_then(|r| r.as_str()) {
                v.extend([s("--range"), s(range)]);
            }
            Ok(v)
        }
        "finance_snapshot" => {
            let tickers = args
                .get("tickers")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("tickers required"))?;
            Ok(vec![s("finance"), s("snapshot"), s("--tickers"), s(tickers)])
        }
        "finance_timeseries" => {
            let tickers = args
                .get("tickers")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("tickers required"))?;
            let mut v = vec![s("finance"), s("timeseries"), s("--tickers"), s(tickers)];
            if let Some(range) = args.get("range").and_then(|r| r.as_str()) {
                v.extend([s("--range"), s(range)]);
            }
            Ok(v)
        }
        "finance_yield_curve" => {
            let mut v = vec![s("finance"), s("yield-curve")];
            if let Some(compare) = args.get("compare").and_then(|c| c.as_str()) {
                v.extend([s("--compare"), s(compare)]);
            }
            Ok(v)
        }
        "finance_rate_path" => Ok(vec![s("finance"), s("rate-path")]),
        "finance_odds" => {
            let search = args
                .get("search")
                .and_then(|s| s.as_str())
                .ok_or_else(|| anyhow::anyhow!("search required"))?;
            let mut v = vec![s("finance"), s("odds"), s("--search"), s(search)];
            let live = args.get("live").and_then(|l| l.as_bool()).unwrap_or(true);
            if live {
                v.push(s("--live"));
            }
            Ok(v)
        }
        "finance_options" => {
            let ticker = args
                .get("ticker")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("ticker required"))?;
            let mut v = vec![s("finance"), s("options"), s("--ticker"), s(ticker)];
            let summary = args.get("summary").and_then(|b| b.as_bool()).unwrap_or(true);
            if summary {
                v.push(s("--summary"));
            }
            if let Some(nm) = args.get("near_money").and_then(|n| n.as_f64()) {
                v.extend([s("--near-money"), nm.to_string()]);
            } else {
                // Default near-money to 10% to prevent oversized chain output
                v.extend([s("--near-money"), s("10")]);
            }
            Ok(v)
        }
        "finance_news" => {
            let ticker = args
                .get("ticker")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("ticker required"))?;
            let date = args
                .get("date")
                .and_then(|d| d.as_str())
                .ok_or_else(|| anyhow::anyhow!("date required"))?;
            Ok(vec![
                s("finance"),
                s("news"),
                s("--ticker"),
                s(ticker),
                s("--date"),
                s(date),
            ])
        }
        "finance_prices" => {
            let mut v = vec![s("finance"), s("prices")];
            if let Some(query) = args.get("query").and_then(|q| q.as_str()) {
                v.extend([s("--query"), s(query)]);
            } else if let Some(at) = args.get("asset_type").and_then(|a| a.as_str()) {
                v.extend([s("--asset-type"), s(at)]);
            }
            // No filter = all feeds (500+). Output auto-saves to file if >50K chars.
            Ok(v)
        }
        "finance_fundamentals" => {
            let tickers = args
                .get("ticker")
                .or_else(|| args.get("tickers"))
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("ticker required"))?;
            Ok(vec![
                s("finance"),
                s("fundamentals"),
                s("--tickers"),
                s(tickers),
            ])
        }
        "finance_sync" => Ok(vec![s("finance"), s("sync")]),
        "web_search" => {
            let query = args
                .get("query")
                .and_then(|q| q.as_str())
                .ok_or_else(|| anyhow::anyhow!("query required"))?;
            Ok(vec![s("web"), s("search"), s("--query"), s(query)])
        }
        "web_read" => {
            let url = args
                .get("url")
                .and_then(|u| u.as_str())
                .ok_or_else(|| anyhow::anyhow!("url required"))?;
            Ok(vec![s("web"), s("read"), s("--url"), s(url)])
        }
        "web_crawl" => {
            let url = args
                .get("url")
                .and_then(|u| u.as_str())
                .ok_or_else(|| anyhow::anyhow!("url required"))?;
            let mut v = vec![s("web"), s("crawl"), s("--url"), s(url)];
            if let Some(mp) = args.get("max_pages").and_then(|n| n.as_u64()) {
                v.extend([s("--max-pages"), mp.to_string()]);
            }
            if args.get("smart").and_then(|b| b.as_bool()).unwrap_or(false) {
                v.push(s("--smart"));
            }
            if args.get("sitemap").and_then(|b| b.as_bool()).unwrap_or(false) {
                v.push(s("--sitemap"));
            }
            Ok(v)
        }
        "web_extract" => {
            let mut v = vec![s("web"), s("extract")];
            if let Some(url) = args.get("url").and_then(|u| u.as_str()) {
                v.extend([s("--url"), s(url)]);
            } else if let Some(text) = args.get("text").and_then(|t| t.as_str()) {
                v.extend([s("--text"), s(text)]);
            } else {
                return Err(anyhow::anyhow!("url or text required"));
            }
            if let Some(b) = args.get("bullets").and_then(|n| n.as_u64()) {
                v.extend([s("--bullets"), b.to_string()]);
            }
            if let Some(focus) = args.get("focus").and_then(|f| f.as_str()) {
                v.extend([s("--focus"), s(focus)]);
            }
            Ok(v)
        }
        "finance_search" => {
            let query = args
                .get("query")
                .and_then(|q| q.as_str())
                .ok_or_else(|| anyhow::anyhow!("query required"))?;
            Ok(vec![s("finance"), s("search"), s("--query"), s(query)])
        }
        "finance_filings" => {
            let ticker = args
                .get("ticker")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("ticker required"))?;
            let mut v = vec![s("finance"), s("filings"), s("--ticker"), s(ticker)];
            if let Some(forms) = args.get("forms").and_then(|f| f.as_str()) {
                v.extend([s("--forms"), s(forms)]);
            }
            if let Some(limit) = args.get("limit").and_then(|n| n.as_u64()) {
                v.extend([s("--limit"), limit.to_string()]);
            }
            if args.get("include_text").and_then(|b| b.as_bool()).unwrap_or(false) {
                v.push(s("--include-text"));
                if let Some(mc) = args.get("max_chars").and_then(|n| n.as_u64()) {
                    v.extend([s("--max-chars"), mc.to_string()]);
                }
            }
            Ok(v)
        }
        "finance_schedule" => {
            let mut v = vec![s("finance"), s("schedule")];
            // Default kind=macro to avoid returning 1000+ earnings rows
            let kind = args
                .get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or("macro");
            v.extend([s("--kind"), s(kind)]);
            if let Some(date) = args.get("date").and_then(|d| d.as_str()) {
                v.extend([s("--date"), s(date)]);
            } else {
                let from = args
                    .get("from")
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
                let to = args
                    .get("to")
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        (chrono::Local::now() + chrono::Duration::days(30))
                            .format("%Y-%m-%d")
                            .to_string()
                    });
                v.extend([s("--from"), from, s("--to"), to]);
            }
            // Default major=true when kind is macro to keep output focused
            let major = args.get("major").and_then(|b| b.as_bool()).unwrap_or(
                kind == "macro" || kind == "all",
            );
            if major {
                v.push(s("--major"));
            }
            Ok(v)
        }
        "finance_dashboard" => {
            let preset = args
                .get("preset")
                .and_then(|p| p.as_str())
                .ok_or_else(|| anyhow::anyhow!("preset required"))?;
            Ok(vec![s("finance"), s("dashboard"), s("--preset"), s(preset)])
        }
        "code_analyze" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| anyhow::anyhow!("path required"))?;
            let mut v = vec![s("code"), s(path)];
            if args.get("pub_api").and_then(|b| b.as_bool()).unwrap_or(false) {
                v.push(s("--pub-api"));
            }
            if let Some(find) = args.get("find").and_then(|f| f.as_str()) {
                v.extend([s("--find"), s(find)]);
            }
            if args.get("include_files").and_then(|b| b.as_bool()).unwrap_or(false) {
                v.push(s("--include-files"));
            }
            if let Some(top) = args.get("top").and_then(|n| n.as_u64()) {
                v.extend([s("--top"), top.to_string()]);
            }
            Ok(v)
        }
        "agent_run" => {
            let task = args
                .get("task")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("task required"))?;
            let mut v = vec![s("agent"), s("run"), s("--task"), s(task)];
            if let Some(ms) = args.get("max_ms").and_then(|n| n.as_u64()) {
                v.extend([s("--max-ms"), ms.to_string()]);
            }
            Ok(v)
        }
        _ => Err(anyhow::anyhow!("Unknown tool: {tool}")),
    }
}

async fn mcp_run_subprocess(args: Vec<String>) -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("get current exe path")?;
    let output = TokioCommand::new(&exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("spawn eli subprocess")?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    if !output.status.success() && stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "exit {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    Ok(stdout)
}
