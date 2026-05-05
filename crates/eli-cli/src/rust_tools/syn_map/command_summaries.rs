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

    let path = extract_eli_tool_path(command).unwrap_or_default();
    let summary_cap = if path.len() >= 2 && path[0] == "finance" && path[1] == "forex" {
        12
    } else {
        5
    };
    let command_parts = command_summary_parts(command, value, summary_cap);
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
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "fundamentals" {
        out.extend(fundamentals_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && (path[1] == "filings" || path[1] == "sec")
    {
        out.extend(filings_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "macro" {
        out.extend(macro_summary_parts(value));
    } else if path.len() >= 2 && path[0] == "finance" && path[1] == "forex" {
        out.extend(forex_summary_parts(value));
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
        }
        if let Some((hi_t, hi_v, lo_t, lo_v)) = top_two_by_abs_change(&returns) {
            out.push(format!("best_return={hi_t}:{}", fmt_pct(hi_v)));
            out.push(format!("worst_return={lo_t}:{}", fmt_pct(lo_v)));
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

fn forex_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };

    if let Some(basket) = map.get("basket").and_then(|v| v.as_str()) {
        out.push(format!("basket={basket}"));
    }

    if let Some(n) = map.get("pair_count").and_then(|v| v.as_u64()) {
        out.push(format!("pairs={n}"));
    } else if let Some(arr) = map.get("pairs").and_then(|v| v.as_array()) {
        out.push(format!("pairs={}", arr.len()));
    }

    if let Some(score) = map
        .get("summary")
        .and_then(|v| v.get("usd_strength_score_pct"))
        .and_then(|v| v.as_f64())
    {
        out.push(format!("usd_strength={score:.2}%"));
    }

    if let Some(pair) = map
        .get("summary")
        .and_then(|v| v.get("strongest_usd_pair"))
        .and_then(|v| v.as_str())
    {
        out.push(format!("strongest_usd={pair}"));
    }

    if let Some(pair) = map
        .get("summary")
        .and_then(|v| v.get("weakest_usd_pair"))
        .and_then(|v| v.as_str())
    {
        out.push(format!("weakest_usd={pair}"));
    }

    if let Some(arr) = map.get("comparison_deltas").and_then(|v| v.as_array()) {
        out.push(format!("comparisons={}", arr.len()));
        if let Some(first) = arr.first() {
            let ts = first.get("as_of").and_then(|v| v.as_str()).unwrap_or("?");
            if let Some(delta) = first
                .get("delta_usd_strength_pct")
                .and_then(|v| v.as_f64())
            {
                out.push(format!("vs_{ts}={delta:.2}%"));
            }
        }
    }

    if let Some(delta) = map.get("delta_context").and_then(|v| v.as_object()) {
        let prev = delta
            .get("previous_synced_at")
            .and_then(|v| v.as_str())
            .or_else(|| delta.get("previous_as_of").and_then(|v| v.as_str()))
            .unwrap_or("?");
        let cur = delta
            .get("current_synced_at")
            .and_then(|v| v.as_str())
            .or_else(|| delta.get("current_as_of").and_then(|v| v.as_str()))
            .unwrap_or("?");
        let prev_as_of = delta
            .get("previous_as_of")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let cur_as_of = delta
            .get("current_as_of")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let compared = delta
            .get("compared_pairs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let changed = delta
            .get("changed_pairs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        out.push(format!("delta_window={prev}->{cur}"));
        out.push(format!("as_of_window={prev_as_of}->{cur_as_of}"));
        out.push(format!("pair_delta={changed}/{compared}"));
        if let Some(top) = delta
            .get("top_pair_deltas")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
        {
            let pair = top.get("pair").and_then(|v| v.as_str()).unwrap_or("?");
            let move_pct = top
                .get("delta_usd_change_pct")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            out.push(format!("top_pair_delta={pair}:{move_pct:.2}%"));
        }
    }

    if let Some(bench) = map.get("usd_benchmark").and_then(|v| v.as_object()) {
        let symbol = bench
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("DXY");
        if let Some(change) = bench.get("change_pct").and_then(|v| v.as_f64()) {
            out.push(format!("{symbol}={change:.2}%"));
        }
    }

    if let Some(arr) = map.get("biggest_daily_usd_moves").and_then(|v| v.as_array()) {
        out.push(format!("top_hits={}", arr.len()));
        if let Some(first) = arr.first() {
            let pair = first.get("pair").and_then(|v| v.as_str()).unwrap_or("?");
            let date = first.get("date").and_then(|v| v.as_str()).unwrap_or("?");
            let impact = first
                .get("usd_impact_pct")
                .and_then(|v| v.as_f64())
                .or_else(|| first.get("daily_change_pct").and_then(|v| v.as_f64()))
                .unwrap_or(0.0);
            out.push(format!("largest_hit={pair}@{date}:{impact:.2}%"));
        }
    }

    if let Some(arr) = map
        .get("summary")
        .and_then(|s| s.get("hot_dates"))
        .and_then(|v| v.as_array())
    {
        if let Some(first) = arr.first() {
            let date = first.get("date").and_then(|v| v.as_str()).unwrap_or("?");
            let n = first.get("move_count").and_then(|v| v.as_u64()).unwrap_or(0);
            out.push(format!("hottest_date={date}:{n}"));
        }
    }

    if let Some(event) = map.get("event_window").and_then(|v| v.as_object()) {
        if let Some(ts) = event.get("event_at").and_then(|v| v.as_str()) {
            out.push(format!("event_at={ts}"));
        }
        if let Some(shift) = event
            .get("shift_usd_strength_pct")
            .and_then(|v| v.as_f64())
        {
            out.push(format!("event_shift={shift:.2}%"));
        }
        if let Some(arr) = event.get("session_attribution").and_then(|v| v.as_array()) {
            if let Some(first) = arr.first() {
                let session = first.get("session").and_then(|v| v.as_str()).unwrap_or("?");
                let n = first.get("move_count").and_then(|v| v.as_u64()).unwrap_or(0);
                out.push(format!("dominant_session={session}:{n}"));
            }
        }
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
    if let Some(delta) = map.get("delta").and_then(|v| v.as_object()) {
        if let Some(n) = delta.get("changed_markets").and_then(|v| v.as_i64()) {
            out.push(format!("delta_changed_markets={n}"));
        }
        if let Some(n) = delta.get("new_markets").and_then(|v| v.as_i64()) {
            out.push(format!("delta_new_markets={n}"));
        }
        if let Some(n) = delta.get("removed_markets").and_then(|v| v.as_i64()) {
            out.push(format!("delta_removed_markets={n}"));
        }
        if let Some(arr) = delta
            .get("top_probability_moves")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
        {
            let ticker = arr.get("ticker").and_then(|v| v.as_str()).unwrap_or("?");
            let pp = arr
                .get("probability_delta_pct_points")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            out.push(format!("delta_top_prob_move={ticker}:{pp:+.2}pp"));
        }
    }
    out
}

fn web_search_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    let mut top_hit: Option<String> = None;
    let items = map
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    out.push(format!("items={}", items.len()));
    if let Some(mode) = map.get("mode").and_then(|v| v.as_str()) {
        out.push(format!("mode={mode}"));
    }
    if let Some(first) = items.first() {
        if let Some(title) = first.get("title").and_then(|v| v.as_str()) {
            top_hit = Some(format!("top_hit={}", truncate_line(title, 45)));
        }
        if let Some(domain) = first.get("domain").and_then(|v| v.as_str()) {
            out.push(format!("top_domain={domain}"));
        } else if let Some(url) = first.get("url").and_then(|v| v.as_str()) {
            out.push(format!("top_domain={}", domain_of(url)));
        }
    }
    if let Some(stats) = map.get("stats").and_then(|v| v.as_object()) {
        if let Some(n) = stats.get("probed_items").and_then(|v| v.as_i64()) {
            out.push(format!("probed={n}"));
        }
    }
    if let Some(delta) = map.get("run_delta").and_then(|v| v.as_object()) {
        let new_count = delta
            .get("new_urls")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len())
            .unwrap_or(0);
        let dropped_count = delta
            .get("dropped_urls")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len())
            .unwrap_or(0);
        if new_count > 0 || dropped_count > 0 {
            out.push(format!("delta=+{new_count}/-{dropped_count}"));
        }
    }
    if let Some(hit) = top_hit {
        out.push(hit);
    }
    out
}

fn web_read_summary_parts(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    if map
        .get("mode")
        .and_then(|v| v.as_str())
        .is_some_and(|m| m.eq_ignore_ascii_case("batch"))
    {
        if let Some(n) = map.get("completed").and_then(|v| v.as_i64()) {
            out.push(format!("completed={n}"));
        }
        if let Some(n) = map.get("success_count").and_then(|v| v.as_i64()) {
            out.push(format!("success={n}"));
        }
        if let Some(n) = map.get("partial_count").and_then(|v| v.as_i64()) {
            out.push(format!("partial={n}"));
        }
        if let Some(n) = map.get("blocked_count").and_then(|v| v.as_i64()) {
            out.push(format!("blocked={n}"));
        }
        if let Some(n) = map.get("error_count").and_then(|v| v.as_i64()) {
            out.push(format!("error={n}"));
        }
        return out;
    }

    if let Some(status) = map.get("fetch_status").and_then(|v| v.as_str()) {
        out.push(format!("status={status}"));
    }
    if let Some(reason) = map.get("blocked_reason").and_then(|v| v.as_str()) {
        out.push(format!("blocked_reason={reason}"));
    }
    if let Some(title) = map.get("title").and_then(|v| v.as_str()) {
        if !title.trim().is_empty() {
            out.push(format!("title={}", truncate_line(title, 45)));
        }
    }
    if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
        let chars = text.chars().count();
        if chars > 0 {
            out.push(format!("chars={chars}"));
            out.push(format!("words={}", text.split_whitespace().count()));
        }
    }
    if let Some(attempts) = map.get("attempts").and_then(|v| v.as_array()) {
        out.push(format!("attempts={}", attempts.len()));
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
