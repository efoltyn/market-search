fn build_command_digest(result: &CommandResult) -> String {
    let stdout = result.stdout.trim();
    let stdout_bytes = result.stdout.as_bytes().len();
    let stderr_bytes = result.stderr.as_bytes().len();

    if result.returncode != 0 {
        return format!(
            "returncode={} stdout_bytes={} stderr_bytes={}",
            result.returncode, stdout_bytes, stderr_bytes
        );
    }

    if stdout.is_empty() {
        return format!(
            "returncode={} stdout_bytes={} stderr_bytes={}",
            result.returncode, stdout_bytes, stderr_bytes
        );
    }

    if stdout.starts_with("[OUTPUT SUPPRESSED]") {
        let mut parts: Vec<String> = Vec::new();
        if let Some(saved_to) = stdout
            .split("saved_to=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
        {
            parts.push(format!("saved_to={saved_to}"));
        }
        if let Some(bytes) = stdout
            .split('(')
            .nth(1)
            .and_then(|s| s.split(" bytes").next())
        {
            if bytes.chars().all(|c| c.is_ascii_digit()) {
                parts.push(format!("bytes={bytes}"));
            }
        }
        if let Some(points) = stdout
            .split("Data points: ")
            .nth(1)
            .and_then(|s| s.split('.').next())
        {
            let points = points.trim();
            if !points.is_empty() && points.chars().all(|c| c.is_ascii_digit()) {
                parts.push(format!("data_points={points}"));
            }
        }
        if parts.is_empty() {
            parts.push(format!("stdout_bytes={stdout_bytes}"));
        }
        return parts.join(" ");
    }

    if let Some(value) = extract_json_from_stdout(stdout) {
        if let Some(file_digest) = digest_from_ok_path_json(&result.command, &value) {
            return file_digest;
        }
        return digest_from_json_for_command(&result.command, &value, stdout_bytes);
    }

    let lines = stdout.lines().count();
    format!("stdout_bytes={} lines={}", stdout_bytes, lines)
}

fn digest_from_ok_path_json(command: &str, value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let ok = obj.get("ok")?.as_bool()?;
    if !ok {
        return None;
    }
    let path = obj.get("path")?.as_str()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let nested = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let mut digest = digest_from_json_for_command(command, &nested, raw.as_bytes().len());
    if !digest.contains("saved_to=") {
        digest = format!("saved_to={} {}", path, digest);
    }
    Some(digest)
}

fn digest_from_json_for_command(command: &str, value: &serde_json::Value, bytes: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("bytes={bytes}"));

    match value {
        serde_json::Value::Array(items) => {
            parts.push(format!("items={}", items.len()));
        }
        serde_json::Value::Object(map) => {
            let mut array_parts: Vec<String> = Vec::new();
            for (key, val) in map.iter() {
                if let serde_json::Value::Array(items) = val {
                    array_parts.push(format!("{key}={}", items.len()));
                }
            }
            if !array_parts.is_empty() {
                array_parts.truncate(4);
                parts.extend(array_parts);
            } else {
                parts.push(format!("keys={}", map.len()));
            }
            if let Some(ts) = map
                .get("generated_at")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
            {
                parts.push(format!("generated_at={ts}"));
            } else if let Some(ts) = map
                .get("fetched_at")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
            {
                parts.push(format!("fetched_at={ts}"));
            }
        }
        _ => {}
    }

    let command_parts = command_summary_parts(command, value, 5);
    parts.extend(command_parts);
    parts.join(" ")
}

fn command_summary_parts(
    command: &str,
    value: &serde_json::Value,
    max_parts: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let path = extract_eli_tool_path(command).unwrap_or_default();
    if path.len() >= 2 && path[0] == "finance" && path[1] == "timeseries" {
        out.extend(timeseries_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "snapshot" {
        out.extend(snapshot_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "fundamentals" {
        out.extend(fundamentals_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && (path[1] == "filings" || path[1] == "sec")
    {
        out.extend(filings_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "news" {
        out.extend(news_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "macro" {
        out.extend(macro_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "schedule" {
        out.extend(schedule_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "prices" {
        out.extend(prices_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "odds" {
        out.extend(odds_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "options" {
        out.extend(options_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "search" {
        out.extend(search_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "sync" {
        out.extend(sync_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "web" && path[1] == "search" {
        out.extend(web_search_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "web" && path[1] == "read" {
        out.extend(web_read_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "web" && path[1] == "extract" {
        out.extend(web_extract_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "web" && path[1] == "crawl" {
        out.extend(web_crawl_summary_parts(value));
    }
    out.truncate(max_parts);
    out
}

fn command_summary_lines(
    command: &str,
    value: &serde_json::Value,
    max_lines: usize,
) -> Vec<String> {
    let parts = command_summary_parts(command, value, max_lines);
    parts
        .into_iter()
        .map(|p| format!("insight: {p}"))
        .collect::<Vec<_>>()
}

fn fmt_pct(v: f64) -> String {
    format!("{:.2}%", v * 100.0)
}

fn top_two_by_abs_change(entries: &[(String, f64)]) -> Option<(String, f64, String, f64)> {
    if entries.len() < 2 {
        return None;
    }
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let lo = sorted.last()?.clone();
    let hi = sorted.first()?.clone();
    Some((hi.0, hi.1, lo.0, lo.1))
}

fn timeseries_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let series = map
        .get("series")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let series_count = series.len();
    let total_points: usize = series
        .iter()
        .filter_map(|s| s.get("candles").and_then(|v| v.as_array()).map(|a| a.len()))
        .sum();
    if series_count > 0 {
        out.push(format!("series={series_count}"));
    }
    if total_points > 0 {
        out.push(format!("points={total_points}"));
    }

    if let Some(stats) = map
        .get("analytics")
        .and_then(|a| a.get("stats"))
        .and_then(|v| v.as_object())
    {
        let mut returns: Vec<(String, f64)> = Vec::new();
        let mut vols: Vec<(String, f64)> = Vec::new();
        let mut sharpes: Vec<(String, f64)> = Vec::new();
        for (ticker, statv) in stats {
            let Some(stat) = statv.as_object() else {
                continue;
            };
            if let Some(r) = stat
                .get("total_return")
                .and_then(|v| v.as_f64())
                .or_else(|| stat.get("return_total").and_then(|v| v.as_f64()))
            {
                returns.push((ticker.clone(), r));
            }
            if let Some(v) = stat
                .get("annualized_vol")
                .and_then(|v| v.as_f64())
                .or_else(|| stat.get("vol_annualized").and_then(|v| v.as_f64()))
            {
                vols.push((ticker.clone(), v));
            }
            if let Some(s) = stat
                .get("sharpe_ratio")
                .and_then(|v| v.as_f64())
                .or_else(|| stat.get("sharpe").and_then(|v| v.as_f64()))
            {
                sharpes.push((ticker.clone(), s));
            }
        }
        if let Some((hi_t, hi_v, lo_t, lo_v)) = top_two_by_abs_change(&returns) {
            out.push(format!("best_return={hi_t}:{}", fmt_pct(hi_v)));
            out.push(format!("worst_return={lo_t}:{}", fmt_pct(lo_v)));
        }
        if !vols.is_empty() {
            vols.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.push(format!("highest_vol={}:{}", vols[0].0, fmt_pct(vols[0].1)));
        }
        if !sharpes.is_empty() {
            sharpes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.push(format!("best_sharpe={}:{:.2}", sharpes[0].0, sharpes[0].1));
        }
    }

    if let Some(cm) = map
        .get("analytics")
        .and_then(|a| a.get("correlation_matrix"))
        .and_then(|v| v.as_object())
    {
        let mut pairs: Vec<(String, String, f64)> = Vec::new();
        for (a, rowv) in cm {
            let Some(row) = rowv.as_object() else {
                continue;
            };
            for (b, cv) in row {
                if a >= b {
                    continue;
                }
                if let Some(c) = cv.as_f64() {
                    pairs.push((a.clone(), b.clone(), c));
                }
            }
        }
        if !pairs.is_empty() {
            let avg = pairs.iter().map(|p| p.2).sum::<f64>() / pairs.len() as f64;
            pairs.sort_by(|x, y| x.2.partial_cmp(&y.2).unwrap_or(std::cmp::Ordering::Equal));
            let low = &pairs[0];
            let high = &pairs[pairs.len() - 1];
            out.push(format!("corr_avg={avg:.3}"));
            out.push(format!("corr_max={}-{}:{:.3}", high.0, high.1, high.2));
            out.push(format!("corr_min={}-{}:{:.3}", low.0, low.1, low.2));
        }
    }
    out
}

fn snapshot_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(weights) = map
        .get("analytics")
        .and_then(|a| a.get("market_cap_weights"))
        .and_then(|v| v.as_object())
    {
        let mut w: Vec<(String, f64)> = weights
            .iter()
            .filter_map(|(k, v)| v.as_f64().map(|x| (k.clone(), x)))
            .collect();
        if !w.is_empty() {
            w.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let top3 = w.iter().take(3).map(|x| x.1).sum::<f64>();
            out.push(format!("top_weight={}:{}", w[0].0, fmt_pct(w[0].1)));
            out.push(format!("top3_weight={}", fmt_pct(top3)));
        }
    }
    if let Some(snaps) = map.get("snapshots").and_then(|v| v.as_array()) {
        let mut caps: Vec<(String, f64)> = Vec::new();
        for s in snaps {
            let t = s.get("ticker").and_then(|v| v.as_str()).unwrap_or("?");
            if let Some(cap) = s.get("market_cap").and_then(|v| v.as_f64()) {
                caps.push((t.to_string(), cap));
            }
        }
        if !caps.is_empty() {
            caps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.push(format!("largest_cap={}", caps[0].0));
        }
    }
    out
}

fn fundamentals_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let Some(first) = map
        .get("statements")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
    else {
        return out;
    };
    let rev = first.get("total_revenue").and_then(|v| v.as_f64());
    let gross = first.get("gross_profit").and_then(|v| v.as_f64());
    let op = first.get("operating_income").and_then(|v| v.as_f64());
    let fcf = first.get("free_cash_flow").and_then(|v| v.as_f64());
    let cash = first.get("cash_and_equivalents").and_then(|v| v.as_f64());
    let debt = first.get("total_debt").and_then(|v| v.as_f64());
    if let (Some(g), Some(r)) = (gross, rev) {
        out.push(format!("gross_margin={}", fmt_pct(g / r)));
    }
    if let (Some(o), Some(r)) = (op, rev) {
        out.push(format!("op_margin={}", fmt_pct(o / r)));
    }
    if let (Some(f), Some(r)) = (fcf, rev) {
        out.push(format!("fcf_margin={}", fmt_pct(f / r)));
    }
    if let (Some(c), Some(d)) = (cash, debt) {
        out.push(format!("net_cash={:.1}B", (c - d) / 1e9));
    }
    out
}

fn filings_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let Some(filings) = map.get("filings").and_then(|v| v.as_array()) else {
        return out;
    };
    out.push(format!("filings={}", filings.len()));
    if let Some(first) = filings.first() {
        let form = first.get("form").and_then(|v| v.as_str()).unwrap_or("?");
        let date = first
            .get("filing_date")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        out.push(format!("latest={form}@{date}"));
    }
    out
}

fn news_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let Some(news) = map.get("news").and_then(|v| v.as_array()) else {
        return out;
    };
    out.push(format!("articles={}", news.len()));
    if let Some(first) = news.first() {
        let t = first.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if !t.is_empty() {
            out.push(format!("top_headline={}", truncate_line(t, 50)));
        }
    }
    out
}

fn macro_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let Some(ind) = map.get("indicators").and_then(|v| v.as_array()) else {
        return out;
    };
    let mut changes: Vec<(String, f64)> = Vec::new();
    for x in ind {
        let s = x.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
        if let Some(c) = x.get("change_1y").and_then(|v| v.as_f64()) {
            changes.push((s.to_string(), c));
        }
    }
    out.push(format!("indicators={}", ind.len()));
    if !changes.is_empty() {
        changes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let hi = &changes[0];
        let lo = &changes[changes.len() - 1];
        out.push(format!("max_1y_change={}:{}", hi.0, fmt_pct(hi.1)));
        out.push(format!("min_1y_change={}:{}", lo.0, fmt_pct(lo.1)));
    }
    out
}

fn schedule_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let earnings = map
        .get("earnings")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let macro_n = map
        .get("macro")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    out.push(format!("earnings={earnings}"));
    out.push(format!("macro={macro_n}"));
    if let Some(arr) = map.get("earnings").and_then(|v| v.as_array()) {
        let mut pre = 0usize;
        for e in arr {
            if e.get("time").and_then(|v| v.as_str()) == Some("pre-market") {
                pre += 1;
            }
        }
        if !arr.is_empty() {
            out.push(format!(
                "premarket_share={:.1}%",
                100.0 * pre as f64 / arr.len() as f64
            ));
        }
    }
    out
}

fn prices_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(status) = map.get("status").and_then(|v| v.as_str()) {
        out.push(format!("status={status}"));
    }
    if let Some(prices) = map.get("prices").and_then(|v| v.as_array()) {
        out.push(format!("prices={}", prices.len()));
    }
    if let Some(cands) = map
        .get("disambiguation")
        .and_then(|d| d.get("candidates"))
        .and_then(|v| v.as_array())
    {
        out.push(format!("candidates={}", cands.len()));
    }
    out
}

fn odds_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(sem) = map.get("field_semantics").and_then(|v| v.as_object()) {
        let prob = sem
            .get("probability_scale")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let yes_units = sem
            .get("yes_price_units")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let vol_units = sem
            .get("volume_units")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        out.push(format!(
            "semantics=probability:{prob},yes_price:{yes_units},volume:{vol_units}"
        ));
    }
    let Some(markets) = map.get("markets").and_then(|v| v.as_array()) else {
        return out;
    };
    out.push(format!("markets={}", markets.len()));
    let mut probs = Vec::new();
    let mut spreads = Vec::new();
    let mut top_vol: Option<(String, f64)> = None;
    let mut open_n = 0usize;
    for m in markets {
        if m.get("status").and_then(|v| v.as_str()) == Some("open") {
            open_n += 1;
        }
        if let Some(p) = m.get("probability_yes").and_then(|v| v.as_f64()) {
            probs.push(p);
        }
        let bid = m.get("yes_bid").and_then(|v| v.as_f64());
        let ask = m.get("yes_ask").and_then(|v| v.as_f64());
        if let (Some(b), Some(a)) = (bid, ask) {
            spreads.push(a - b);
        }
        if let Some(vol) = m.get("volume").and_then(|v| v.as_f64()) {
            let t = m
                .get("ticker")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            match &top_vol {
                Some((_, best)) if vol <= *best => {}
                _ => top_vol = Some((t, vol)),
            }
        }
    }
    if !markets.is_empty() {
        out.push(format!(
            "open_share={:.1}%",
            100.0 * open_n as f64 / markets.len() as f64
        ));
    }
    if !probs.is_empty() {
        let avg = probs.iter().sum::<f64>() / probs.len() as f64;
        out.push(format!("prob_yes_avg={avg:.3}"));
    }
    if !spreads.is_empty() {
        let avg = spreads.iter().sum::<f64>() / spreads.len() as f64;
        let max = spreads
            .iter()
            .copied()
            .fold(f64::MIN, |a, b| if b > a { b } else { a });
        out.push(format!("spread_avg={avg:.3}"));
        out.push(format!("spread_max={max:.3}"));
    }
    if let Some((ticker, vol)) = top_vol {
        out.push(format!("top_volume={ticker}:{vol:.0}"));
    }
    out
}

fn options_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let metrics = map.get("metrics").and_then(|v| v.as_object());
    if let Some(m) = metrics {
        if let Some(v) = m.get("put_call_ratio_volume").and_then(|v| v.as_f64()) {
            out.push(format!("pcr_vol={v:.2}"));
        }
        if let Some(v) = m.get("put_call_ratio_oi").and_then(|v| v.as_f64()) {
            out.push(format!("pcr_oi={v:.2}"));
        }
        if let Some(v) = m.get("atm_iv_call").and_then(|v| v.as_f64()) {
            out.push(format!("atm_iv_call={:.2}%", v));
        }
        if let Some(v) = m.get("atm_iv_put").and_then(|v| v.as_f64()) {
            out.push(format!("atm_iv_put={:.2}%", v));
        }
        if let Some(v) = m.get("max_pain").and_then(|v| v.as_f64()) {
            out.push(format!("max_pain={v:.2}"));
        }
    }
    out
}

fn search_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let results = map
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    out.push(format!("results={}", results.len()));
    if let Some(first) = results.first() {
        let sym = first.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
        let typ = first
            .get("asset_type")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        out.push(format!("top_match={sym}:{typ}"));
    }
    if let Some(ms) = map.get("macro_suggestions").and_then(|v| v.as_array()) {
        out.push(format!("macro_suggestions={}", ms.len()));
    }
    out
}

fn sync_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(n) = map.get("total_markets").and_then(|v| v.as_i64()) {
        out.push(format!("total_markets={n}"));
    }
    if let Some(n) = map.get("total_events").and_then(|v| v.as_i64()) {
        out.push(format!("total_events={n}"));
    }
    if let Some(sources) = map.get("sources").and_then(|v| v.as_array()) {
        let ok = sources
            .iter()
            .filter(|s| s.get("ok").and_then(|v| v.as_bool()) == Some(true))
            .count();
        out.push(format!("sources_ok={ok}/{}", sources.len()));
    }
    if let Some(analysis) = map.get("analysis").and_then(|v| v.as_object()) {
        if let Some(v_cents) = analysis.get("total_volume").and_then(|v| v.as_i64()) {
            out.push(format!("total_volume_cents={v_cents}"));
            out.push(format!("total_volume_usd={:.2}", v_cents as f64 / 100.0));
        }
        if let Some(v_pct) = analysis
            .get("extreme_prob_volume_share_pct")
            .and_then(|v| v.as_f64())
        {
            out.push(format!("extreme_prob_volume_share_pct={v_pct:.2}"));
        }
    }
    out
}

fn web_search_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let hits = map
        .get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    out.push(format!("hits={}", hits.len()));
    if let Some(first) = hits.first() {
        if let Some(title) = first.get("title").and_then(|v| v.as_str()) {
            out.push(format!("top_hit={}", truncate_line(title, 45)));
        }
        if let Some(url) = first.get("url").and_then(|v| v.as_str()) {
            out.push(format!("top_domain={}", domain_of(url)));
        }
    }
    out
}

fn web_read_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(title) = map.get("title").and_then(|v| v.as_str()) {
        out.push(format!("title={}", truncate_line(title, 45)));
    }
    if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
        out.push(format!("chars={}", text.chars().count()));
        out.push(format!("words={}", text.split_whitespace().count()));
    }
    out
}

fn web_extract_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(n) = map.get("word_count").and_then(|v| v.as_i64()) {
        out.push(format!("word_count={n}"));
    }
    if let Some(b) = map.get("bullets").and_then(|v| v.as_array()) {
        out.push(format!("bullets={}", b.len()));
    }
    out
}

fn web_crawl_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if let Some(n) = map.get("pages_crawled").and_then(|v| v.as_i64()) {
        out.push(format!("pages={n}"));
    }
    if let Some(ms) = map.get("duration_ms").and_then(|v| v.as_i64()) {
        out.push(format!("duration_ms={ms}"));
    }
    if let Some(pages) = map
        .get("pages")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
    {
        if let Some(title) = pages.get("title").and_then(|v| v.as_str()) {
            out.push(format!("first_title={}", truncate_line(title, 45)));
        }
    }
    out
}

fn domain_of(url: &str) -> String {
    let mut s = url;
    if let Some(x) = s.strip_prefix("https://") {
        s = x;
    } else if let Some(x) = s.strip_prefix("http://") {
        s = x;
    }
    s.split('/').next().unwrap_or(s).to_string()
}

