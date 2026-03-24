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

        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

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
    use tokio::io::AsyncBufReadExt as _;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request = serde_json::from_str(trimmed).context("parse mcp request json")?;
        return Ok(Some(request));
    }
}

fn mcp_write_response<W: std::io::Write>(out: &mut W, response: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_string(response).context("serialize response")?;
    writeln!(out, "{body}")?;
    out.flush()?;
    Ok(())
}

fn mcp_initialize(id: serde_json::Value) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "eli", "version": "0.1.0" }
        }
    })
}

fn mcp_tools_list(id: serde_json::Value) -> serde_json::Value {
    let tools: serde_json::Value =
        serde_json::from_str(include_str!("mcp_tools.json")).expect("valid MCP tools catalog");

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

    // ── data_query: jq on cached /tmp/eli_* files (no subprocess needed) ──
    if tool_name == "data_query" {
        return mcp_data_query(id, &args).await;
    }

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
            // File-first, summary-back: ALWAYS save full output to file.
            // Return only a compact synthesis (500-2000 chars) + file path.
            // The AI reads the file only when it needs drill-down detail.
            //
            // Small tools (< 2K chars) are returned inline — they're already compact enough.
            // Everything else: save to file, build per-tool summary, return summary.
            const INLINE_THRESHOLD: usize = 2_000;

            let response_text = if output.len() <= INLINE_THRESHOLD {
                // Small outputs: return inline (yield_curve, rate_path, auctions, etc.)
                output
            } else {
                // Strip repeated metadata from individual items before saving.
                // Hoists one copy to top-level _meta, strips from array items.
                let cleaned = mcp_strip_metadata(&output);
                let save_data = cleaned.as_deref().unwrap_or(&output);

                // Save (cleaned) output to file
                let ts = chrono::Utc::now().timestamp_millis();
                let path = format!("/tmp/eli_{tool_name}_{ts}.json");
                let saved = std::fs::write(&path, save_data).is_ok();

                // Build per-tool compact summary
                let summary = mcp_build_summary(&tool_name, &output);

                if saved {
                    format!(
                        "{{\
                        \"_file\":\"{path}\",\
                        \"_chars\":{chars},\
                        {summary}\
                        }}",
                        path = path,
                        chars = save_data.len(),
                        summary = summary,
                    )
                } else {
                    // File save failed — return truncated inline
                    let truncated = &output[..4000.min(output.len())];
                    truncated.to_string()
                }
            };
            // Prepend wall-clock timestamp to every tool response so the AI
            // always knows the current date/time without a separate call.
            let now_local = chrono::Local::now();
            let response_text = format!(
                "[current_time: {}]\n{}",
                now_local.format("%A %B %-d, %Y %l:%M %p %Z"),
                response_text
            );
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

/// Handle data_query: run jq on a cached /tmp/eli_* file.
/// This is the "filing cabinet drawer pull" — lets the AI extract specific
/// slices from large cached outputs without loading the whole file into context.
async fn mcp_data_query(id: serde_json::Value, args: &serde_json::Value) -> serde_json::Value {
    let file = match args.get("file").and_then(|f| f.as_str()) {
        Some(f) => f,
        None => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": "file parameter required" }
            });
        }
    };

    // Security: only allow /tmp/eli_* files
    if !file.starts_with("/tmp/eli_") {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": "file must be a /tmp/eli_* path from a previous tool call" }
        });
    }

    if !std::path::Path::new(file).exists() {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": format!("File not found: {file}") }
        });
    }

    let jq_expr = match args.get("jq").and_then(|j| j.as_str()) {
        Some(j) => j,
        None => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": "jq expression required" }
            });
        }
    };

    // Run jq on the file
    let result = TokioCommand::new("jq")
        .arg("-c")
        .arg(jq_expr)
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32603, "message": format!("jq error: {}", stderr.trim()) }
                });
            }
            // Apply same inline threshold — if query result is still huge, save it
            const QUERY_THRESHOLD: usize = 4_000;
            let response_text = if stdout.len() <= QUERY_THRESHOLD {
                stdout
            } else {
                let ts = chrono::Utc::now().timestamp_millis();
                let out_path = format!("/tmp/eli_query_{ts}.json");
                let _ = std::fs::write(&out_path, &stdout);
                format!(
                    "{{\"_file\":\"{}\",\"_chars\":{},\"_note\":\"query result saved — read file for full data\"}}",
                    out_path, stdout.len()
                )
            };
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": response_text }]
                }
            })
        }
        Err(_) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": "jq not found — install with: brew install jq" }
        }),
    }
}

/// Strip repeated per-item metadata keys before saving to file.
/// Hoists first-seen freshness/run_meta to top-level _meta, removes from items.
/// Returns None if parsing fails or no stripping needed.
fn mcp_strip_metadata(output: &str) -> Option<String> {
    let mut root: serde_json::Value = serde_json::from_str(output).ok()?;

    // Keys to strip from individual array items
    const STRIP_KEYS: &[&str] = &[
        "freshness",
        "delta_context",
        "delta_since_last_sync",
        "run_meta",
        "collected_at",
        "transport_received",
        "transport_origin",
    ];

    // Find arrays in the JSON (markets, snapshots, indicators, positions, series, etc.)
    let array_keys: Vec<String> = root
        .as_object()?
        .iter()
        .filter(|(_, v)| v.is_array())
        .map(|(k, _)| k.clone())
        .collect();

    let mut stripped_any = false;
    let mut hoisted_meta = serde_json::Map::new();

    for key in &array_keys {
        if let Some(arr) = root.get_mut(key).and_then(|v| v.as_array_mut()) {
            for item in arr.iter_mut() {
                if let Some(obj) = item.as_object_mut() {
                    for &strip_key in STRIP_KEYS {
                        if let Some(removed) = obj.remove(strip_key) {
                            stripped_any = true;
                            // Hoist first-seen value to _meta
                            if !hoisted_meta.contains_key(strip_key) {
                                hoisted_meta.insert(strip_key.to_string(), removed);
                            }
                        }
                    }
                }
            }
        }
    }

    if !stripped_any {
        return None;
    }

    // Add hoisted metadata at top level
    if !hoisted_meta.is_empty() {
        if let Some(obj) = root.as_object_mut() {
            obj.insert(
                "_meta".to_string(),
                serde_json::Value::Object(hoisted_meta),
            );
        }
    }

    serde_json::to_string(&root).ok()
}

/// Compute per-ticker summary stats from a timeseries JSON response.
/// Returns a JSON string like `{"SPY":{"start":550.0,"end":530.0,...},...}` or None on parse failure.
fn mcp_timeseries_summary(output: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(output).ok()?;
    let series = v.get("series").and_then(|s| s.as_array())?;

    let mut map = serde_json::Map::new();
    for entry in series {
        let ticker = entry.get("ticker").and_then(|t| t.as_str())?;
        let candles = entry.get("candles").and_then(|c| c.as_array())?;
        if candles.is_empty() {
            continue;
        }
        let get_close = |c: &serde_json::Value| c.get("c").and_then(|x| x.as_f64());
        let start = get_close(candles.first()?)?;
        let end = get_close(candles.last()?)?;
        let return_pct = if start != 0.0 {
            (end / start - 1.0) * 100.0
        } else {
            0.0
        };
        let (high, low) = candles
            .iter()
            .fold((f64::NEG_INFINITY, f64::INFINITY), |(h, l), c| {
                let hi = c.get("h").and_then(|x| x.as_f64()).unwrap_or(h);
                let lo = c.get("l").and_then(|x| x.as_f64()).unwrap_or(l);
                (h.max(hi), l.min(lo))
            });
        // Annualised volatility — detect candle period from timestamps to use correct √N
        // √252 is only correct for daily candles; hourly needs √(252*6.5), etc.
        let ann_factor = if candles.len() >= 2 {
            let t0 = candles[0].get("t").and_then(|t| t.as_f64());
            let t1 = candles[1].get("t").and_then(|t| t.as_f64());
            if let (Some(t0), Some(t1)) = (t0, t1) {
                let diff = (t1 - t0).abs();
                // If timestamps look like milliseconds (>1e10), convert to seconds
                let diff_secs = if diff > 1e9 { diff / 1000.0 } else { diff };
                if diff_secs < 120.0        { 252.0 * 390.0 }  // 1-min (390 min/day)
                else if diff_secs < 600.0   { 252.0 * 78.0  }  // 5-min
                else if diff_secs < 1800.0  { 252.0 * 26.0  }  // 15-min
                else if diff_secs < 7200.0  { 252.0 * 6.5   }  // 1-hour
                else if diff_secs < 28800.0 { 252.0 * 2.0   }  // 4-hour
                else                        { 252.0          }  // daily or longer
            } else { 252.0 }
        } else { 252.0f64 };
        let closes: Vec<f64> = candles.iter().filter_map(get_close).collect();
        let vol_ann = if closes.len() >= 2 {
            let log_rets: Vec<f64> = closes
                .windows(2)
                .map(|w| (w[1] / w[0]).ln())
                .filter(|r| r.is_finite())
                .collect();
            if log_rets.len() >= 2 {
                let mean = log_rets.iter().sum::<f64>() / log_rets.len() as f64;
                let var = log_rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
                    / (log_rets.len() - 1) as f64;
                (var.sqrt() * ann_factor.sqrt() * 100.0 * 10.0).round() / 10.0
            } else {
                0.0
            }
        } else {
            0.0
        };
        map.insert(
            ticker.to_string(),
            serde_json::json!({
                "start": (start * 100.0).round() / 100.0,
                "end":   (end   * 100.0).round() / 100.0,
                "return_pct": (return_pct * 10.0).round() / 10.0,
                "high": (high * 100.0).round() / 100.0,
                "low":  (low  * 100.0).round() / 100.0,
                "vol_ann_pct": vol_ann,
                "n_candles": candles.len(),
            }),
        );
    }
    if map.is_empty() {
        None
    } else {
        serde_json::to_string(&serde_json::Value::Object(map)).ok()
    }
}

/// Build a compact summary string for the MCP response.
/// Each tool type extracts the 5-10 key numbers/facts the AI needs to reason.
/// The full data is always available at the file path.
/// Returns a JSON fragment (without outer braces) to be embedded in the response object.
fn mcp_build_summary(tool: &str, output: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(output) {
        Ok(v) => v,
        Err(_) => return format!("\"_summary\":\"parse error — read full file for data\""),
    };

    match tool {
        "finance_snapshot" => {
            // Extract: ticker, price, daily_return for each ticker
            let mut lines = Vec::new();
            if let Some(snaps) = v.get("snapshots").and_then(|s| s.as_array()) {
                for snap in snaps {
                    let ticker = snap.get("ticker").and_then(|t| t.as_str()).unwrap_or("?");
                    let price = snap.get("price").and_then(|p| p.as_f64())
                        .or_else(|| snap.get("current_price").and_then(|p| p.as_f64()))
                        .unwrap_or(0.0);
                    let prev = snap.get("previous_close").and_then(|p| p.as_f64()).unwrap_or(price);
                    let ret = if prev != 0.0 { (price / prev - 1.0) * 100.0 } else { 0.0 };
                    let mcap = snap.get("market_cap").and_then(|m| m.as_f64());
                    let mcap_str = match mcap {
                        Some(m) if m >= 1e12 => format!(" ${:.1}T", m / 1e12),
                        Some(m) if m >= 1e9 => format!(" ${:.0}B", m / 1e9),
                        _ => String::new(),
                    };
                    // Include 1mo return if available for trend context
                    let ret_1mo = snap.get("returns")
                        .and_then(|r| r.get("1mo")).and_then(|r| r.as_f64())
                        .map(|r| format!(" 1mo:{:+.1}%", r * 100.0))
                        .unwrap_or_default();
                    lines.push(format!("{}:{:.2}({:+.1}%){}{}", ticker, price, ret, mcap_str, ret_1mo));
                }
            }
            let n = v.get("snapshots").and_then(|s| s.as_array()).map(|a| a.len()).unwrap_or(0);
            let first_snap = v.get("snapshots").and_then(|s| s.as_array()).and_then(|a| a.first());
            let session = first_snap.and_then(|s| s.get("session_state")).and_then(|s| s.as_str()).unwrap_or("unknown");
            let market_note = v.get("market_note").and_then(|s| s.as_str()).unwrap_or("");
            let note_str = if !market_note.is_empty() { format!(",\"market_note\":\"{}\"", market_note) } else { String::new() };
            format!(
                "\"n\":{},\"session\":\"{}\"{},\"_schema\":\".snapshots[].{{ticker,current_price,previous_close,market_cap,shares_outstanding,returns.{{1d,1mo,3mo,6mo,1y}}}}\",\"tickers\":[{}]",
                n, session, note_str,
                lines.iter()
                    .map(|l| format!("\"{}\"", l))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        "finance_odds" => {
            // Extract: top markets by volume with probability
            let mut lines = Vec::new();
            let mut total_vol_usd: f64 = 0.0;
            let mut kalshi_count: usize = 0;
            let mut poly_count: usize = 0;
            if let Some(markets) = v.get("markets").and_then(|m| m.as_array()) {
                // Stats
                for mkt in markets.iter() {
                    total_vol_usd += mkt.get("volume_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    match mkt.get("source").and_then(|s| s.as_str()) {
                        Some("kalshi") => kalshi_count += 1,
                        Some("polymarket") => poly_count += 1,
                        _ => {}
                    }
                }
                // Sort by volume descending
                let mut sorted: Vec<&serde_json::Value> = markets.iter().collect();
                sorted.sort_by(|a, b| {
                    let va = a.get("volume_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let vb = b.get("volume_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
                });
                // Filter: prefer markets where the query term appears in the title
                // or event_ticker. This catches markets whose parent event matched
                // the query even when the individual market title uses an abbreviation
                // (e.g. "BTC" vs "bitcoin", "CPI" vs "inflation").
                let query_str = v.get("query").and_then(|q| q.as_str()).unwrap_or("");
                let query_lower = query_str.to_lowercase();
                let relevance_match = |mkt: &&serde_json::Value| -> bool {
                    if query_lower.is_empty() { return true; }
                    let title_ok = mkt.get("title").and_then(|t| t.as_str())
                        .map(|t| t.to_lowercase().contains(&query_lower))
                        .unwrap_or(false);
                    let ticker_ok = mkt.get("event_ticker").and_then(|t| t.as_str())
                        .map(|t| t.to_lowercase().contains(&query_lower))
                        .unwrap_or(false);
                    title_ok || ticker_ok
                };
                let relevant: Vec<_> = sorted.iter().filter(|m| relevance_match(m)).collect();
                let skipped = sorted.len() - relevant.len();
                // If the title/ticker filter removes ALL results, fall back to showing
                // unfiltered results — the search function already validated event-level
                // relevance, so these markets ARE topical.
                let display: &Vec<_> = if relevant.is_empty() && !sorted.is_empty() {
                    if skipped > 0 {
                        lines.push(format!("_note:{} results filtered (title did not contain query '{}'), showing unfiltered", skipped, query_str));
                    }
                    &sorted.iter().collect()
                } else {
                    if skipped > 0 {
                        lines.push(format!("_note:{} results filtered (title did not contain query '{}')", skipped, query_str));
                    }
                    &relevant
                };
                for mkt in display.iter().take(10) {
                    let title = mkt.get("title").and_then(|t| t.as_str()).unwrap_or("?");
                    let title_short: String = title.chars().take(120).collect();
                    let prob = mkt.get("probability_yes").and_then(|p| p.as_f64()).unwrap_or(0.0);
                    let vol = mkt.get("volume_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let src = mkt.get("source").and_then(|s| s.as_str()).unwrap_or("?");
                    let delta_str = mkt.get("delta_since_last_sync")
                        .and_then(|d| d.get("probability_delta_pct_points"))
                        .and_then(|d| d.as_f64())
                        .map(|d| format!(" d:{:+.1}pp", d))
                        .unwrap_or_default();
                    lines.push(format!(
                        "{}|{:.0}%|${:.0}K_USD|{}{}",
                        title_short, prob * 100.0, vol / 1000.0, src, delta_str
                    ));
                }
            }
            let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("?");
            let total = v.get("total_markets").and_then(|t| t.as_u64()).unwrap_or(0);
            // Surface sync age so delta_pp can be interpreted correctly
            let sync_age = v.get("cache_synced_at").or_else(|| v.get("generated_at"))
                .and_then(|s| s.as_str())
                .map(|s| format!(",\"delta_as_of\":\"{}\"", &s[..16.min(s.len())]))
                .unwrap_or_default();
            format!(
                "\"query\":\"{}\",\"total_markets\":{},\"total_vol_usd\":{:.0},\"sources\":\"kalshi:{} poly:{}\"{},\"_schema\":\".markets[].{{title,probability_yes,volume_usd,source,market_id,event_id}}\",\"top\":[{}]",
                query, total, total_vol_usd, kalshi_count, poly_count, sync_age,
                lines.iter()
                    .map(|l| format!("\"{}\"", l.replace('"', "'")))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        "finance_options" => {
            // Extract: ticker, price, P/C ratio, max pain, IV, ATM straddle
            let ticker = v.get("ticker").and_then(|t| t.as_str()).unwrap_or("?");
            let price = v.get("underlying_price").and_then(|p| p.as_f64()).unwrap_or(0.0);

            // Look for summary fields
            let pc_vol = v.get("put_call_volume_ratio").and_then(|p| p.as_f64());
            let pc_oi = v.get("put_call_oi_ratio").and_then(|p| p.as_f64());
            let max_pain = v.get("max_pain").and_then(|p| p.as_f64());
            let iv = v.get("implied_volatility").and_then(|i| i.as_f64())
                .or_else(|| v.get("iv").and_then(|i| i.as_f64()));
            let total_oi = v.get("total_open_interest").and_then(|o| o.as_u64());
            let total_vol = v.get("total_volume").and_then(|o| o.as_u64());

            // Expiration dates available
            let exp_count = v.get("expirations").and_then(|e| e.as_array()).map(|a| a.len())
                .or_else(|| v.get("chains").and_then(|c| c.as_array()).map(|a| a.len()))
                .unwrap_or(0);

            let selected_exp = v.get("selected_expiry").and_then(|e| e.as_str());
            let sel_reason = v.get("selection_reason").and_then(|e| e.as_str());

            let mut parts = vec![format!("\"ticker\":\"{}\",\"price\":{:.2}", ticker, price)];
            if let Some(exp) = selected_exp { parts.push(format!("\"selected_expiry\":\"{}\"", exp)); }
            if let Some(reason) = sel_reason { parts.push(format!("\"selection_reason\":\"{}\"", reason)); }
            if let Some(r) = pc_vol { parts.push(format!("\"pc_vol_ratio\":{:.2}", r)); }
            if let Some(r) = pc_oi { parts.push(format!("\"pc_oi_ratio\":{:.2}", r)); }
            if let Some(mp) = max_pain {
                parts.push(format!("\"max_pain\":{:.2}", mp));
                let gap = mp - price;
                let gravity = if gap > 1.0 {
                    format!("price ${:.0} below pain ${:.0} (gap +{:.0})", price, mp, gap)
                } else if gap < -1.0 {
                    format!("price ${:.0} above pain ${:.0} (gap -{:.0})", price, mp, gap.abs())
                } else {
                    format!("price ≈ pain ${:.0}", mp)
                };
                parts.push(format!("\"max_pain_gap\":\"{}\"", gravity));
            }
            if let Some(i) = iv { parts.push(format!("\"iv_pct\":{:.1}", i * 100.0)); }
            if let Some(oi) = total_oi { parts.push(format!("\"total_oi\":{}", oi)); }
            if let Some(vol) = total_vol { parts.push(format!("\"total_vol\":{}", vol)); }
            if exp_count > 0 { parts.push(format!("\"exp_dates\":{}", exp_count)); }
            parts.push("\"_schema\":\".chains[].{expiry,calls[].{strike,bid,ask,iv,oi,volume},puts[]}\"".to_string());
            parts.join(",")
        }
        "finance_timeseries" => {
            match mcp_timeseries_summary(output) {
                Some(summary) => format!(
                    "\"_schema\":\".series[].{{ticker,candles[].{{t,o,h,l,c,v}}}}\",\"_summary\":{}",
                    summary
                ),
                None => format!("\"_summary\":\"timeseries parse failed — read file\""),
            }
        }
        "finance_cot" => {
            // Group all weeks by contract, compute historical percentile + direction signal + data age
            let mut contract_weeks: std::collections::HashMap<String, Vec<(String, i64, i64, f64)>> =
                std::collections::HashMap::new();
            if let Some(positions) = v.get("positions").and_then(|p| p.as_array()) {
                for pos in positions {
                    let contract = pos.get("contract_name").and_then(|c| c.as_str()).unwrap_or("?").to_string();
                    let date = pos.get("report_date").and_then(|d| d.as_str()).unwrap_or("").to_string();
                    let net = pos.get("spec_net").and_then(|s| s.as_i64()).unwrap_or(0);
                    let chg = pos.get("spec_net_change").and_then(|c| c.as_i64()).unwrap_or(0);
                    let pct = pos.get("spec_net_pct_oi").and_then(|p| p.as_f64()).unwrap_or(0.0);
                    contract_weeks.entry(contract).or_default().push((date, net, chg, pct));
                }
            }

            let gen_date = v.get("generated_at").and_then(|g| g.as_str())
                .and_then(|g| if g.len() >= 10 { Some(g[..10].to_string()) } else { None });

            let mut lines = Vec::new();
            let mut contract_list: Vec<String> = contract_weeks.keys().cloned().collect();
            contract_list.sort();

            for contract in &contract_list {
                let weeks = contract_weeks.get(contract).unwrap();
                let mut sorted = weeks.clone();
                sorted.sort_by(|a, b| b.0.cmp(&a.0)); // desc by date → latest first

                let (latest_date, latest_net, latest_chg, latest_pct) = &sorted[0];
                let all_nets: Vec<i64> = sorted.iter().map(|w| w.1).collect();
                let n_weeks = all_nets.len();

                // Historical percentile: where does current net sit across all returned weeks
                let rank = all_nets.iter().filter(|&&n| n <= *latest_net).count();
                let pctile = if n_weeks > 1 { (rank * 100) / n_weeks } else { 50usize };

                // Direction: week-over-week change relative to current position size (not absolute)
                // Threshold: |chg| > 15% of |net|, minimum 200 contracts to filter noise
                let abs_net = latest_net.unsigned_abs() as f64;
                let abs_chg = latest_chg.unsigned_abs() as f64;
                let rel_move = if abs_net > 0.0 { abs_chg / abs_net } else { 0.0 };
                let significant = abs_chg >= 200.0 && rel_move >= 0.10;
                let direction = if !significant { "FLAT" }
                    else if *latest_chg > 0 && *latest_net < 0 { "COVERING↑" }
                    else if *latest_chg > 0 { "ADDING_LONGS↑" }
                    else if *latest_chg < 0 && *latest_net > 0 { "TRIMMING↓" }
                    else { "ADDING_SHORTS↓" };

                // Signal: combine direction of net with historical percentile
                // pctile = rank of current net (low = most negative / most short)
                // Signal: net direction + whether position is growing or shrinking vs recent history
                // pctile low = position more negative (shorter) than most recent weeks
                // pctile high = position less negative (more covered) than most recent weeks
                let signal = if *latest_net < -500 {
                    match pctile {
                        p if p <= 10 => "NET_SHORT_DEEPENING",   // most negative in range
                        p if p <= 30 => "NET_SHORT",
                        p if p >= 70 => "NET_SHORT_COVERING",    // still short but less than recent weeks
                        _ => "NET_SHORT",
                    }
                } else if *latest_net > 500 {
                    match pctile {
                        p if p >= 90 => "NET_LONG_GROWING",
                        p if p >= 70 => "NET_LONG",
                        p if p <= 10 => "NET_LONG_TRIMMING",     // still long but less than recent weeks
                        _ => "NET_LONG",
                    }
                } else { "FLAT" };

                // Data age
                let age_str = if latest_date.len() >= 10 {
                    if let Some(ref gd) = gen_date {
                        use chrono::NaiveDate;
                        if let (Ok(rd), Ok(gd)) = (
                            NaiveDate::parse_from_str(&latest_date[..10], "%Y-%m-%d"),
                            NaiveDate::parse_from_str(gd, "%Y-%m-%d"),
                        ) {
                            let days = (gd - rd).num_days();
                            if days > 0 { format!(" (as_of:{},{}d-stale)", &latest_date[..10], days) } else { String::new() }
                        } else { format!(" as_of:{}", &latest_date[..10]) }
                    } else { format!(" as_of:{}", &latest_date[..10]) }
                } else { String::new() };

                let pctile_tag = if pctile <= 15 || pctile >= 85 { format!("[{}th/{}w]", pctile, n_weeks) } else { format!("({}w)", n_weeks) };
                let name_short: String = contract.chars().take(35).collect();
                lines.push(format!(
                    "{}|{}{}|net:{:+}|chg:{:+}|{:.1}%OI{}",
                    name_short, signal, pctile_tag, latest_net, latest_chg, latest_pct, age_str
                ));
            }

            let report_type = v.get("report_type").and_then(|r| r.as_str()).unwrap_or("?");
            format!(
                "\"report\":\"{}\",\"contracts\":{},\"_schema\":\".positions[].{{contract_name,report_date,spec_net,spec_net_change,spec_net_pct_oi,comm_net}}\",\"latest\":[{}]",
                report_type, contract_list.len(),
                lines.iter()
                    .map(|l| format!("\"{}\"", l.replace('"', "'")))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        "finance_rate_path" => {
            let rate = v.get("current_rate").and_then(|r| r.as_f64()).unwrap_or(0.0);
            let rate_basis = v
                .get("current_rate_basis")
                .and_then(|r| r.as_str())
                .unwrap_or("?");
            let target_lower = v
                .get("current_rates")
                .and_then(|r| r.get("target_lower_bound"))
                .and_then(|r| r.as_f64());
            let target_upper = v
                .get("current_rates")
                .and_then(|r| r.get("target_upper_bound"))
                .and_then(|r| r.as_f64());
            let effective_rate = v
                .get("current_rates")
                .and_then(|r| r.get("effective_rate"))
                .and_then(|r| r.as_f64());
            let mut lines = Vec::new();
            let mut first_50pct_cut: Option<String> = None;
            if let Some(meetings) = v.get("meetings").and_then(|m| m.as_array()) {
                for mtg in meetings {
                    let date = mtg.get("date").and_then(|d| d.as_str()).unwrap_or("?");
                    // Use Option to distinguish missing data from 0% probability
                    let hold = mtg.get("hold_prob").and_then(|p| p.as_f64());
                    let cut  = mtg.get("cut_prob").and_then(|p| p.as_f64());
                    let hike = mtg.get("hike_prob").and_then(|p| p.as_f64());
                    let fmt_prob = |p: Option<f64>| p.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "?".to_string());
                    if date.starts_with("2026") && cut.map_or(false, |c| c > 0.50) && first_50pct_cut.is_none() {
                        first_50pct_cut = Some(date[..7.min(date.len())].to_string());
                    }
                    let label = &date[..7.min(date.len())];
                    lines.push(format!("{}:H{}/C{}/K{}", label, fmt_prob(hold), fmt_prob(cut), fmt_prob(hike)));
                }
            }
            let first_cut_str = first_50pct_cut
                .map(|m| format!(",\"first_50pct_cut_month\":\"{}\"", m))
                .unwrap_or_default();
            let target_range_str = match (target_lower, target_upper) {
                (Some(lower), Some(upper)) => {
                    format!(",\"target_range\":\"{lower:.2}-{upper:.2}\"")
                }
                _ => String::new(),
            };
            let effective_rate_str = effective_rate
                .map(|value| format!(",\"effective_rate\":{value:.2}"))
                .unwrap_or_default();
            format!(
                "\"current_rate\":{:.2},\"current_rate_basis\":\"{}\"{}{}{},\"_note\":\"H/C/K are independently priced prediction markets — may not sum to 100%\",\"_schema\":\".meetings[].{{date,hold_prob,cut_prob,hike_prob,cut_25bp_prob,cut_50bp_plus_prob,volume}}\",\"meetings\":[{}]",
                rate, rate_basis, target_range_str, effective_rate_str, first_cut_str,
                lines.iter().map(|l| format!("\"{}\"", l)).collect::<Vec<_>>().join(",")
            )
        }
        "finance_schedule" => {
            let mut earnings_lines = Vec::new();
            let mut macro_lines = Vec::new();
            let total_earnings;
            if let Some(earnings) = v.get("earnings").and_then(|e| e.as_array()) {
                total_earnings = earnings.len();
                // Parse market cap strings like "$28,611,802,485" → f64 for sorting
                let parse_mcap = |s: &str| -> f64 {
                    s.chars().filter(|c| c.is_ascii_digit()).collect::<String>()
                        .parse::<f64>().unwrap_or(0.0)
                };
                // Sort by market cap descending, show top 30
                let mut sorted: Vec<&serde_json::Value> = earnings.iter().collect();
                sorted.sort_by(|a, b| {
                    let ma = a.get("market_cap").and_then(|m| m.as_str()).map(parse_mcap).unwrap_or(0.0);
                    let mb = b.get("market_cap").and_then(|m| m.as_str()).map(parse_mcap).unwrap_or(0.0);
                    mb.partial_cmp(&ma).unwrap_or(std::cmp::Ordering::Equal)
                });
                for e in sorted.iter().take(30) {
                    let sym = e.get("symbol").and_then(|s| s.as_str()).unwrap_or("?");
                    let date = e.get("date").and_then(|d| d.as_str()).unwrap_or("?");
                    let time = e.get("time").and_then(|t| t.as_str()).unwrap_or("?");
                    let eps = e.get("eps_forecast").and_then(|f| f.as_str()).unwrap_or("?");
                    let mcap_raw = e.get("market_cap").and_then(|m| m.as_str()).unwrap_or("");
                    let mcap_val = parse_mcap(mcap_raw);
                    let mcap_str = if mcap_val >= 1e12 { format!("${:.1}T", mcap_val / 1e12) }
                        else if mcap_val >= 1e9 { format!("${:.0}B", mcap_val / 1e9) }
                        else { String::new() };
                    earnings_lines.push(format!("{}|{}|{}|eps:{} {}", sym, date, time, eps, mcap_str));
                }
            } else {
                total_earnings = 0;
            }
            if let Some(macro_evts) = v.get("macro").and_then(|m| m.as_array()) {
                for evt in macro_evts.iter().take(10) {
                    let name = evt.get("title").and_then(|n| n.as_str()).unwrap_or("?");
                    let date = evt.get("date").and_then(|d| d.as_str()).unwrap_or("?");
                    let name_short: String = name.chars().take(40).collect();
                    macro_lines.push(format!("{}|{}", name_short, date));
                }
            }
            // Stats: pre/after-hours breakdown
            let mut pre_count = 0usize;
            let mut after_count = 0usize;
            let mut by_date: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            if let Some(earnings) = v.get("earnings").and_then(|e| e.as_array()) {
                for e in earnings {
                    let time = e.get("time").and_then(|t| t.as_str()).unwrap_or("");
                    if time.contains("pre") { pre_count += 1; }
                    else if time.contains("after") { after_count += 1; }
                    let date = e.get("date").and_then(|d| d.as_str()).unwrap_or("?");
                    *by_date.entry(date[..10.min(date.len())].to_string()).or_insert(0) += 1;
                }
            }
            let date_dist: Vec<String> = by_date.iter().map(|(d, n)| format!("{}:{}", d, n)).collect();
            format!(
                "\"earnings_total\":{},\"showing_top\":30,\"pre_market\":{},\"after_hours\":{},\"by_date\":\"{}\",\"macro_count\":{},\"_schema\":\".earnings[].{{symbol,date,time,market_cap,eps_forecast,last_year_eps,fiscal_quarter_ending}}\",\"earnings\":[{}],\"macro\":[{}]",
                total_earnings, pre_count, after_count, date_dist.join(" "),
                macro_lines.len(),
                earnings_lines.iter().map(|l| format!("\"{}\"", l.replace('"', "'"))).collect::<Vec<_>>().join(","),
                macro_lines.iter().map(|l| format!("\"{}\"", l.replace('"', "'"))).collect::<Vec<_>>().join(","),
            )
        }
        "finance_news" => {
            let ticker = v.get("ticker").and_then(|t| t.as_str()).unwrap_or("?");
            let date = v.get("date").and_then(|d| d.as_str()).unwrap_or("?");
            let mut lines = Vec::new();
            // Track source domain counts
            let mut source_counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            let articles = v.get("articles").or_else(|| v.get("news"))
                .and_then(|a| a.as_array());
            let total_articles = articles.map(|a| a.len()).unwrap_or(0);
            if let Some(arts) = v.get("articles").or_else(|| v.get("news")).and_then(|a| a.as_array()) {
                for art in arts.iter() {
                    // Extract domain from link for source tracking
                    if let Some(link) = art.get("link").and_then(|l| l.as_str()) {
                        // Simple domain extraction: grab 2nd segment after //
                        let domain = link.split("//").nth(1)
                            .and_then(|s| s.split('/').next())
                            .and_then(|d| d.split('.').rev().take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(".").into())
                            .unwrap_or("unknown".to_string());
                        // Simplify google news to actual source from title
                        let src_label = if domain.contains("google") {
                            // Try to extract publisher from title pattern "title - Publisher"
                            art.get("title").and_then(|t| t.as_str())
                                .and_then(|t| t.rsplit(" - ").next())
                                .map(|s| s.chars().take(20).collect::<String>())
                                .unwrap_or("google-news".to_string())
                        } else {
                            domain.chars().take(20).collect()
                        };
                        *source_counts.entry(src_label).or_insert(0) += 1;
                    }
                    let title = art.get("title").and_then(|t| t.as_str()).unwrap_or("?");
                    let pub_date = art.get("published_at").and_then(|d| d.as_str())
                        .map(|d| &d[..10.min(d.len())]).unwrap_or("?");
                    let title_short: String = title.chars().take(80).collect();
                    lines.push(format!("{} ({})", title_short, pub_date));
                }
            }
            // Top 5 sources
            let mut src_vec: Vec<(String, usize)> = source_counts.into_iter().collect();
            src_vec.sort_by(|a, b| b.1.cmp(&a.1));
            let top_sources: Vec<String> = src_vec.iter().take(5).map(|(s, n)| format!("{}:{}", s, n)).collect();
            // Generic/ambiguous tickers produce noisy Google RSS results
            let noisy_tickers = ["SPY", "AI", "TLT", "IWM", "QQQ", "GLD", "USO", "DIA", "XLE", "VIX"];
            let noise_flag = if noisy_tickers.contains(&ticker.to_uppercase().as_str()) {
                ",\"noise\":\"HIGH — ETF/index ticker, RSS results likely mixed with unrelated stories\""
            } else { "" };
            format!(
                "\"ticker\":\"{}\",\"date\":\"{}\",\"count\":{},\"top_sources\":\"{}\"{},\"_schema\":\".articles[].{{title,published_at,link}}\",\"headlines\":[{}]",
                ticker, date, total_articles, top_sources.join(" "), noise_flag,
                lines.iter().take(15).map(|l| format!("\"{}\"", l.replace('"', "'"))).collect::<Vec<_>>().join(",")
            )
        }
        "finance_auctions" => {
            let mut lines = Vec::new();
            if let Some(auctions) = v.get("auctions").and_then(|a| a.as_array()) {
                for auction in auctions.iter().take(15) {
                    let sec_type = auction.get("security_type").and_then(|t| t.as_str()).unwrap_or("?");
                    let term = auction.get("security_term").and_then(|t| t.as_str()).unwrap_or("?");
                    let btc = auction.get("bid_to_cover_ratio").and_then(|b| b.as_f64()).unwrap_or(0.0);
                    let accepted = auction.get("total_accepted").and_then(|a| a.as_f64()).unwrap_or(0.0);
                    let indirect = auction.get("indirect_bidder_pct").and_then(|i| i.as_f64()).unwrap_or(0.0);
                    let date = auction.get("auction_date").and_then(|d| d.as_str()).unwrap_or("?");
                    let accepted_b = accepted / 1e9;
                    // high_yield: present for notes/bonds, null for bills (discount securities)
                    let yield_str = auction.get("high_yield").and_then(|y| y.as_f64())
                        .map(|y| format!("|y:{:.3}%", y))
                        .unwrap_or_default();
                    lines.push(format!("{} {}|BTC:{:.2}|${:.0}B|ind:{:.0}%{}|{}", sec_type, term, btc, accepted_b, indirect, yield_str, date));
                }
            }
            let total = v.get("count").and_then(|c| c.as_u64()).unwrap_or(lines.len() as u64);
            format!(
                "\"count\":{},\"_schema\":\".auctions[].{{security_type,security_term,auction_date,bid_to_cover_ratio,high_yield,total_accepted,indirect_bidder_pct,direct_bidder_pct}}\",\"auctions\":[{}]",
                total,
                lines.iter().map(|l| format!("\"{}\"", l.replace('"', "'"))).collect::<Vec<_>>().join(",")
            )
        }
        _ => {
            // No custom summary for this tool — data is in the file, read it.
            format!("\"_hint\":\"no summary for {tool} — use data_query or read file directly\"")
        }
    }
}

fn mcp_build_cli_args(tool: &str, args: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let s = |v: &str| v.to_string();
    match tool {
        "finance_snapshot" => {
            let tickers = args
                .get("tickers")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("tickers required"))?;
            let mut v = vec![s("finance"), s("snapshot"), s("--tickers"), s(tickers)];
            if let Some(provider) = args.get("provider").and_then(|p| p.as_str()) {
                v.extend([s("--provider"), s(provider)]);
            }
            if let Some(account) = args.get("ibkr_account").and_then(|a| a.as_str()) {
                v.extend([s("--ibkr-account"), s(account)]);
            }
            Ok(v)
        }
        "finance_timeseries" => {
            let mut v = vec![s("finance"), s("timeseries")];
            if let Some(preset) = args.get("preset").and_then(|p| p.as_str()) {
                v.extend([s("--preset"), s(preset)]);
            }
            if let Some(tickers) = args
                .get("tickers")
                .or_else(|| args.get("ticker"))
                .and_then(|t| t.as_str())
            {
                v.extend([s("--tickers"), s(tickers)]);
            }
            if v.len() <= 2 {
                anyhow::bail!("either preset or tickers required");
            }
            if let Some(range) = args.get("range").and_then(|r| r.as_str()) {
                v.extend([s("--range"), s(range)]);
            }
            if let Some(granularity) = args.get("granularity").and_then(|g| g.as_str()) {
                v.extend([s("--granularity"), s(granularity)]);
            }
            if let Some(provider) = args.get("provider").and_then(|p| p.as_str()) {
                v.extend([s("--provider"), s(provider)]);
            }
            if let Some(account) = args.get("ibkr_account").and_then(|a| a.as_str()) {
                v.extend([s("--ibkr-account"), s(account)]);
            }
            if let Some(odds_market) = args.get("odds_market").and_then(|m| m.as_str()) {
                v.extend([s("--odds-market"), s(odds_market)]);
            }
            if let Some(odds_provider) = args.get("odds_provider").and_then(|p| p.as_str()) {
                v.extend([s("--odds-provider"), s(odds_provider)]);
            }
            if let Some(odds_side) = args.get("odds_side").and_then(|s_| s_.as_str()) {
                v.extend([s("--odds-side"), s(odds_side)]);
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
            if let Some(provider) = args.get("provider").and_then(|p| p.as_str()) {
                v.extend([s("--provider"), s(provider)]);
            }
            if let Some(account) = args.get("ibkr_account").and_then(|a| a.as_str()) {
                v.extend([s("--ibkr-account"), s(account)]);
            }
            let summary = args
                .get("summary")
                .and_then(|b| b.as_bool())
                .unwrap_or(true);
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
        "finance_sync" => {
            let mut v = vec![s("finance"), s("sync")];
            if let Some(sources) = args.get("sources").and_then(|s| s.as_str()) {
                v.extend([s("--sources"), s(sources)]);
            }
            if let Some(max_pages) = args.get("max_pages").and_then(|n| n.as_u64()) {
                v.extend([s("--max-pages"), max_pages.to_string()]);
            }
            if args
                .get("strict")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                v.push(s("--strict"));
            }
            if args
                .get("include_sports")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                v.push(s("--include-sports"));
            }
            if args
                .get("include_historical")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                v.push(s("--include-historical"));
            }
            if args
                .get("stream_refresh")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                v.push(s("--stream-refresh"));
            }
            if let Some(hours) = args.get("refresh_heartbeat_hours").and_then(|n| n.as_u64()) {
                v.extend([s("--refresh-heartbeat-hours"), hours.to_string()]);
            }
            if let Some(secs) = args
                .get("stream_refresh_timeout_secs")
                .and_then(|n| n.as_u64())
            {
                v.extend([s("--stream-refresh-timeout-secs"), secs.to_string()]);
            }
            if args.get("full").and_then(|b| b.as_bool()).unwrap_or(false) {
                v.push(s("--full"));
            }
            Ok(v)
        }
        "finance_paper" => {
            let mut v = vec![s("finance"), s("paper")];
            if let Some(command) = args.get("command").and_then(|c| c.as_str()) {
                v.extend([s("--command"), s(command)]);
            }
            if let Some(mode) = args.get("mode").and_then(|m| m.as_str()) {
                v.extend([s("--mode"), s(mode)]);
            }
            if let Some(account) = args.get("account").and_then(|a| a.as_str()) {
                v.extend([s("--account"), s(account)]);
            }
            if let Some(provider) = args.get("provider").and_then(|p| p.as_str()) {
                v.extend([s("--provider"), s(provider)]);
            }
            if let Some(market) = args.get("market").and_then(|m| m.as_str()) {
                v.extend([s("--market"), s(market)]);
            }
            if let Some(side) = args.get("side").and_then(|s| s.as_str()) {
                v.extend([s("--side"), s(side)]);
            }
            if let Some(action) = args.get("action").and_then(|a| a.as_str()) {
                v.extend([s("--action"), s(action)]);
            }
            if let Some(qty) = args.get("qty").and_then(|q| q.as_f64()) {
                v.extend([s("--qty"), qty.to_string()]);
            }
            if let Some(price) = args.get("price").and_then(|p| p.as_f64()) {
                v.extend([s("--price"), price.to_string()]);
            }
            if let Some(starting_cash) = args.get("starting_cash").and_then(|c| c.as_f64()) {
                v.extend([s("--starting-cash"), starting_cash.to_string()]);
            }
            if let Some(limit) = args.get("limit").and_then(|n| n.as_u64()) {
                v.extend([s("--limit"), limit.to_string()]);
            }
            Ok(v)
        }
        "finance_ibkr_account_summary" => {
            let mut v = vec![
                s("finance"),
                s("ibkr"),
                s("--command"),
                s("account-summary"),
            ];
            if let Some(account) = args.get("account").and_then(|a| a.as_str()) {
                v.extend([s("--account"), s(account)]);
            }
            if let Some(tags) = args.get("tags").and_then(|t| t.as_str()) {
                v.extend([s("--tags"), s(tags)]);
            }
            Ok(v)
        }
        "finance_ibkr_positions" => {
            let mut v = vec![s("finance"), s("ibkr"), s("--command"), s("positions")];
            if let Some(account) = args.get("account").and_then(|a| a.as_str()) {
                v.extend([s("--account"), s(account)]);
            }
            Ok(v)
        }
        "finance_ibkr_portfolio" => {
            let mut v = vec![s("finance"), s("ibkr"), s("--command"), s("portfolio")];
            if let Some(account) = args.get("account").and_then(|a| a.as_str()) {
                v.extend([s("--account"), s(account)]);
            }
            Ok(v)
        }
        "finance_ibkr_open_orders" => Ok(vec![
            s("finance"),
            s("ibkr"),
            s("--command"),
            s("open-orders"),
        ]),
        "finance_ibkr_place_order" => {
            let symbol = args
                .get("symbol")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("symbol required"))?;
            let side = args
                .get("side")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("side required"))?;
            let quantity = args
                .get("quantity")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| anyhow::anyhow!("quantity required"))?;
            let mut v = vec![
                s("finance"),
                s("ibkr"),
                s("--command"),
                s("place-order"),
                s("--symbol"),
                s(symbol),
                s("--side"),
                s(side),
                s("--quantity"),
                quantity.to_string(),
            ];
            if let Some(account) = args.get("account").and_then(|a| a.as_str()) {
                v.extend([s("--account"), s(account)]);
            }
            if let Some(order_type) = args.get("order_type").and_then(|a| a.as_str()) {
                v.extend([s("--order-type"), s(order_type)]);
            }
            if let Some(limit_price) = args.get("limit_price").and_then(|a| a.as_f64()) {
                v.extend([s("--limit-price"), limit_price.to_string()]);
            }
            if let Some(stop_price) = args.get("stop_price").and_then(|a| a.as_f64()) {
                v.extend([s("--stop-price"), stop_price.to_string()]);
            }
            if let Some(tif) = args.get("tif").and_then(|a| a.as_str()) {
                v.extend([s("--tif"), s(tif)]);
            }
            if let Some(sec_type) = args.get("sec_type").and_then(|a| a.as_str()) {
                v.extend([s("--sec-type"), s(sec_type)]);
            }
            if let Some(exchange) = args.get("exchange").and_then(|a| a.as_str()) {
                v.extend([s("--exchange"), s(exchange)]);
            }
            if let Some(primary_exchange) = args.get("primary_exchange").and_then(|a| a.as_str()) {
                v.extend([s("--primary-exchange"), s(primary_exchange)]);
            }
            if let Some(currency) = args.get("currency").and_then(|a| a.as_str()) {
                v.extend([s("--currency"), s(currency)]);
            }
            if let Some(expiry) = args.get("expiry").and_then(|a| a.as_str()) {
                v.extend([s("--expiry"), s(expiry)]);
            }
            if let Some(strike) = args.get("strike").and_then(|a| a.as_f64()) {
                v.extend([s("--strike"), strike.to_string()]);
            }
            if let Some(right) = args.get("right").and_then(|a| a.as_str()) {
                v.extend([s("--right"), s(right)]);
            }
            if let Some(multiplier) = args.get("multiplier").and_then(|a| a.as_str()) {
                v.extend([s("--multiplier"), s(multiplier)]);
            }
            if let Some(trading_class) = args.get("trading_class").and_then(|a| a.as_str()) {
                v.extend([s("--trading-class"), s(trading_class)]);
            }
            Ok(v)
        }
        "finance_ibkr_cancel_order" => {
            let order_id = args
                .get("order_id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow::anyhow!("order_id required"))?;
            Ok(vec![
                s("finance"),
                s("ibkr"),
                s("--command"),
                s("cancel-order"),
                s("--order-id"),
                order_id.to_string(),
            ])
        }
        "web_search" => {
            let query = args
                .get("query")
                .and_then(|q| q.as_str())
                .ok_or_else(|| anyhow::anyhow!("query required"))?;
            let mut v = vec![s("web"), s("search"), s("--query"), s(query)];
            if let Some(mode) = args.get("mode").and_then(|m| m.as_str()) {
                v.extend([s("--mode"), s(mode)]);
            }
            if let Some(domains) = args.get("domains").and_then(|d| d.as_str()) {
                v.extend([s("--domains"), s(domains)]);
            }
            if let Some(exclude) = args.get("exclude_domains").and_then(|d| d.as_str()) {
                v.extend([s("--exclude-domains"), s(exclude)]);
            }
            if let Some(recency) = args.get("recency").and_then(|r| r.as_str()) {
                v.extend([s("--recency"), s(recency)]);
            }
            if let Some(since) = args.get("since").and_then(|d| d.as_str()) {
                v.extend([s("--since"), s(since)]);
            }
            if let Some(until) = args.get("until").and_then(|d| d.as_str()) {
                v.extend([s("--until"), s(until)]);
            }
            if let Some(top) = args.get("top").and_then(|n| n.as_u64()) {
                v.extend([s("--top"), top.to_string()]);
            }
            if let Some(probe_top) = args.get("probe_top").and_then(|n| n.as_u64()) {
                v.extend([s("--probe-top"), probe_top.to_string()]);
            }
            if let Some(max_parallel) = args.get("max_parallel").and_then(|n| n.as_u64()) {
                v.extend([s("--max-parallel"), max_parallel.to_string()]);
            }
            if let Some(track_key) = args.get("track_key").and_then(|k| k.as_str()) {
                v.extend([s("--track-key"), s(track_key)]);
            }
            if args.get("full").and_then(|b| b.as_bool()).unwrap_or(false) {
                v.push(s("--full"));
            }
            Ok(v)
        }
        "web_read" => {
            let mut v = vec![s("web"), s("read")];
            if let Some(url) = args.get("url").and_then(|u| u.as_str()) {
                v.extend([s("--url"), s(url)]);
            }
            if let Some(urls) = args.get("urls").and_then(|u| u.as_array()) {
                for url in urls {
                    if let Some(url_str) = url.as_str() {
                        v.extend([s("--url"), s(url_str)]);
                    }
                }
            }
            if v.len() == 2 {
                return Err(anyhow::anyhow!("url or urls required"));
            }
            if let Some(max_parallel) = args.get("max_parallel").and_then(|n| n.as_u64()) {
                v.extend([s("--max-parallel"), max_parallel.to_string()]);
            }
            if let Some(max_chars) = args.get("max_chars").and_then(|n| n.as_u64()) {
                v.extend([s("--max-chars"), max_chars.to_string()]);
            }
            if args.get("full").and_then(|b| b.as_bool()).unwrap_or(false) {
                v.push(s("--full"));
            }
            Ok(v)
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
            if args
                .get("sitemap")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
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
            let mut v = vec![s("finance"), s("search"), s("--query"), s(query)];
            if let Some(provider) = args.get("provider").and_then(|p| p.as_str()) {
                v.extend([s("--provider"), s(provider)]);
            }
            if let Some(account) = args.get("ibkr_account").and_then(|a| a.as_str()) {
                v.extend([s("--ibkr-account"), s(account)]);
            }
            Ok(v)
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
            if args
                .get("include_text")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
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
            let kind = args.get("kind").and_then(|k| k.as_str()).unwrap_or("macro");
            v.extend([s("--kind"), s(kind)]);
            let profile_arg = args.get("macro_profile").and_then(|p| p.as_str());
            let mut macro_profile = profile_arg.unwrap_or("market").to_string();
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
            let major = args
                .get("major")
                .and_then(|b| b.as_bool())
                .unwrap_or(kind == "macro" || kind == "all");
            if major {
                v.push(s("--major"));
                if profile_arg.is_none() {
                    macro_profile = "major".to_string();
                }
            }
            v.extend([s("--macro-profile"), macro_profile]);
            if let Some(tickers) = args.get("ticker").and_then(|s| s.as_str()) {
                v.extend([s("--ticker"), s(tickers)]);
            }
            if let Some(min_cap) = args.get("min_cap").and_then(|s| s.as_str()) {
                v.extend([s("--min-cap"), s(min_cap)]);
            }
            if let Some(time) = args.get("time").and_then(|s| s.as_str()) {
                v.extend([s("--time"), s(time)]);
            }
            Ok(v)
        }
        "finance_auctions" => {
            let mut v = vec![s("finance"), s("auctions")];
            if let Some(st) = args.get("security_type").and_then(|s| s.as_str()) {
                v.extend([s("--security-type"), s(st)]);
            }
            if let Some(limit) = args.get("limit").and_then(|n| n.as_u64()) {
                v.extend([s("--limit"), limit.to_string()]);
            }
            Ok(v)
        }
        "finance_cot" => {
            let mut v = vec![s("finance"), s("cot")];
            if let Some(query) = args.get("query").and_then(|q| q.as_str()) {
                v.extend([s("--query"), s(query)]);
            }
            if let Some(weeks) = args.get("weeks").and_then(|n| n.as_u64()) {
                v.extend([s("--weeks"), weeks.to_string()]);
            }
            if let Some(report) = args.get("report").and_then(|r| r.as_str()) {
                v.extend([s("--report"), s(report)]);
            }
            Ok(v)
        }
        "finance_nyfed" => {
            let mut v = vec![s("finance"), s("nyfed")];
            if let Some(kind) = args.get("kind").and_then(|k| k.as_str()) {
                v.extend([s("--kind"), s(kind)]);
            }
            Ok(v)
        }
        "finance_volsurface" => {
            let mut v = vec![s("finance"), s("volsurface")];
            if let Some(symbols) = args.get("symbols").and_then(|s| s.as_str()) {
                v.extend([s("--symbols"), s(symbols)]);
            }
            if let Some(history) = args.get("history").and_then(|n| n.as_u64()) {
                v.extend([s("--history"), history.to_string()]);
            }
            Ok(v)
        }
        "finance_stress" => {
            let mut v = vec![s("finance"), s("stress")];
            if let Some(range) = args.get("range").and_then(|n| n.as_u64()) {
                v.extend([s("--range"), range.to_string()]);
            }
            Ok(v)
        }
        "finance_fiscal" => {
            let mut v = vec![s("finance"), s("fiscal")];
            if let Some(kind) = args.get("kind").and_then(|k| k.as_str()) {
                v.extend([s("--kind"), s(kind)]);
            }
            Ok(v)
        }
        "finance_ecb" => {
            let mut v = vec![s("finance"), s("ecb")];
            if let Some(preset) = args.get("preset").and_then(|p| p.as_str()) {
                v.extend([s("--preset"), s(preset)]);
            }
            if let Some(dataset) = args.get("dataset").and_then(|d| d.as_str()) {
                v.extend([s("--dataset"), s(dataset)]);
            }
            if let Some(key) = args.get("key").and_then(|k| k.as_str()) {
                v.extend([s("--key"), s(key)]);
            }
            if let Some(start) = args.get("start").and_then(|s| s.as_str()) {
                v.extend([s("--start"), s(start)]);
            }
            Ok(v)
        }
        "finance_eia" => {
            let mut v = vec![s("finance"), s("eia")];
            if let Some(preset) = args.get("preset").and_then(|p| p.as_str()) {
                v.extend([s("--preset"), s(preset)]);
            }
            if let Some(start) = args.get("start").and_then(|s| s.as_str()) {
                v.extend([s("--start"), s(start)]);
            }
            if let Some(length) = args.get("length").and_then(|n| n.as_u64()) {
                v.extend([s("--length"), length.to_string()]);
            }
            Ok(v)
        }
        "finance_bis" => {
            let mut v = vec![s("finance"), s("bis")];
            if let Some(preset) = args.get("preset").and_then(|p| p.as_str()) {
                v.extend([s("--preset"), s(preset)]);
            }
            if let Some(countries) = args.get("countries").and_then(|c| c.as_str()) {
                v.extend([s("--countries"), s(countries)]);
            }
            if let Some(start) = args.get("start").and_then(|s| s.as_str()) {
                v.extend([s("--start"), s(start)]);
            }
            Ok(v)
        }
        "finance_boj" => {
            let mut v = vec![s("finance"), s("boj")];
            if let Some(preset) = args.get("preset").and_then(|p| p.as_str()) {
                v.extend([s("--preset"), s(preset)]);
            }
            if let Some(start) = args.get("start").and_then(|s| s.as_str()) {
                v.extend([s("--start"), s(start)]);
            }
            Ok(v)
        }
        "finance_boe" => {
            let mut v = vec![s("finance"), s("boe")];
            if let Some(preset) = args.get("preset").and_then(|p| p.as_str()) {
                v.extend([s("--preset"), s(preset)]);
            }
            if let Some(codes) = args.get("codes").and_then(|c| c.as_str()) {
                v.extend([s("--codes"), s(codes)]);
            }
            if let Some(start) = args.get("start").and_then(|s| s.as_str()) {
                v.extend([s("--start"), s(start)]);
            }
            Ok(v)
        }
        "code_analyze" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| anyhow::anyhow!("path required"))?;
            let mut v = vec![s("code"), s(path)];
            if args
                .get("pub_api")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                v.push(s("--pub-api"));
            }
            if let Some(find) = args.get("find").and_then(|f| f.as_str()) {
                v.extend([s("--find"), s(find)]);
            }
            if args
                .get("include_files")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
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
        return Err(anyhow::anyhow!("exit {}: {}", output.status, stderr.trim()));
    }

    Ok(stdout)
}

// ── MCP Streamable HTTP transport ────────────────────────────────────────────
// POST /mcp  → JSON-RPC request/response (same handlers as stdio mode)
// GET  /     → health check

async fn cmd_mcp_http(port: u16) -> Result<()> {
    use axum::{
        extract::Json as AxumJson,
        http::{header, Method, StatusCode},
        response::IntoResponse,
        routing::{get, post},
        Router,
    };
    use tower_http::cors::{Any, CorsLayer};

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::ACCEPT, header::AUTHORIZATION]);

    let app = Router::new()
        .route("/", get(mcp_http_health))
        .route("/mcp", post(mcp_http_handle))
        .layer(cors);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    eprintln!("eli mcp http → http://0.0.0.0:{port}/mcp");
    eprintln!("Waiting for connections...");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind port {port}"))?;

    axum::serve(listener, app).await.context("mcp http serve")?;
    Ok(())
}

async fn mcp_http_health() -> impl axum::response::IntoResponse {
    axum::Json(serde_json::json!({
        "status": "ok",
        "server": "eli-mcp",
        "transport": "streamable-http"
    }))
}

async fn mcp_http_handle(
    axum::extract::Json(request): axum::extract::Json<serde_json::Value>,
) -> impl axum::response::IntoResponse {
    let method = match request.get("method").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => {
            // No method — might be a notification or malformed
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32600, "message": "Missing method" }
                })),
            );
        }
    };

    // Notifications have no response
    if method.starts_with("notifications/") {
        return (
            axum::http::StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({"ok": true})),
        );
    }

    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

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

    (axum::http::StatusCode::OK, axum::Json(response))
}

#[cfg(test)]
mod mcp_tool_tests {
    use super::*;

    #[test]
    fn mcp_build_cli_args_maps_finance_paper() {
        let args = serde_json::json!({
            "command": "trade",
            "mode": "live_like",
            "account": "sandbox",
            "provider": "kalshi",
            "market": "KXBTC-26FEB28-B70000",
            "side": "yes",
            "action": "buy",
            "qty": 3.0,
            "price": 0.42
        });
        let built = mcp_build_cli_args("finance_paper", &args).expect("build args");
        assert_eq!(built[0], "finance");
        assert_eq!(built[1], "paper");
        assert!(built.contains(&"--mode".to_string()));
        assert!(built.contains(&"live_like".to_string()));
        assert!(built.contains(&"--provider".to_string()));
        assert!(built.contains(&"kalshi".to_string()));
        assert!(built.contains(&"--qty".to_string()));
    }

    #[test]
    fn mcp_build_cli_args_maps_finance_sync_extended_flags() {
        let args = serde_json::json!({
            "sources": "kalshi",
            "strict": true,
            "include_sports": true,
            "include_historical": true,
            "stream_refresh": true,
            "refresh_heartbeat_hours": 12,
            "stream_refresh_timeout_secs": 300,
            "full": true
        });
        let built = mcp_build_cli_args("finance_sync", &args).expect("build args");
        assert_eq!(built[0], "finance");
        assert_eq!(built[1], "sync");
        assert!(built.contains(&"--sources".to_string()));
        assert!(built.contains(&"kalshi".to_string()));
        assert!(built.contains(&"--strict".to_string()));
        assert!(built.contains(&"--include-sports".to_string()));
        assert!(built.contains(&"--include-historical".to_string()));
        assert!(built.contains(&"--stream-refresh".to_string()));
        assert!(built.contains(&"--refresh-heartbeat-hours".to_string()));
        assert!(built.contains(&"12".to_string()));
        assert!(built.contains(&"--stream-refresh-timeout-secs".to_string()));
        assert!(built.contains(&"300".to_string()));
        assert!(built.contains(&"--full".to_string()));
    }

    #[test]
    fn mcp_build_cli_args_maps_web_search_advanced_filters() {
        let args = serde_json::json!({
            "query": "fed decision",
            "mode": "news",
            "domains": "reuters.com,bloomberg.com",
            "exclude_domains": "example.com",
            "recency": "week",
            "since": "2026-01-01",
            "until": "2026-01-31",
            "top": 10,
            "probe_top": 3,
            "max_parallel": 4,
            "track_key": "fed-weekly",
            "full": true
        });
        let built = mcp_build_cli_args("web_search", &args).expect("build args");
        assert_eq!(built[0], "web");
        assert_eq!(built[1], "search");
        assert!(built.contains(&"--mode".to_string()));
        assert!(built.contains(&"news".to_string()));
        assert!(built.contains(&"--domains".to_string()));
        assert!(built.contains(&"--exclude-domains".to_string()));
        assert!(built.contains(&"--track-key".to_string()));
        assert!(built.contains(&"--full".to_string()));
    }

    #[test]
    fn mcp_build_cli_args_maps_web_read_single_and_batch() {
        let single = serde_json::json!({
            "url": "https://example.com/a"
        });
        let built_single = mcp_build_cli_args("web_read", &single).expect("single args");
        assert_eq!(built_single[0], "web");
        assert_eq!(built_single[1], "read");
        assert!(built_single.contains(&"--url".to_string()));

        let batch = serde_json::json!({
            "urls": ["https://example.com/a", "https://example.com/b"],
            "max_parallel": 8,
            "max_chars": 1600,
            "full": true
        });
        let built_batch = mcp_build_cli_args("web_read", &batch).expect("batch args");
        let url_flag_count = built_batch.iter().filter(|arg| *arg == "--url").count();
        assert_eq!(url_flag_count, 2);
        assert!(built_batch.contains(&"--max-parallel".to_string()));
        assert!(built_batch.contains(&"8".to_string()));
        assert!(built_batch.contains(&"--max-chars".to_string()));
        assert!(built_batch.contains(&"1600".to_string()));
        assert!(built_batch.contains(&"--full".to_string()));
    }

    #[test]
    fn mcp_strip_metadata_removes_per_item_keys() {
        let input = serde_json::json!({
            "query": "crude",
            "markets": [
                {
                    "title": "Oil above $80?",
                    "volume_usd": 50000,
                    "freshness": { "observed_at": "2026-03-08" },
                    "delta_since_last_sync": { "probability_delta_pct_points": 1.2 },
                    "run_meta": { "source": "kalshi" }
                },
                {
                    "title": "Oil below $60?",
                    "volume_usd": 30000,
                    "freshness": { "observed_at": "2026-03-08" },
                    "delta_since_last_sync": { "probability_delta_pct_points": -0.5 },
                    "run_meta": { "source": "kalshi" }
                }
            ]
        });
        let raw = serde_json::to_string(&input).unwrap();
        let cleaned = mcp_strip_metadata(&raw).expect("should strip metadata");
        let v: serde_json::Value = serde_json::from_str(&cleaned).unwrap();

        // Items should not have stripped keys
        let markets = v["markets"].as_array().unwrap();
        for m in markets {
            assert!(m.get("freshness").is_none(), "freshness should be stripped");
            assert!(m.get("delta_since_last_sync").is_none(), "delta should be stripped");
            assert!(m.get("run_meta").is_none(), "run_meta should be stripped");
            assert!(m.get("title").is_some(), "title should remain");
            assert!(m.get("volume_usd").is_some(), "volume should remain");
        }

        // Top-level _meta should have hoisted values
        assert!(v.get("_meta").is_some(), "_meta should exist");
        assert!(v["_meta"].get("freshness").is_some());

        // Cleaned output should be smaller
        assert!(cleaned.len() < raw.len(), "cleaned should be smaller than raw");
    }

    #[test]
    fn mcp_strip_metadata_returns_none_when_nothing_to_strip() {
        let input = serde_json::json!({
            "ticker": "SPY",
            "price": 595.0
        });
        let raw = serde_json::to_string(&input).unwrap();
        assert!(mcp_strip_metadata(&raw).is_none());
    }
}
