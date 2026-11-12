use futures::future::join_all;
use std::collections::{BTreeMap, HashSet};

const KALSHI_CANDLESTICKS_URL: &str =
    "https://api.elections.kalshi.com/trade-api/v2/markets/candlesticks";
const POLYMARKET_GAMMA_URL: &str = "https://gamma-api.polymarket.com";

/// Collapse redundant candle fields in a serialized timeseries response:
/// - when o==h==l==c (macro point series, carried-forward prediction-market
///   buckets) the o/h/l fields are omitted — a candle with only `c` means
///   "no intra-bucket movement", which also makes thin-market carry-forward
///   stretches visible at a glance;
/// - `v` is dropped when zero across the whole series (indices like ^VIX
///   report no real volume — repeating `"v": 0.0` per candle is dead weight).
///   Exchange venues are the exception: their volume is real, so a series
///   that is zero throughout means "no trade in the whole window" and stays.
fn collapse_point_candles(value: &mut serde_json::Value) {
    let Some(series) = value.get_mut("series").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for s in series {
        let exchange_volume = matches!(
            s.get("source").and_then(|v| v.as_str()),
            Some("binance") | Some("binance.us")
        );
        let Some(candles) = s.get_mut("candles").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        let all_zero_vol = !exchange_volume
            && candles.iter().all(|c| {
                c.get("v")
                    .map_or(true, |v| v.as_f64().unwrap_or(0.0) == 0.0)
            });
        for c in candles.iter_mut() {
            let Some(obj) = c.as_object_mut() else { continue };
            // Never collapse a candle with real volume: a thin stock can print
            // o==h==l==c on a 1m bar with thousands of shares traded, and that
            // is real activity, not a carried-forward placeholder.
            let traded = obj
                .get("v")
                .and_then(|v| v.as_f64())
                .is_some_and(|v| v > 0.0);
            if !traded {
                if let Some(close) = obj.get("c").and_then(|v| v.as_f64()) {
                    let all_eq = ["o", "h", "l"]
                        .iter()
                        .all(|k| obj.get(*k).and_then(|v| v.as_f64()) == Some(close));
                    if all_eq {
                        obj.remove("o");
                        obj.remove("h");
                        obj.remove("l");
                    }
                }
            }
            if all_zero_vol {
                obj.remove("v");
            }
        }
    }
}

/// Stages that describe a series that was served, not a failure. They go to
/// `notices` and never count against `status` or the failed-ticker count.
fn is_timeseries_notice_stage(stage: Option<&str>) -> bool {
    matches!(
        stage,
        Some("granularity_downgrade")
            | Some("data_quality")
            | Some("ibkr_fallback")
            | Some("event_legs")
            | Some("kalshi_event_legs")
    )
}

/// The tickers one call asked for, spelled as the caller wrote them, in
/// order and without repeats: preset tickers, explicit tickers, prediction
/// markets.
fn requested_timeseries_tickers(
    preset: &[String],
    explicit: &[String],
    markets: &[PredictionMarketRequest],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let all = preset
        .iter()
        .chain(explicit.iter())
        .map(|t| t.as_str())
        .chain(markets.iter().map(|m| m.market.as_str()));
    for t in all {
        let t = t.trim();
        if !t.is_empty() && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Give returned tickers back the spelling the caller used. The core
/// upper-cases a ticker before it fetches, so `aapl` or `KF:btc` came back
/// as `AAPL` and `KF:BTC`, and a caller matching on its own input found
/// nothing. Only case is undone: `vix` still comes back as `^VIX`, and a
/// bare Polymarket id as `POLYMARKET:<id>:YES`.
fn restore_requested_spelling(
    resp: &mut eli_core::finance::TimeseriesResponse,
    requested: &[String],
) {
    let fix = |t: &mut String| {
        if requested.iter().any(|r| r == t) {
            return;
        }
        if let Some(r) = requested.iter().find(|r| r.eq_ignore_ascii_case(t)) {
            *t = r.clone();
        }
    };
    for s in &mut resp.series {
        fix(&mut s.ticker);
    }
    for e in resp.errors.iter_mut().flatten() {
        fix(&mut e.ticker);
    }
    for t in &mut resp.tickers {
        fix(t);
    }
    for t in resp.valid_tickers.iter_mut().flatten() {
        fix(t);
    }
}

/// Every requested ticker must come back as a series or as an error. One
/// that fell through both (a bucket that returned its siblings and skipped
/// it) gets a `silent_drop` error, so the caller is told. Case is ignored:
/// the returned spelling can differ from the requested one.
fn append_silent_drops(
    resp: &mut eli_core::finance::TimeseriesResponse,
    requested: &[String],
    granularity: eli_core::finance::Span,
) {
    // A returned ticker often differs from the input (906975 comes back as
    // POLYMARKET:906975:YES, IBKR:FUT:CL:NYMEX as FUT:CL:NYMEX, vix as ^VIX),
    // so a requested ticker is accounted for when it equals a returned one,
    // is part of one, or contains one.
    let returned: Vec<String> = resp
        .series
        .iter()
        .map(|s| s.ticker.as_str())
        .chain(resp.errors.iter().flatten().map(|e| e.ticker.as_str()))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    let dropped: Vec<String> = requested
        .iter()
        .filter(|req| {
            let r = req.to_ascii_lowercase();
            !returned
                .iter()
                .any(|t| *t == r || t.contains(r.as_str()) || r.contains(t.as_str()))
        })
        .cloned()
        .collect();
    if dropped.is_empty() {
        return;
    }
    // Give the caller a reason it can act on.
    let window_days = (resp.end - resp.start).num_days();
    let intraday = granularity.approx_duration().num_seconds() < 86_400;
    let drop_errors = dropped.into_iter().map(|t| {
        let looks_like_pm_slug = t == t.to_ascii_lowercase()
            && t.contains('-')
            && !t.contains('=')
            && t.chars().any(|c| c.is_ascii_alphabetic());
        let message = if looks_like_pm_slug {
            "looks like a Polymarket slug — slugs don't join here; use the numeric market id from finance_odds markets[].ticker".to_string()
        } else if intraday && window_days <= 3 {
            "no data in window — a short intraday range can fall entirely on a weekend/holiday; widen --range (e.g. 5d) or use --granularity 1d".to_string()
        } else {
            "no provider returned data for this ticker in the requested window".to_string()
        };
        eli_core::finance::TimeseriesError {
            ticker: t,
            stage: Some("silent_drop".to_string()),
            message,
        }
    });
    resp.errors.get_or_insert_with(Vec::new).extend(drop_errors);
}

/// How many of the requested tickers failed: (failed, out of). A ticker
/// failed when a real (non-notice) error names it, however many errors do.
/// An error label that matches no requested ticker counts once on both
/// sides, so the first number can never pass the second.
fn count_failed_tickers(
    real_errors: &[eli_core::finance::TimeseriesError],
    requested: &[String],
) -> (usize, usize) {
    let mut keys: Vec<String> = Vec::new();
    for r in requested {
        let k = r.trim().to_ascii_lowercase();
        if !k.is_empty() && !keys.contains(&k) {
            keys.push(k);
        }
    }
    let mut failed: HashSet<usize> = HashSet::new();
    let mut stray: HashSet<String> = HashSet::new();
    for e in real_errors {
        // A whole-bucket failure can name several tickers in one entry.
        for label in e.ticker.split(',') {
            let label = label.trim().to_ascii_lowercase();
            let hit = keys.iter().position(|k| *k == label).or_else(|| {
                // KALSHI:KXFED-26DEC-T3.50:YES for a request of
                // KXFED-26DEC-T3.50, ^VIX for vix: take the longest
                // requested ticker the label contains or is part of.
                if label.is_empty() {
                    return None;
                }
                keys.iter()
                    .enumerate()
                    .filter(|(_, k)| label.contains(k.as_str()) || k.contains(label.as_str()))
                    .max_by_key(|(_, k)| k.len())
                    .map(|(i, _)| i)
            });
            match hit {
                Some(i) => {
                    failed.insert(i);
                }
                None => {
                    stray.insert(label);
                }
            }
        }
    }
    (failed.len() + stray.len(), keys.len() + stray.len())
}

/// A timeseries response in the shape every output shares.
struct ShapedTimeseries {
    value: serde_json::Value,
    /// None when every ticker returned, else "partial" or "error".
    status: Option<String>,
    sources: Vec<String>,
    series: usize,
    errors: usize,
    notices: usize,
    /// The error object's message, when the document carries one.
    error_message: Option<String>,
}

/// Final shaping shared by stdout and `--out`:
/// - `sources` = distinct series[].source — the truth on mixed calls (the
///   top-level `provider` reflects only the dispatch-time main bucket, so it
///   is dropped unless it names the single source);
/// - informational entries move from `errors` to `notices`;
/// - `status` recomputed as a real aggregate: absent = every ticker
///   returned, "partial" = some failed, "error" = nothing returned;
/// - the `error` object is absent when nothing failed; a partial result
///   carries one that counts the tickers that failed out of the tickers
///   requested.
fn shape_timeseries_response(
    resp: &eli_core::finance::TimeseriesResponse,
    requested: &[String],
) -> Result<ShapedTimeseries> {
    let mut resp = resp.clone();
    let mut distinct: Vec<String> = Vec::new();
    for s in &resp.series {
        if let Some(src) = &s.source {
            if !distinct.iter().any(|x| x == src) {
                distinct.push(src.clone());
            }
        }
    }
    distinct.sort();
    resp.sources = distinct;
    // Informational entries (granularity downgrades, data-quality caveats,
    // transparent fallbacks) are notices, not failures: they must not flip
    // status to "partial" and shouldn't drown real errors in the same list.
    let mut notices: Vec<eli_core::finance::TimeseriesError> = Vec::new();
    if let Some(errs) = resp.errors.take() {
        let mut real = Vec::new();
        for e in errs {
            if is_timeseries_notice_stage(e.stage.as_deref()) {
                notices.push(e);
            } else {
                real.push(e);
            }
        }
        if !real.is_empty() {
            resp.errors = Some(real);
        }
    }
    let err_count = resp.errors.as_ref().map_or(0, |e| e.len());
    if resp.series.is_empty() {
        // Nothing returned: the error object that came with it stands. When
        // none did (each bucket failed by itself), the document still gets
        // one, so "error" status always comes with a message.
        if err_count > 0 && resp.error.is_none() {
            resp.error = Some(eli_core::finance::ToolErrorInfo {
                error: "TickerFetchFailed".to_string(),
                message: "All tickers failed to fetch timeseries data.".to_string(),
                hint: Some("errors[] gives the reason for each ticker.".to_string()),
                debug: None,
            });
        }
    } else if err_count == 0 {
        // The core counts every entry as a failed ticker, so a response whose
        // only entries were notices arrives with "1 of 1 tickers failed; 1
        // series returned". Nothing failed: drop the error object with them.
        resp.error = None;
        resp.valid_tickers = None;
    } else {
        // Some returned, some failed. The core writes its count for one
        // provider bucket, with notices counted as failures and the caller's
        // other buckets unknown to it; the merge paths drop that object. So
        // count here, over the whole call, and always say it.
        let (failed, out_of) =
            count_failed_tickers(resp.errors.as_deref().unwrap_or(&[]), requested);
        resp.error = Some(eli_core::finance::ToolErrorInfo {
            error: "PartialTickerFetchFailed".to_string(),
            message: format!(
                "{failed} of {out_of} tickers failed; {} series returned.",
                resp.series.len()
            ),
            hint: Some("errors[] gives the reason for each failed ticker.".to_string()),
            debug: None,
        });
        resp.valid_tickers = Some(resp.series.iter().map(|s| s.ticker.clone()).collect());
    }
    // An error object with no series is an error even when it came without
    // per-ticker entries; `status` must not read as success next to it.
    resp.status = if resp.series.is_empty() && (err_count > 0 || resp.error.is_some()) {
        Some("error".to_string())
    } else if err_count > 0 {
        Some("partial".to_string())
    } else {
        None
    };
    let mut value = serde_json::to_value(&resp).context("serialize response")?;
    if !notices.is_empty() {
        value["notices"] = serde_json::to_value(&notices).context("serialize notices")?;
    }
    // The dispatch-time `provider` lies on mixed calls (any FRED fallback —
    // even for a bare unknown ticker — used to flip it to "fred" over three
    // healthy yahoo series). `sources` is the truth; drop `provider` whenever
    // it doesn't match a single-source reality.
    let single_source_matches = resp.sources.len() == 1
        && value
            .get("provider")
            .and_then(|v| v.as_str())
            .is_some_and(|p| p.eq_ignore_ascii_case(&resp.sources[0]));
    if !single_source_matches {
        if let Some(obj) = value.as_object_mut() {
            obj.remove("provider");
        }
    }
    Ok(ShapedTimeseries {
        value,
        status: resp.status,
        sources: resp.sources,
        series: resp.series.len(),
        errors: err_count,
        notices: notices.len(),
        error_message: resp.error.map(|e| e.message),
    })
}

/// Stdout: the shaped document, then
/// - point-candle collapse + all-zero volume drop;
/// - floats rounded to 7 significant digits (kills f32 feed noise like
///   747.4000244140625);
/// - pretty JSON only at a terminal; compact for pipes/agents.
fn emit_timeseries_stdout(
    resp: &eli_core::finance::TimeseriesResponse,
    requested: &[String],
) -> Result<()> {
    let mut shaped = shape_timeseries_response(resp, requested)?;
    collapse_point_candles(&mut shaped.value);
    round_json_floats(&mut shaped.value);
    let out = json_to_stdout_string(&shaped.value)?;
    audit_set_summary(serde_json::json!({
        "series": shaped.series,
        "sources": shaped.sources,
        "status": shaped.status,
        "bytes": out.len(),
    }));
    println!("{out}");
    Ok(())
}

/// `--out`: the shaped document goes to the file with every candle field and
/// full number precision (only stdout collapses flat candles and rounds).
/// Stdout gets a one-line receipt that carries the document's status, so a
/// pull where every ticker failed cannot pass for a good one: `ok` is true
/// when the file was written and at least one series came back.
fn write_timeseries_out(
    out_path: PathBuf,
    resp: &eli_core::finance::TimeseriesResponse,
    requested: &[String],
    meta_args: &[String],
) -> Result<()> {
    let shaped = shape_timeseries_response(resp, requested)?;
    let json = serde_json::to_string_pretty(&shaped.value).context("serialize response")?;
    let wr = write_json_text_with_meta(
        out_path,
        &shaped.value,
        &json,
        "finance.timeseries",
        meta_args,
    )?;
    let status = shaped.status.as_deref().unwrap_or("ok");
    let quoted = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    let mut line = format!(
        "{{\"ok\":{},\"path\":{},\"meta_path\":{},\"status\":{},\"series\":{},\"errors\":{},\"notices\":{}",
        status != "error",
        quoted(&wr.out_path.display().to_string()),
        quoted(&wr.meta_path.display().to_string()),
        quoted(status),
        shaped.series,
        shaped.errors,
        shaped.notices,
    );
    if status == "error" {
        if let Some(message) = &shaped.error_message {
            line.push_str(&format!(",\"error\":{}", quoted(message)));
        }
    }
    line.push_str(&format!(
        ",\"cache\":{}}}",
        serde_json::to_string(&resp.cache).unwrap_or_else(|_| "null".to_string())
    ));
    println!("{line}");
    Ok(())
}

async fn cmd_finance_timeseries(args: FinanceTimeseriesArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    // Expand --preset into tickers (merged with any explicit --ticker values).
    let mut preset_tickers: Vec<String> = Vec::new();
    let preset_name = args.preset.as_deref().map(|s| s.trim().to_ascii_lowercase());
    // preset=list (or presets/help) → return the self-documenting preset catalog and exit.
    // This is the discovery path: presets were previously only learnable from the tool desc.
    if matches!(preset_name.as_deref(), Some("list") | Some("presets") | Some("help")) {
        emit_tool_payload(&list_timeseries_presets())?;
        return Ok(());
    }
    if let Some(ref preset) = preset_name {
        preset_tickers = expand_timeseries_preset(preset)?;
    }

    // Auto-detect prediction market tickers mixed in with stock/FRED tickers.
    // KX* / KALSHI:* → Kalshi, pure numeric (6+ digits) / POLYMARKET:* → Polymarket.
    let explicit_odds_provider = args.odds_provider.clone();
    let explicit_odds_market = args.odds_market.clone();
    let mut prediction_markets = Vec::new();
    let mut preset_stock_tickers = Vec::new();
    for t in &preset_tickers {
        push_timeseries_input(t, &mut prediction_markets, &mut preset_stock_tickers);
    }
    let mut explicit_stock_tickers = Vec::new();
    for t in &args.tickers {
        push_timeseries_input(t, &mut prediction_markets, &mut explicit_stock_tickers);
    }

    if let Some(odds_req) = parse_optional_prediction_market_request(
        explicit_odds_provider.as_deref(),
        explicit_odds_market.as_deref(),
        &args.odds_side,
    )? {
        push_prediction_market_request(&mut prediction_markets, odds_req);
    }

    let mut tickers = explicit_stock_tickers;
    if let Some(ref path) = args.tickers_file {
        let raw = std::fs::read_to_string(&path).context("read tickers_file")?;
        for line in raw.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            push_timeseries_input(t, &mut prediction_markets, &mut tickers);
        }
    }

    // Expand CURVE:<commodity> tickers into individual contract month tickers.
    // e.g. "CURVE:oil" → CLK26.NYM,CLM26.NYM,CLN26.NYM,...
    let mut expanded_tickers: Vec<String> = Vec::new();
    for t in &tickers {
        if let Some(commodity) = t.strip_prefix("CURVE:").or_else(|| t.strip_prefix("curve:")) {
            let months = 12usize;
            if commodity == "all" {
                // Expand all commodities
                for (aliases, _, _) in list_commodities() {
                    let name = aliases.split(" / ").next().unwrap_or(aliases);
                    if let Some(spec) = lookup_commodity(name) {
                        for (ticker, _label) in generate_futures_tickers(&spec, months) {
                            expanded_tickers.push(ticker);
                        }
                    }
                }
            } else if let Some(spec) = lookup_commodity(commodity) {
                let futures = generate_futures_tickers(&spec, months);
                for (ticker, _label) in futures {
                    expanded_tickers.push(ticker);
                }
            } else {
                expanded_tickers.push(t.clone());
            }
        } else {
            expanded_tickers.push(t.clone());
        }
    }
    let tickers = expanded_tickers;

    let mut range = eli_core::finance::Span::parse(&args.range)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --range")?;
    let granularity = eli_core::finance::Span::parse(&args.granularity)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --granularity")?;

    let mut as_of = match args.as_of.as_ref() {
        Some(raw) => Some(
            eli_core::finance::parse_as_of(raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --as-of")?,
        ),
        None => None,
    };

    match (args.start.as_deref(), args.end.as_deref()) {
        (Some(start_raw), Some(end_raw)) => {
            if args.as_of.is_some() {
                anyhow::bail!("--as-of cannot be combined with --start/--end");
            }
            let start_dt = parse_window_start(start_raw).context("parse --start")?;
            let end_dt = eli_core::finance::parse_as_of(end_raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --end")?;
            if end_dt <= start_dt {
                anyhow::bail!("--end must be strictly after --start");
            }
            let delta = end_dt.signed_duration_since(start_dt);
            let minutes = ((delta.num_seconds() + 59) / 60).max(1);
            range = eli_core::finance::Span::parse(&format!("{minutes}min"))
                .map_err(|e| anyhow::anyhow!(e))
                .context("derive range from --start/--end")?;
            as_of = Some(end_dt);
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("--start and --end must be used together");
        }
        (None, None) => {}
    }

    if tickers.is_empty() && preset_stock_tickers.is_empty() && prediction_markets.is_empty() {
        anyhow::bail!("at least one ticker is required");
    }
    // What the caller asked for, in its own spelling: the reference for the
    // returned ticker names, the dropped-ticker check and the failure count.
    let requested =
        requested_timeseries_tickers(&preset_stock_tickers, &tickers, &prediction_markets);

    // Standalone prediction market mode: only prediction-market tickers. A
    // preset counts as tickers, so `--preset crypto --tickers <market>` takes
    // the main path and returns both.
    if tickers.is_empty() && preset_stock_tickers.is_empty() {
        if !prediction_markets.is_empty() {
            let now = chrono::Utc::now();
            let end = as_of.unwrap_or(now).min(now);
            let start = end
                .checked_sub_signed(range.approx_duration())
                .ok_or_else(|| anyhow::anyhow!("range underflow"))?;
            // Same --max-points-per-ticker guard the core applies to every other
            // provider (this standalone path never reaches the core). Matters
            // now that Kalshi serves 1m bars: 1y at 1m is 525k points.
            let cap_error = args.max_points_per_ticker.map(|v| v.max(2)).and_then(|max_points| {
                let step = granularity.approx_duration().num_seconds().max(1);
                let approx_points = ((end - start).num_seconds() / step).max(1) as usize + 1;
                (approx_points > max_points).then(|| {
                    format!(
                        "requested ~{approx_points} points per ticker exceeds limit {max_points}; use a coarser --granularity for this window, or reduce range"
                    )
                })
            });
            let pm_meta_args = [
                format!("range={}", args.range),
                format!("granularity={}", args.granularity),
                format!("prediction_markets={}", prediction_markets.len()),
            ];
            let (market_series, market_errors) = match cap_error {
                Some(message) => (
                    Vec::new(),
                    prediction_markets
                        .iter()
                        .map(|req| eli_core::finance::TimeseriesError {
                            ticker: prediction_market_request_label(req),
                            stage: Some("max_points".to_string()),
                            message: message.clone(),
                        })
                        .collect(),
                ),
                None => {
                    fetch_prediction_market_series_batch(&prediction_markets, start, end, granularity)
                        .await
                }
            };
            if market_series.is_empty() {
                // Emit the same JSON error envelope every other provider path
                // uses (a bare anyhow bail printed plain text + exit 1, so
                // callers needed two different error parsers).
                let err_msg = market_errors
                    .first()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "no usable prediction market series found".to_string());
                let resp = eli_core::finance::TimeseriesResponse {
                    provider: prediction_market_response_provider(&prediction_markets),
                    sources: Vec::new(),
                    tickers: prediction_markets.iter().map(|p| p.market.clone()).collect(),
                    granularity,
                    range,
                    start,
                    end,
                    generated_at: now,
                    series: Vec::new(),
                    status: Some("error".to_string()),
                    error: Some(eli_core::finance::ToolErrorInfo {
                        error: "TickerFetchFailed".to_string(),
                        message: err_msg,
                        hint: None,
                        debug: None,
                    }),
                    errors: if market_errors.is_empty() {
                        None
                    } else {
                        Some(market_errors)
                    },
                    valid_tickers: None,
                    analytics: None,
                    cache: None,
                };
                return match args.out {
                    Some(out_path) => {
                        write_timeseries_out(out_path, &resp, &requested, &pm_meta_args)
                    }
                    None => emit_timeseries_stdout(&resp, &requested),
                };
            }
            let series = market_series;
            let analytics =
                eli_core::finance::build_timeseries_analytics(&series, granularity);
            let provider = prediction_market_response_provider(&prediction_markets);
            let tickers: Vec<String> = series.iter().map(|s| s.ticker.clone()).collect();
            let mut resp = eli_core::finance::TimeseriesResponse {
                provider,
                sources: Vec::new(),
                tickers,
                granularity,
                range,
                start,
                end,
                generated_at: now,
                series,
                status: None,
                error: None,
                errors: if market_errors.is_empty() {
                    None
                } else {
                    Some(market_errors)
                },
                valid_tickers: None,
                analytics: Some(analytics),
                cache: None,
            };
            append_silent_drops(&mut resp, &requested, granularity);

            return match args.out {
                Some(out_path) => write_timeseries_out(out_path, &resp, &requested, &pm_meta_args),
                None => emit_timeseries_stdout(&resp, &requested),
            };
        }
    }

    let provider_str = args.provider.trim().to_ascii_lowercase();
    let is_auto = provider_str == "auto";

    // Split tickers by provider. Known-FRED presets stay FRED only in auto mode;
    // explicit tickers continue through the normal provider heuristics.
    let fred_preset = is_auto
        && matches!(
            preset_name.as_deref(),
            Some("macro") | Some("liquidity") | Some("yield_curve")
        );
    let mut pyth_tickers = Vec::new();
    let mut fred_tickers = Vec::new();
    let mut binance_tickers = Vec::new();
    let mut govt_tickers = Vec::new();
    // NAV:/PNAV:, BOND:, BUYBACK:, NYFED:, OFR:, ETF:, CBOE:, HL:, DERIBIT:, KF: — one
    // bucket per provider, fetched concurrently as side merges (or as the main
    // bucket when nothing else is present).
    let mut extra_buckets: Vec<(ExtraProvider, Vec<String>)> = Vec::new();
    let mut ibkr_tickers = Vec::new();
    let mut cleveland_tickers = Vec::new();
    let mut ratepath_tickers = Vec::new();
    let mut yahoo_tickers = Vec::new();
    for t in &preset_stock_tickers {
        match classify_timeseries_ticker(t, &provider_str, fred_preset) {
            TimeseriesTickerBucket::Pyth => pyth_tickers.push(t.clone()),
            TimeseriesTickerBucket::Fred => fred_tickers.push(t.clone()),
            TimeseriesTickerBucket::Binance => binance_tickers.push(t.clone()),
            TimeseriesTickerBucket::Govt => govt_tickers.push(t.clone()),
            TimeseriesTickerBucket::Extra(p) => push_extra_ticker(&mut extra_buckets, p, t),
            TimeseriesTickerBucket::Ibkr => ibkr_tickers.push(t.clone()),
            TimeseriesTickerBucket::ClevelandFed => cleveland_tickers.push(t.clone()),
            TimeseriesTickerBucket::RatePath => ratepath_tickers.push(t.clone()),
            TimeseriesTickerBucket::Main => yahoo_tickers.push(t.clone()),
        }
    }
    for t in &tickers {
        match classify_timeseries_ticker(t, &provider_str, false) {
            TimeseriesTickerBucket::Pyth => pyth_tickers.push(t.clone()),
            TimeseriesTickerBucket::Fred => fred_tickers.push(t.clone()),
            TimeseriesTickerBucket::Binance => binance_tickers.push(t.clone()),
            TimeseriesTickerBucket::Govt => govt_tickers.push(t.clone()),
            TimeseriesTickerBucket::Extra(p) => push_extra_ticker(&mut extra_buckets, p, t),
            TimeseriesTickerBucket::Ibkr => ibkr_tickers.push(t.clone()),
            TimeseriesTickerBucket::ClevelandFed => cleveland_tickers.push(t.clone()),
            TimeseriesTickerBucket::RatePath => ratepath_tickers.push(t.clone()),
            TimeseriesTickerBucket::Main => yahoo_tickers.push(t.clone()),
        }
    }
    let has_pyth = !pyth_tickers.is_empty();
    let has_fred = !fred_tickers.is_empty();
    let has_binance = !binance_tickers.is_empty();
    let has_govt = !govt_tickers.is_empty();
    let has_extra = !extra_buckets.is_empty();
    let has_ibkr = !ibkr_tickers.is_empty();
    let has_cleveland = !cleveland_tickers.is_empty();
    let has_ratepath = !ratepath_tickers.is_empty();

    let provider = match provider_str.as_str() {
        "auto" | "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "mock" => eli_core::finance::ProviderKind::Mock,
        "fred" => eli_core::finance::ProviderKind::Fred,
        "ibkr" => eli_core::finance::ProviderKind::Ibkr,
        "pyth" => eli_core::finance::ProviderKind::Pyth,
        "binance" => eli_core::finance::ProviderKind::Binance,
        "govt" => eli_core::finance::ProviderKind::Govt,
        other => match ExtraProvider::from_provider_str(other) {
            Some(p) => p.kind(),
            None => anyhow::bail!(
                "unsupported --provider '{other}' (supported: auto, yahoo, fred, ibkr, pyth, binance, govt, secnav, bond, buyback, nyfed, ofr, etf, cboe, hyperliquid, deribit, kraken_futures)"
            ),
        },
    };
    let use_ibkr_explicit = matches!(provider, eli_core::finance::ProviderKind::Ibkr);

    let cache_dir = if let Some(path) = args.cache_dir {
        path
    } else {
        let paths = Paths::discover().context("discover paths")?;
        paths.ensure_dirs().context("ensure dirs")?;
        paths.cache_dir
    };

    // Standalone Cleveland Fed path: only CLEV: tickers (+ optional prediction markets).
    let only_cleveland = has_cleveland
        && yahoo_tickers.is_empty()
        && fred_tickers.is_empty()
        && pyth_tickers.is_empty()
        && binance_tickers.is_empty()
        && govt_tickers.is_empty()
        && !has_extra
        && ibkr_tickers.is_empty()
        && !has_ratepath;
    if only_cleveland {
        let now = chrono::Utc::now();
        let end = as_of.unwrap_or(now).min(now);
        let start = end
            .checked_sub_signed(range.approx_duration())
            .ok_or_else(|| anyhow::anyhow!("range underflow"))?;

        let clev_series = fetch_cleveland_fed_series(&cleveland_tickers, start, end).await?;
        if clev_series.is_empty() {
            anyhow::bail!("no Cleveland Fed nowcast data found for requested tickers/range");
        }

        let mut all_series = clev_series;
        let mut all_errors: Vec<eli_core::finance::TimeseriesError> = Vec::new();

        // Merge prediction market series if present.
        if !prediction_markets.is_empty() {
            let (market_series, market_errors) =
                fetch_prediction_market_series_batch(&prediction_markets, start, end, granularity)
                    .await;
            all_series.extend(market_series);
            all_errors.extend(market_errors);
        }

        let analytics =
            eli_core::finance::build_timeseries_analytics(&all_series, granularity);
        let tickers_out: Vec<String> = all_series.iter().map(|s| s.ticker.clone()).collect();
        let mut resp = eli_core::finance::TimeseriesResponse {
            provider: eli_core::finance::ProviderKind::Fred, // closest match
            sources: Vec::new(),
            tickers: tickers_out,
            granularity,
            range,
            start,
            end,
            generated_at: now,
            series: all_series,
            status: None,
            error: None,
            errors: if all_errors.is_empty() { None } else { Some(all_errors) },
            valid_tickers: None,
            analytics: Some(analytics),
            cache: None,
        };
        // An unknown CLEV: name, or a prediction market that failed, used to
        // vanish from this path without a word.
        append_silent_drops(&mut resp, &requested, granularity);

        return match args.out {
            Some(out_path) => write_timeseries_out(
                out_path,
                &resp,
                &requested,
                &[
                    "provider=cleveland_fed".to_string(),
                    format!("tickers={}", cleveland_tickers.join(",")),
                ],
            ),
            None => emit_timeseries_stdout(&resp, &requested),
        };
    }

    // Standalone RATEPATH path: only RATEPATH: tickers (+ optional prediction markets).
    let only_ratepath = has_ratepath
        && yahoo_tickers.is_empty()
        && fred_tickers.is_empty()
        && pyth_tickers.is_empty()
        && binance_tickers.is_empty()
        && govt_tickers.is_empty()
        && !has_extra
        && ibkr_tickers.is_empty()
        && !has_cleveland;
    if only_ratepath {
        let now = chrono::Utc::now();
        let end = as_of.unwrap_or(now).min(now);
        let start = end
            .checked_sub_signed(range.approx_duration())
            .ok_or_else(|| anyhow::anyhow!("range underflow"))?;

        let (rp_series, rp_errors) =
            fetch_ratepath_series_batch(&ratepath_tickers, start, end, granularity).await;
        if rp_series.is_empty() {
            let err_msg = rp_errors
                .first()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "no usable RATEPATH series found".to_string());
            anyhow::bail!("{err_msg}");
        }

        let mut all_series = rp_series;
        let mut all_errors = rp_errors;

        // Merge prediction market series if present.
        if !prediction_markets.is_empty() {
            let (market_series, market_errors) =
                fetch_prediction_market_series_batch(&prediction_markets, start, end, granularity)
                    .await;
            all_series.extend(market_series);
            all_errors.extend(market_errors);
        }

        let analytics =
            eli_core::finance::build_timeseries_analytics(&all_series, granularity);
        let tickers_out: Vec<String> = all_series.iter().map(|s| s.ticker.clone()).collect();
        let mut resp = eli_core::finance::TimeseriesResponse {
            provider: eli_core::finance::ProviderKind::Yahoo, // closest match (no rate_path enum variant)
            sources: vec!["rate_path".to_string()],
            tickers: tickers_out,
            granularity,
            range,
            start,
            end,
            generated_at: now,
            series: all_series,
            status: None,
            error: None,
            errors: if all_errors.is_empty() { None } else { Some(all_errors) },
            valid_tickers: None,
            analytics: Some(analytics),
            cache: None,
        };
        append_silent_drops(&mut resp, &requested, granularity);

        return match args.out {
            Some(out_path) => write_timeseries_out(
                out_path,
                &resp,
                &requested,
                &[
                    "provider=rate_path".to_string(),
                    format!("tickers={}", ratepath_tickers.join(",")),
                ],
            ),
            None => emit_timeseries_stdout(&resp, &requested),
        };
    }

    // Route to the right provider based on ticker types.
    // Priority: FRED tickers are the "main" request when present (most common preset case).
    // Pyth and Yahoo are merged in separately.
    let (main_tickers, main_provider) = if has_ibkr && yahoo_tickers.is_empty() && !has_fred && !has_pyth && !has_binance {
        // All IBKR — strip IBKR: prefix before sending to provider
        let stripped: Vec<String> = ibkr_tickers.iter().map(|t| t.strip_prefix("IBKR:").or_else(|| t.strip_prefix("ibkr:")).unwrap_or(t).to_string()).collect();
        (stripped, eli_core::finance::ProviderKind::Ibkr)
    } else if has_fred {
        // FRED as main, Pyth and Yahoo merged separately
        (fred_tickers.clone(), eli_core::finance::ProviderKind::Fred)
    } else if has_binance && yahoo_tickers.is_empty() && !has_pyth && !has_ibkr {
        // All Binance
        (binance_tickers.clone(), eli_core::finance::ProviderKind::Binance)
    } else if has_pyth && yahoo_tickers.is_empty() && !has_binance && !has_ibkr {
        // All Pyth
        (pyth_tickers.clone(), eli_core::finance::ProviderKind::Pyth)
    } else if has_govt && yahoo_tickers.is_empty() && !has_pyth && !has_binance && !has_ibkr {
        // All sovereign yields (CNBC)
        (govt_tickers.clone(), eli_core::finance::ProviderKind::Govt)
    } else if has_extra && yahoo_tickers.is_empty() && !has_pyth && !has_binance && !has_ibkr {
        // Only new-prefix providers (± CLEV/RATEPATH/odds): first bucket is main,
        // the rest merge in as side fetches below.
        let (p, ts) = &extra_buckets[0];
        (ts.clone(), p.kind())
    } else {
        // Yahoo (default) — other providers merge in separately
        (yahoo_tickers.clone(), provider.clone())
    };
    let use_ibkr = use_ibkr_explicit || matches!(main_provider, eli_core::finance::ProviderKind::Ibkr);

    // FRED rejects sub-daily granularity. If the user requested 1m/5m/15m/30m/1h
    // for a request whose main bucket is FRED, downgrade JUST the FRED sub-fetch
    // to 1d rather than failing the whole call. Other-bucket merges below keep the
    // user's requested granularity. A `granularity_downgraded` warning is added to
    // each FRED series at response time.
    let fred_granularity_downgraded = matches!(
        main_provider,
        eli_core::finance::ProviderKind::Fred
    ) && matches!(
        granularity.unit,
        eli_core::finance::SpanUnit::Minute | eli_core::finance::SpanUnit::Hour
    );
    let main_granularity = if fred_granularity_downgraded {
        eli_core::finance::Span {
            n: 1,
            unit: eli_core::finance::SpanUnit::Day,
        }
    } else {
        granularity
    };

    let req = eli_core::finance::TimeseriesRequest {
        tickers: main_tickers.clone(),
        range,
        granularity: main_granularity,
        as_of,
        provider: main_provider.clone(),
        max_points_per_ticker: args.max_points_per_ticker,
        ibkr: use_ibkr.then(|| {
            build_ibkr_connection_config(
                args.ibkr_account.clone(),
                args.ibkr_host.clone(),
                args.ibkr_port,
                args.ibkr_client_id,
                args.ibkr_market_data_type,
                None,
            )
        }),
    };

    let fetch_result = eli_core::finance::fetch_timeseries(req, &cache_dir).await;
    let mut resp = match fetch_result {
        Ok(mut r) => {
            if fred_granularity_downgraded {
                let warn = format!(
                    "granularity downgraded from requested {:?} to 1d for FRED provider (sub-daily not supported)",
                    granularity.unit
                );
                // One entry per ticker: errors[].ticker is a single-identifier
                // contract — a comma-joined string breaks per-ticker lookup.
                let errs = r.errors.get_or_insert_with(Vec::new);
                for t in &main_tickers {
                    errs.push(eli_core::finance::TimeseriesError {
                        ticker: t.clone(),
                        stage: Some("granularity_downgrade".to_string()),
                        message: warn.clone(),
                    });
                }
            }
            // IBKR strips its `IBKR:` prefix before fetch (line ~354). Restore it on
            // returned series so callers see the round-tripped ticker — matching how
            // PYTH:/CLEV:/POLYMARKET:/KALSHI: prefixes round-trip in their fetchers.
            if matches!(main_provider, eli_core::finance::ProviderKind::Ibkr) {
                for s in &mut r.series {
                    if !s.ticker.starts_with("IBKR:") && !s.ticker.starts_with("ibkr:") {
                        s.ticker = format!("IBKR:{}", s.ticker);
                    }
                }
            }
            r
        }
        // IB Gateway down on a *preset* (energy/commodities/treasuries, which route purely to
        // IBKR) → don't crash the whole call; transparently remap the preset's IBKR futures to
        // their Yahoo front-month equivalents. Explicit IBKR: user tickers (no preset) still
        // surface the real error so a power user debugging the Gateway sees it.
        Err(e)
            if preset_name.is_some()
                && matches!(main_provider, eli_core::finance::ProviderKind::Ibkr)
                && is_ibkr_unavailable(&e.to_string()) =>
        {
            let fallback: Vec<String> = ibkr_tickers
                .iter()
                .filter_map(|t| ibkr_to_yahoo_fallback(t))
                .map(String::from)
                .collect();
            if fallback.is_empty() {
                return Err(anyhow::anyhow!(e)).context(
                    "fetch timeseries (IB Gateway unavailable; no Yahoo fallback for these tickers)",
                );
            }
            eprintln!(
                "warning: IB Gateway unreachable; serving Yahoo front-month equivalents for preset '{}'",
                preset_name.as_deref().unwrap_or("")
            );
            let yahoo_req = eli_core::finance::TimeseriesRequest {
                tickers: fallback,
                range,
                granularity,
                as_of,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: args.max_points_per_ticker,
                ibkr: None,
            };
            let mut r = eli_core::finance::fetch_timeseries(yahoo_req, &cache_dir)
                .await
                .map_err(|e2| anyhow::anyhow!(e2))
                .context("fetch timeseries (Yahoo fallback after IB Gateway unavailable)")?;
            r.errors.get_or_insert_with(Vec::new).push(
                eli_core::finance::TimeseriesError {
                    ticker: ibkr_tickers.join(","),
                    stage: Some("ibkr_fallback".to_string()),
                    message: format!(
                        "IB Gateway unreachable; served Yahoo front-month equivalents instead. Underlying error: {e}"
                    ),
                },
            );
            r
        }
        Err(e) => {
            return Err(anyhow::anyhow!(e)).context("fetch timeseries");
        }
    };

    // ---- Stage 2: every remaining provider group fetches CONCURRENTLY ----
    // These used to run as six sequential awaits, so a mixed-source call paid
    // the SUM of provider latencies instead of the MAX. Fetch first (joined),
    // merge in deterministic order afterwards, compute analytics once at the
    // end instead of after every merge.
    let window_start = resp.start;
    let window_end = resp.end;
    let max_points = args.max_points_per_ticker;

    let yahoo_extra_fut = async {
        if has_fred && !yahoo_tickers.is_empty() {
            Some(
                eli_core::finance::fetch_timeseries(
                    eli_core::finance::TimeseriesRequest {
                        tickers: yahoo_tickers.clone(),
                        range,
                        granularity,
                        as_of,
                        provider: eli_core::finance::ProviderKind::Yahoo,
                        max_points_per_ticker: max_points,
                        ibkr: None,
                    },
                    &cache_dir,
                )
                .await,
            )
        } else {
            None
        }
    };
    let pyth_fut = async {
        if has_pyth && (has_fred || !yahoo_tickers.is_empty()) {
            Some(
                eli_core::finance::fetch_timeseries(
                    eli_core::finance::TimeseriesRequest {
                        tickers: pyth_tickers.clone(),
                        range,
                        granularity,
                        as_of,
                        provider: eli_core::finance::ProviderKind::Pyth,
                        max_points_per_ticker: max_points,
                        ibkr: None,
                    },
                    &cache_dir,
                )
                .await,
            )
        } else {
            None
        }
    };
    let binance_fut = async {
        if has_binance && !binance_tickers.iter().all(|t| main_tickers.contains(t)) {
            Some(
                eli_core::finance::fetch_timeseries(
                    eli_core::finance::TimeseriesRequest {
                        tickers: binance_tickers.clone(),
                        range,
                        granularity,
                        as_of,
                        provider: eli_core::finance::ProviderKind::Binance,
                        max_points_per_ticker: max_points,
                        ibkr: None,
                    },
                    &cache_dir,
                )
                .await,
            )
        } else {
            None
        }
    };
    let govt_fut = async {
        if has_govt && !govt_tickers.iter().all(|t| main_tickers.contains(t)) {
            Some(
                eli_core::finance::fetch_timeseries(
                    eli_core::finance::TimeseriesRequest {
                        tickers: govt_tickers.clone(),
                        range,
                        granularity,
                        as_of,
                        provider: eli_core::finance::ProviderKind::Govt,
                        max_points_per_ticker: max_points,
                        ibkr: None,
                    },
                    &cache_dir,
                )
                .await,
            )
        } else {
            None
        }
    };
    let extra_fut = join_all(
        extra_buckets
            .iter()
            .filter(|(_, ts)| !ts.iter().all(|t| main_tickers.contains(t)))
            .map(|(p, ts)| {
                let cache_dir = &cache_dir;
                async move {
                    let r = eli_core::finance::fetch_timeseries(
                        eli_core::finance::TimeseriesRequest {
                            tickers: ts.clone(),
                            range,
                            granularity,
                            as_of,
                            provider: p.kind(),
                            max_points_per_ticker: max_points,
                            ibkr: None,
                        },
                        cache_dir,
                    )
                    .await;
                    (*p, ts.clone(), r)
                }
            }),
    );
    let ibkr_fut = async {
        if has_ibkr && !use_ibkr {
            let stripped: Vec<String> = ibkr_tickers
                .iter()
                .map(|t| {
                    t.strip_prefix("IBKR:")
                        .or_else(|| t.strip_prefix("ibkr:"))
                        .unwrap_or(t)
                        .to_string()
                })
                .collect();
            let ibkr_conn = eli_core::finance::IbkrConnectionConfig {
                market_data_type: Some(3), // delayed
                ..Default::default()
            };
            Some(
                eli_core::finance::fetch_timeseries(
                    eli_core::finance::TimeseriesRequest {
                        tickers: stripped,
                        range,
                        granularity,
                        as_of,
                        provider: eli_core::finance::ProviderKind::Ibkr,
                        max_points_per_ticker: max_points,
                        ibkr: Some(ibkr_conn),
                    },
                    &cache_dir,
                )
                .await,
            )
        } else {
            None
        }
    };
    let clev_fut = async {
        if has_cleveland && !only_cleveland {
            let now = chrono::Utc::now();
            let clev_end = as_of.unwrap_or(now).min(now);
            match clev_end.checked_sub_signed(range.approx_duration()) {
                Some(clev_start) => {
                    Some(fetch_cleveland_fed_series(&cleveland_tickers, clev_start, clev_end).await)
                }
                None => Some(Err(anyhow::anyhow!("range underflow"))),
            }
        } else {
            None
        }
    };
    let pm_fut = async {
        if !prediction_markets.is_empty() {
            Some(
                fetch_prediction_market_series_batch(
                    &prediction_markets,
                    window_start,
                    window_end,
                    granularity,
                )
                .await,
            )
        } else {
            None
        }
    };
    let rp_fut = async {
        if has_ratepath {
            Some(
                fetch_ratepath_series_batch(&ratepath_tickers, window_start, window_end, granularity)
                    .await,
            )
        } else {
            None
        }
    };

    let (yahoo_extra_res, pyth_res, binance_res, govt_res, extra_res, ibkr_res, clev_res, pm_res, rp_res) = tokio::join!(
        yahoo_extra_fut,
        pyth_fut,
        binance_fut,
        govt_fut,
        extra_fut,
        ibkr_fut,
        clev_fut,
        pm_fut,
        rp_fut
    );

    // Auto-fallback: if in auto mode and Yahoo returned errors, retry failed tickers
    // with FRED, and re-fetch valid Yahoo tickers individually so their data isn't
    // lost (the core drops all series when any ticker fails). Runs on the MAIN
    // response only, BEFORE side merges — so Pyth/Binance error tickers are never
    // mistaken for failed Yahoo tickers, and side-provider series can't be dropped
    // by the rebuild (both were latent bugs of the old sequential ordering).
    let auto_fallback_needed = is_auto
        && matches!(resp.provider, eli_core::finance::ProviderKind::Yahoo)
        && (resp.series.is_empty()
            || resp.status.as_deref() == Some("error")
            || resp.errors.as_ref().map(|e| !e.is_empty()).unwrap_or(false));
    if auto_fallback_needed {
        let failed_tickers: Vec<String> = resp
            .errors
            .as_ref()
            .map(|errs| errs.iter().map(|e| e.ticker.clone()).collect())
            .unwrap_or_default();
        let valid_tickers: Vec<String> = resp.valid_tickers.clone().unwrap_or_default();

        let mut merged_series = Vec::new();
        let mut remaining_errors = Vec::new();

        // Re-fetch valid Yahoo tickers (core dropped their data on partial failure)
        for t in &valid_tickers {
            let re_req = eli_core::finance::TimeseriesRequest {
                tickers: vec![t.clone()],
                range,
                granularity,
                as_of,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: max_points,
                ibkr: None,
            };
            if let Ok(re_resp) = eli_core::finance::fetch_timeseries(re_req, &cache_dir).await {
                merged_series.extend(re_resp.series);
            }
        }

        // Retry failed tickers with FRED
        if !failed_tickers.is_empty() {
            let fred_req = eli_core::finance::TimeseriesRequest {
                tickers: failed_tickers.clone(),
                range,
                granularity,
                as_of,
                provider: eli_core::finance::ProviderKind::Fred,
                max_points_per_ticker: max_points,
                ibkr: None,
            };
            match eli_core::finance::fetch_timeseries(fred_req, &cache_dir).await {
                Ok(fred_resp) => {
                    let fred_ok: std::collections::HashSet<String> =
                        fred_resp.series.iter().map(|s| s.ticker.clone()).collect();
                    merged_series.extend(fred_resp.series);
                    // Collect tickers that failed both Yahoo AND FRED
                    for t in &failed_tickers {
                        if !fred_ok.contains(t) {
                            remaining_errors.push(eli_core::finance::TimeseriesError {
                                ticker: t.clone(),
                                stage: Some("auto-fallback".to_string()),
                                message: "failed on both Yahoo and FRED".to_string(),
                            });
                        }
                    }
                }
                Err(_) => {
                    // FRED also failed entirely — keep original errors
                    if let Some(errs) = &resp.errors {
                        remaining_errors.extend(errs.iter().cloned());
                    }
                }
            }
        }

        if !merged_series.is_empty() {
            // If all data came from FRED (no Yahoo successes), label provider as fred.
            if valid_tickers.is_empty() {
                resp.provider = eli_core::finance::ProviderKind::Fred;
            }
            resp.series = merged_series;
            resp.status = if remaining_errors.is_empty() {
                None
            } else {
                Some("partial".to_string())
            };
            resp.error = None;
            resp.errors = if remaining_errors.is_empty() {
                None
            } else {
                Some(remaining_errors)
            };
            resp.valid_tickers = None;
        }
    }

    // ---- Merges (deterministic order) ----
    if let Some(result) = yahoo_extra_res {
        match result {
            Ok(yahoo_resp) => {
                resp.series.extend(yahoo_resp.series);
                resp.tickers.extend(yahoo_tickers.clone());
            }
            Err(e) => {
                eprintln!("warning: Yahoo fetch failed for mixed request: {e}");
            }
        }
    }
    if let Some(result) = pyth_res {
        match result {
            Ok(pyth_resp) => {
                resp.series.extend(pyth_resp.series);
                resp.tickers.extend(pyth_tickers.clone());
                if let Some(ref pyth_errors) = pyth_resp.errors {
                    resp.errors
                        .get_or_insert_with(Vec::new)
                        .extend(pyth_errors.clone());
                }
            }
            Err(e) => {
                eprintln!("warning: PYTH: fetch failed: {e}");
                // One error per ticker: errors[].ticker is a single-identifier contract.
                let errs = resp.errors.get_or_insert_with(Vec::new);
                for t in &pyth_tickers {
                    errs.push(eli_core::finance::TimeseriesError {
                        ticker: t.clone(),
                        stage: Some("pyth".to_string()),
                        message: format!("PYTH: fetch failed: {e}"),
                    });
                }
            }
        }
    }
    if let Some(result) = binance_res {
        match result {
            Ok(binance_resp) => {
                resp.series.extend(binance_resp.series);
                resp.tickers.extend(binance_tickers.clone());
                if let Some(ref binance_errors) = binance_resp.errors {
                    resp.errors
                        .get_or_insert_with(Vec::new)
                        .extend(binance_errors.clone());
                }
            }
            Err(e) => {
                eprintln!("warning: Binance fetch failed: {e}");
                let errs = resp.errors.get_or_insert_with(Vec::new);
                for t in &binance_tickers {
                    errs.push(eli_core::finance::TimeseriesError {
                        ticker: t.clone(),
                        stage: Some("binance".to_string()),
                        message: format!("Binance provider failed: {e}"),
                    });
                }
            }
        }
    }
    if let Some(result) = govt_res {
        match result {
            Ok(govt_resp) => {
                resp.series.extend(govt_resp.series);
                resp.tickers.extend(govt_tickers.clone());
                if let Some(ref govt_errors) = govt_resp.errors {
                    resp.errors
                        .get_or_insert_with(Vec::new)
                        .extend(govt_errors.clone());
                }
            }
            Err(e) => {
                eprintln!("warning: sovereign-yield (GOVT) fetch failed: {e}");
                let errs = resp.errors.get_or_insert_with(Vec::new);
                for t in &govt_tickers {
                    errs.push(eli_core::finance::TimeseriesError {
                        ticker: t.clone(),
                        stage: Some("govt".to_string()),
                        message: format!("GOVT provider failed: {e}"),
                    });
                }
            }
        }
    }
    for (p, ts, result) in extra_res {
        match result {
            Ok(r) => {
                resp.series.extend(r.series);
                resp.tickers.extend(ts);
                if let Some(errs) = r.errors {
                    resp.errors.get_or_insert_with(Vec::new).extend(errs);
                }
            }
            Err(e) => {
                eprintln!("warning: {} fetch failed: {e}", p.label());
                // One error per ticker: errors[].ticker is a single-identifier contract.
                let errs = resp.errors.get_or_insert_with(Vec::new);
                for t in ts {
                    errs.push(eli_core::finance::TimeseriesError {
                        ticker: t,
                        stage: Some(p.label().to_string()),
                        message: format!("{} provider failed: {e}", p.label()),
                    });
                }
            }
        }
    }
    if let Some(result) = ibkr_res {
        match result {
            Ok(mut ibkr_resp) => {
                // Re-attach IBKR: prefix on returned series so round-trip is consistent
                // with PYTH:/CLEV:/POLYMARKET:/KALSHI: prefix preservation.
                for s in &mut ibkr_resp.series {
                    if !s.ticker.starts_with("IBKR:") && !s.ticker.starts_with("ibkr:") {
                        s.ticker = format!("IBKR:{}", s.ticker);
                    }
                }
                resp.series.extend(ibkr_resp.series);
                resp.tickers.extend(ibkr_tickers.clone());
                if let Some(ref ibkr_errors) = ibkr_resp.errors {
                    resp.errors
                        .get_or_insert_with(Vec::new)
                        .extend(ibkr_errors.clone());
                }
            }
            Err(e) => {
                eprintln!("warning: IBKR fetch failed: {e}");
                resp.errors
                    .get_or_insert_with(Vec::new)
                    .push(eli_core::finance::TimeseriesError {
                        ticker: ibkr_tickers.join(","),
                        stage: Some("ibkr".to_string()),
                        message: format!("IBKR provider failed: {e}"),
                    });
            }
        }
    }
    if let Some(result) = clev_res {
        match result {
            Ok(clev_series) => {
                resp.series.extend(clev_series);
                resp.tickers.extend(cleveland_tickers.clone());
            }
            Err(e) => {
                eprintln!("warning: Cleveland Fed fetch failed: {e}");
                let errs = resp.errors.get_or_insert_with(Vec::new);
                for t in &cleveland_tickers {
                    errs.push(eli_core::finance::TimeseriesError {
                        ticker: t.clone(),
                        stage: Some("cleveland_fed".to_string()),
                        message: format!("Cleveland Fed provider failed: {e}"),
                    });
                }
            }
        }
    }
    if let Some((market_series, market_errors)) = pm_res {
        let mut existing_series: HashSet<String> =
            resp.series.iter().map(|s| s.ticker.clone()).collect();
        for series in market_series {
            if existing_series.insert(series.ticker.clone()) {
                resp.tickers.push(series.ticker.clone());
                resp.series.push(series);
            }
        }
        if !market_errors.is_empty() {
            resp.errors
                .get_or_insert_with(Vec::new)
                .extend(market_errors);
        }
        if !resp.series.is_empty() {
            resp.error = None;
            resp.valid_tickers = None;
        }
    }
    if let Some((rp_series, rp_errors)) = rp_res {
        let mut existing_series: HashSet<String> =
            resp.series.iter().map(|s| s.ticker.clone()).collect();
        for series in rp_series {
            if existing_series.insert(series.ticker.clone()) {
                resp.tickers.push(series.ticker.clone());
                resp.series.push(series);
            }
        }
        if !rp_errors.is_empty() {
            resp.errors.get_or_insert_with(Vec::new).extend(rp_errors);
        }
        if !resp.series.is_empty() {
            resp.error = None;
            resp.valid_tickers = None;
        }
    }

    // Returned tickers go back to the caller's spelling before anything is
    // keyed on them (analytics.stats, the drop check, the failure count).
    restore_requested_spelling(&mut resp, &requested);

    // Single analytics pass over the final merged series set (was recomputed
    // after every individual merge above).
    if !resp.series.is_empty() {
        resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
            &resp.series,
            resp.granularity,
        ));
    }

    // `sources`, `status` and the error object are settled by the shared
    // shaping step, the same for stdout and for --out.
    append_silent_drops(&mut resp, &requested, granularity);

    match args.out {
        Some(out_path) => write_timeseries_out(
            out_path,
            &resp,
            &requested,
            &[
                format!("range={}", args.range),
                format!("granularity={}", args.granularity),
            ],
        ),
        None => emit_timeseries_stdout(&resp, &requested),
    }
}

fn parse_window_start(raw: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        anyhow::bail!("empty start value");
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("invalid start '{raw}' (use YYYY-MM-DD or RFC3339)"))?;
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid start date '{raw}'"))?;
    Ok(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
        naive,
        chrono::Utc,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimeseriesTickerBucket {
    Pyth,
    Fred,
    Binance,
    Govt,
    Extra(ExtraProvider),
    Ibkr,
    ClevelandFed,
    RatePath,
    Main,
}

fn classify_timeseries_ticker(
    ticker: &str,
    provider_str: &str,
    auto_prefers_fred: bool,
) -> TimeseriesTickerBucket {
    // RATEPATH: virtual provider — matched BEFORE other prefixes so per-meeting
    // rate-path probability paths flow through the same multi-asset surface as
    // Polymarket/Kalshi/Pyth/etc.
    if ticker.starts_with("RATEPATH:") || ticker.starts_with("ratepath:") {
        return TimeseriesTickerBucket::RatePath;
    }
    if ticker.starts_with("CLEV:") || ticker.starts_with("clev:") {
        return TimeseriesTickerBucket::ClevelandFed;
    }
    // Explicit-prefix providers win over --provider and every heuristic below
    // (FRED's all-caps rule would otherwise grab nothing here since ':' is
    // excluded, but IBKR/pyth/binance provider overrides would).
    if let Some(p) = ExtraProvider::classify(ticker) {
        return TimeseriesTickerBucket::Extra(p);
    }
    if let Some(p) = ExtraProvider::from_provider_str(provider_str) {
        return TimeseriesTickerBucket::Extra(p);
    }
    if ticker.starts_with("IBKR:") || ticker.starts_with("ibkr:") || provider_str == "ibkr" {
        return TimeseriesTickerBucket::Ibkr;
    }
    if eli_core::finance::is_pyth_ticker(ticker) || provider_str == "pyth" {
        return TimeseriesTickerBucket::Pyth;
    }
    if eli_core::finance::is_binance_ticker(ticker) || provider_str == "binance" {
        return TimeseriesTickerBucket::Binance;
    }
    if eli_core::finance::is_govt_ticker(ticker) || provider_str == "govt" {
        return TimeseriesTickerBucket::Govt;
    }
    if provider_str == "fred" || (provider_str == "auto" && (auto_prefers_fred || is_fred_ticker(ticker))) {
        return TimeseriesTickerBucket::Fred;
    }
    TimeseriesTickerBucket::Main
}

/// Prefix-routed providers added after GOVT: (all share the core fetch path and
/// are merged generically).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExtraProvider {
    Secnav,
    FinraBonds,
    Buyback,
    Nyfed,
    Ofr,
    EtfFlows,
    Cboe,
    Hyperliquid,
    Deribit,
    KrakenFutures,
}

impl ExtraProvider {
    fn classify(ticker: &str) -> Option<Self> {
        use eli_core::finance as f;
        if f::is_secnav_ticker(ticker) {
            Some(Self::Secnav)
        } else if f::is_finra_bond_ticker(ticker) {
            Some(Self::FinraBonds)
        } else if f::is_buyback_ticker(ticker) {
            Some(Self::Buyback)
        } else if f::is_nyfed_ticker(ticker) {
            Some(Self::Nyfed)
        } else if f::is_ofr_ticker(ticker) {
            Some(Self::Ofr)
        } else if f::is_etf_flow_ticker(ticker) {
            Some(Self::EtfFlows)
        } else if f::is_cboe_ticker(ticker) {
            Some(Self::Cboe)
        } else if f::is_hyperliquid_ticker(ticker) {
            Some(Self::Hyperliquid)
        } else if f::is_deribit_ticker(ticker) {
            Some(Self::Deribit)
        } else if f::is_kraken_futures_ticker(ticker) {
            Some(Self::KrakenFutures)
        } else {
            None
        }
    }

    fn from_provider_str(s: &str) -> Option<Self> {
        Some(match s {
            "secnav" | "nav" | "sec_nav" => Self::Secnav,
            "bond" | "bonds" | "finra" | "finra_bonds" | "trace" => Self::FinraBonds,
            "buyback" | "buybacks" => Self::Buyback,
            "nyfed" => Self::Nyfed,
            "ofr" => Self::Ofr,
            "etf" | "etf_flows" => Self::EtfFlows,
            "cboe" => Self::Cboe,
            "hl" | "hyperliquid" => Self::Hyperliquid,
            "deribit" => Self::Deribit,
            "kf" | "kraken_futures" | "kraken-futures" | "krakenfutures" => Self::KrakenFutures,
            _ => return None,
        })
    }

    fn kind(self) -> eli_core::finance::ProviderKind {
        use eli_core::finance::ProviderKind as K;
        match self {
            Self::Secnav => K::Secnav,
            Self::FinraBonds => K::FinraBonds,
            Self::Buyback => K::Buyback,
            Self::Nyfed => K::Nyfed,
            Self::Ofr => K::Ofr,
            Self::EtfFlows => K::EtfFlows,
            Self::Cboe => K::Cboe,
            Self::Hyperliquid => K::Hyperliquid,
            Self::Deribit => K::Deribit,
            Self::KrakenFutures => K::KrakenFutures,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Secnav => "secnav",
            Self::FinraBonds => "finra_bonds",
            Self::Buyback => "buyback",
            Self::Nyfed => "nyfed",
            Self::Ofr => "ofr",
            Self::EtfFlows => "etf_flows",
            Self::Cboe => "cboe",
            Self::Hyperliquid => "hyperliquid",
            Self::Deribit => "deribit",
            Self::KrakenFutures => "kraken_futures",
        }
    }
}

fn push_extra_ticker(buckets: &mut Vec<(ExtraProvider, Vec<String>)>, p: ExtraProvider, t: &str) {
    match buckets.iter_mut().find(|(bp, _)| *bp == p) {
        Some((_, ts)) => ts.push(t.to_string()),
        None => buckets.push((p, vec![t.to_string()])),
    }
}

#[derive(Clone, Copy, Debug)]
enum PredictionMarketProvider {
    Kalshi,
    Polymarket,
}

impl PredictionMarketProvider {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Kalshi => "kalshi",
            Self::Polymarket => "polymarket",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum PredictionMarketSide {
    Yes,
    No,
}

impl PredictionMarketSide {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
        }
    }

    fn apply(self, yes_probability: f64) -> f64 {
        let p = yes_probability.clamp(0.0, 1.0);
        match self {
            Self::Yes => p,
            Self::No => (1.0 - p).clamp(0.0, 1.0),
        }
    }
}

#[derive(Clone, Debug)]
struct PredictionMarketRequest {
    provider: PredictionMarketProvider,
    market: String,
    side: PredictionMarketSide,
}

fn parse_optional_prediction_market_request(
    provider: Option<&str>,
    market: Option<&str>,
    side: &str,
) -> Result<Option<PredictionMarketRequest>> {
    let provider = provider.map(str::trim).filter(|v| !v.is_empty());
    let market = market.map(str::trim).filter(|v| !v.is_empty());

    if provider.is_none() && market.is_none() {
        return Ok(None);
    }
    if provider.is_none() || market.is_none() {
        anyhow::bail!("--odds-provider and --odds-market must be provided together");
    }

    let provider = match provider.unwrap_or_default().to_ascii_lowercase().as_str() {
        "kalshi" => PredictionMarketProvider::Kalshi,
        "polymarket" => PredictionMarketProvider::Polymarket,
        other => {
            anyhow::bail!("unsupported --odds-provider '{other}' (supported: kalshi, polymarket)")
        }
    };

    let side = match side.trim().to_ascii_lowercase().as_str() {
        "yes" | "y" => PredictionMarketSide::Yes,
        "no" | "n" => PredictionMarketSide::No,
        other => anyhow::bail!("unsupported --odds-side '{other}' (supported: yes, no)"),
    };

    Ok(Some(PredictionMarketRequest {
        provider,
        market: market.unwrap_or_default().to_string(),
        side,
    }))
}

/// Known futures root symbols → (exchange, contract offsets in months).
/// When a bare root symbol is passed as a ticker, it auto-expands to the full
/// curve across multiple contract months via IBKR.
/// Valid contract-month set for a futures root.
/// `&[u32]` of months 1-12 that are actually traded.
type ContractMonths = &'static [u32];

/// Every month — energy, metals (mostly).
const ALL_MONTHS: ContractMonths = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
/// Quarterly — Treasuries, equity index, palladium, platinum.
const QUARTERLY_MONTHS: ContractMonths = &[3, 6, 9, 12];
/// Grains: Mar, May, Jul, Sep, Dec.
const GRAIN_MONTHS: ContractMonths = &[3, 5, 7, 9, 12];
/// Soybeans: Jan, Mar, May, Jul, Aug, Sep, Nov.
const SOYBEAN_MONTHS: ContractMonths = &[1, 3, 5, 7, 8, 9, 11];
/// Sugar: Mar, May, Jul, Oct.
const SUGAR_MONTHS: ContractMonths = &[3, 5, 7, 10];
/// Cotton: Mar, May, Jul, Oct, Dec.
const COTTON_MONTHS: ContractMonths = &[3, 5, 7, 10, 12];

fn futures_curve_months(root: &str) -> Option<(&'static str, ContractMonths)> {
    match root {
        // Energy — every month
        "CL" => Some(("NYMEX", ALL_MONTHS)),    // WTI Crude
        "BZ" => Some(("NYMEX", ALL_MONTHS)),    // Brent Crude Last Day Financial (NYMEX cash-settled, Yahoo BZ=F)
        "COIL" | "B" | "BRN" => Some(("IPE", ALL_MONTHS)),  // ICE Brent (physical, IPE/ICEEU)
        "HO" => Some(("NYMEX", ALL_MONTHS)),    // Heating Oil
        "RB" => Some(("NYMEX", ALL_MONTHS)),    // RBOB Gasoline
        "NG" => Some(("NYMEX", ALL_MONTHS)),    // Natural Gas
        "GOIL" => Some(("IPE", ALL_MONTHS)),    // ICE Gasoil
        // Metals
        "GC" => Some(("COMEX", &[2, 4, 6, 8, 10, 12])),  // Gold (even months + active)
        "SI" => Some(("COMEX", &[3, 5, 7, 9, 12])),      // Silver
        "HG" => Some(("COMEX", ALL_MONTHS)),              // Copper
        "PA" => Some(("NYMEX", QUARTERLY_MONTHS)),        // Palladium
        "PL" => Some(("NYMEX", &[1, 4, 7, 10])),          // Platinum (Jan/Apr/Jul/Oct)
        // Treasuries — quarterly (Mar/Jun/Sep/Dec)
        "ZT" => Some(("CBOT", QUARTERLY_MONTHS)),         // 2-Year Note
        "ZF" => Some(("CBOT", QUARTERLY_MONTHS)),         // 5-Year Note
        "ZN" => Some(("CBOT", QUARTERLY_MONTHS)),         // 10-Year Note
        "ZB" => Some(("CBOT", QUARTERLY_MONTHS)),         // 30-Year Bond
        "UB" => Some(("CBOT", QUARTERLY_MONTHS)),         // Ultra Bond
        // Equity index — quarterly
        "ES" => Some(("CME", QUARTERLY_MONTHS)),          // E-mini S&P 500
        "NQ" => Some(("CME", QUARTERLY_MONTHS)),          // E-mini Nasdaq
        "YM" => Some(("CBOT", QUARTERLY_MONTHS)),         // E-mini Dow
        "RTY" => Some(("CME", QUARTERLY_MONTHS)),         // E-mini Russell
        // Volatility — every month
        "VIX" | "VX" => Some(("CFE", ALL_MONTHS)),        // VIX monthly
        // Agriculture
        "ZC" => Some(("CBOT", GRAIN_MONTHS)),             // Corn
        "ZW" => Some(("CBOT", GRAIN_MONTHS)),             // Wheat
        "ZS" => Some(("CBOT", SOYBEAN_MONTHS)),           // Soybeans
        "KC" => Some(("NYBOT", GRAIN_MONTHS)),            // Coffee (Mar/May/Jul/Sep/Dec)
        "CT" => Some(("NYBOT", COTTON_MONTHS)),           // Cotton
        "SB" => Some(("NYBOT", SUGAR_MONTHS)),            // Sugar
        "CC" => Some(("NYBOT", GRAIN_MONTHS)),            // Cocoa (Mar/May/Jul/Sep/Dec)
        _ => None,
    }
}

/// Generate up to 8 forward contract months by walking the valid contract calendar
/// starting from the current month. Skips months not in the valid set.
fn expand_futures_curve(root: &str) -> Option<Vec<String>> {
    let (exchange, valid_months) = futures_curve_months(root)?;
    let now = chrono::Utc::now();
    let now_year = now.format("%Y").to_string().parse::<i32>().ok()?;
    let now_month = now.format("%m").to_string().parse::<u32>().ok()?;

    let mut tickers = Vec::new();
    let mut year = now_year;
    let mut month = now_month;
    let max_contracts = 8;
    // Walk forward up to ~3 years to collect 8 contracts.
    for _ in 0..36 {
        if tickers.len() >= max_contracts {
            break;
        }
        if valid_months.contains(&month) {
            let ym = format!("{year:04}{month:02}");
            tickers.push(format!("IBKR:FUT:{root}:{exchange}:{ym}"));
        }
        month += 1;
        if month > 12 {
            month = 1;
            year += 1;
        }
    }
    Some(tickers)
}

fn push_timeseries_input(
    raw: &str,
    prediction_markets: &mut Vec<PredictionMarketRequest>,
    plain_tickers: &mut Vec<String>,
) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(req) = parse_prediction_market_ticker(trimmed) {
        push_prediction_market_request(prediction_markets, req);
    } else if let Some(curve_tickers) = expand_futures_curve(trimmed) {
        // Bare futures root symbol → auto-expand to the full curve
        for t in curve_tickers {
            plain_tickers.push(t);
        }
    } else {
        plain_tickers.push(trimmed.to_string());
    }
}

fn push_prediction_market_request(
    prediction_markets: &mut Vec<PredictionMarketRequest>,
    req: PredictionMarketRequest,
) {
    if prediction_markets
        .iter()
        .any(|existing| {
            existing.provider.as_str() == req.provider.as_str()
                && existing.market == req.market
                && existing.side.as_str() == req.side.as_str()
        })
    {
        return;
    }
    prediction_markets.push(req);
}

fn parse_prediction_market_ticker(raw: &str) -> Option<PredictionMarketRequest> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("KALSHI:") {
        let (market, side) = split_prediction_market_side(rest)?;
        return Some(PredictionMarketRequest {
            provider: PredictionMarketProvider::Kalshi,
            market: market.to_string(),
            side,
        });
    }

    if let Some(rest) = trimmed.strip_prefix("POLYMARKET:") {
        // POLYMARKET:EVENT:<id|slug>[:NO] expands to every leg of the event.
        if let Some(event) = rest
            .strip_prefix("EVENT:")
            .or_else(|| rest.strip_prefix("event:"))
        {
            let (key, side) = split_prediction_market_side(event)?;
            return Some(PredictionMarketRequest {
                provider: PredictionMarketProvider::Polymarket,
                market: format!("EVENT:{key}"),
                side,
            });
        }
        let (market, side) = split_prediction_market_side(rest)?;
        return Some(PredictionMarketRequest {
            provider: PredictionMarketProvider::Polymarket,
            market: market.to_string(),
            side,
        });
    }

    if let Some((market, side)) = trimmed.rsplit_once(':') {
        let side = parse_prediction_market_side(side)?;
        if (market.starts_with("KX") && market.contains('-'))
            || looks_like_bare_kalshi_market(market)
        {
            return Some(PredictionMarketRequest {
                provider: PredictionMarketProvider::Kalshi,
                market: market.to_string(),
                side,
            });
        }
        if market.len() >= 6 && market.chars().all(|c| c.is_ascii_digit()) {
            return Some(PredictionMarketRequest {
                provider: PredictionMarketProvider::Polymarket,
                market: market.to_string(),
                side,
            });
        }
    }

    if (trimmed.starts_with("KX") && trimmed.contains('-')) || looks_like_bare_kalshi_market(trimmed) {
        return Some(PredictionMarketRequest {
            provider: PredictionMarketProvider::Kalshi,
            market: trimmed.to_string(),
            side: PredictionMarketSide::Yes,
        });
    }

    if trimmed.len() >= 6 && trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Some(PredictionMarketRequest {
            provider: PredictionMarketProvider::Polymarket,
            market: trimmed.to_string(),
            side: PredictionMarketSide::Yes,
        });
    }

    None
}

/// A Kalshi MARKET ticker without the KX prefix (CONTROLS-2026-D,
/// SENATEOHS-26-R, older FED-23DEC-T5.25): uppercase SERIES-EVENT-LEG with
/// at least two dashes, a letter-led series of 3+ characters, and a digit in
/// the event segment. Yahoo symbols never take that shape (BRK-B, BTC-USD,
/// RDS-A have one dash; ^VIX, CL=F, USD/JPY=X carry other characters), so
/// finance_odds tickers join without a prefix. One-dash Kalshi EVENT
/// tickers (CONTROLS-2026) stay ambiguous: use KALSHI:.
fn looks_like_bare_kalshi_market(raw: &str) -> bool {
    let parts: Vec<&str> = raw.split('-').collect();
    if parts.len() < 3 || parts.iter().any(|p| p.is_empty()) {
        return false;
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '-' | '.'))
    {
        return false;
    }
    let series = parts[0];
    series.len() >= 3
        && series.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && !series.contains('.')
        && parts[1].chars().any(|c| c.is_ascii_digit())
}

fn split_prediction_market_side(raw: &str) -> Option<(&str, PredictionMarketSide)> {
    match raw.rsplit_once(':') {
        Some((market, side)) if !market.is_empty() => {
            parse_prediction_market_side(side).map(|parsed| (market, parsed))
        }
        _ => Some((raw, PredictionMarketSide::Yes)),
    }
}

fn parse_prediction_market_side(raw: &str) -> Option<PredictionMarketSide> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "yes" | "y" => Some(PredictionMarketSide::Yes),
        "no" | "n" => Some(PredictionMarketSide::No),
        _ => None,
    }
}

fn prediction_market_response_provider(
    prediction_markets: &[PredictionMarketRequest],
) -> eli_core::finance::ProviderKind {
    match prediction_markets.first().map(|req| req.provider) {
        Some(PredictionMarketProvider::Kalshi) => eli_core::finance::ProviderKind::Kalshi,
        Some(PredictionMarketProvider::Polymarket) => eli_core::finance::ProviderKind::Polymarket,
        None => eli_core::finance::ProviderKind::Yahoo,
    }
}

async fn fetch_prediction_market_series(
    req: &PredictionMarketRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<eli_core::finance::TickerSeries> {
    match req.provider {
        PredictionMarketProvider::Kalshi => {
            fetch_kalshi_market_series(req, start, end, granularity).await
        }
        PredictionMarketProvider::Polymarket => {
            fetch_polymarket_market_series(req, start, end, granularity).await
        }
    }
}

const CLEVELAND_FED_NOWCAST_URL: &str =
    "https://www.clevelandfed.org/-/media/files/webcharts/inflationnowcasting/nowcast_quarter.json";

/// Fetch Cleveland Fed inflation nowcast series for the given CLEV: tickers.
/// Returns daily nowcast values as TickerSeries (OHLC all set to the nowcast value).
async fn fetch_cleveland_fed_series(
    tickers: &[String],
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<eli_core::finance::TickerSeries>> {
    use chrono::{NaiveDate, TimeZone};

    // Map CLEV: ticker suffixes to FusionCharts series names.
    let series_map: Vec<(String, &str)> = tickers
        .iter()
        .filter_map(|t| {
            let suffix = t
                .strip_prefix("CLEV:")
                .or_else(|| t.strip_prefix("clev:"))
                .unwrap_or(t)
                .to_ascii_uppercase();
            let fc_name = match suffix.as_str() {
                "CPI" => "CPI Inflation",
                "CORECPI" => "Core CPI Inflation",
                "PCE" => "PCE Inflation",
                "COREPCE" => "Core PCE Inflation",
                _ => return None,
            };
            Some((t.clone(), fc_name))
        })
        .collect();

    if series_map.is_empty() {
        anyhow::bail!("no valid Cleveland Fed tickers (supported: CPI, CORECPI, PCE, COREPCE)");
    }

    // Fetch the JSON.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let body = client
        .get(CLEVELAND_FED_NOWCAST_URL)
        .header("User-Agent", "eli/1.0")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let quarters: Vec<serde_json::Value> = serde_json::from_str(&body)?;

    if quarters.is_empty() {
        anyhow::bail!("Cleveland Fed nowcast returned empty data");
    }

    // Build date+value vectors from ALL quarters that overlap [start, end].
    // For each quarter: subcaption = "YYYY:QN", categories has date labels.
    let mut ticker_candles: BTreeMap<String, Vec<eli_core::finance::Candle>> = BTreeMap::new();
    for (orig_ticker, _) in &series_map {
        ticker_candles.insert(orig_ticker.clone(), Vec::new());
    }

    for quarter in &quarters {
        let subcaption = quarter
            .pointer("/chart/subcaption")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Parse "YYYY:QN" → base year.
        let base_year: i32 = match subcaption.split(':').next().and_then(|y| y.parse().ok()) {
            Some(y) => y,
            None => continue,
        };

        // Extract non-vline date labels.
        let categories = match quarter.pointer("/categories/0/category").and_then(|v| v.as_array())
        {
            Some(c) => c,
            None => continue,
        };
        let mut date_labels: Vec<NaiveDate> = Vec::new();
        let mut prev_month: u32 = 0;
        let mut year = base_year;
        for cat in categories {
            if cat.get("vline").is_some() {
                continue;
            }
            let label = match cat.get("label").and_then(|v| v.as_str()) {
                Some(l) => l,
                None => continue,
            };
            // Label format: "MM/DD"
            let parts: Vec<&str> = label.split('/').collect();
            if parts.len() != 2 {
                continue;
            }
            let month: u32 = match parts[0].parse() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let day: u32 = match parts[1].parse() {
                Ok(d) => d,
                Err(_) => continue,
            };
            // Detect year rollover (e.g., Q4: Oct→Dec is base_year, Jan→Mar is base_year+1).
            if month < prev_month && prev_month >= 10 && month <= 3 {
                year = base_year + 1;
            }
            prev_month = month;
            if let Some(nd) = NaiveDate::from_ymd_opt(year, month, day) {
                date_labels.push(nd);
            }
        }

        // Extract dataset values for requested series.
        let datasets = match quarter.get("dataset").and_then(|v| v.as_array()) {
            Some(d) => d,
            None => continue,
        };

        for (orig_ticker, fc_name) in &series_map {
            // Find matching dataset.
            let ds = match datasets.iter().find(|d| {
                d.get("seriesname")
                    .and_then(|v| v.as_str())
                    .map(|s| s == *fc_name)
                    .unwrap_or(false)
            }) {
                Some(d) => d,
                None => continue,
            };
            let data_arr = match ds.get("data").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => continue,
            };

            let candles = ticker_candles.get_mut(orig_ticker).unwrap();
            // data_arr aligns 1:1 with non-vline categories.
            let mut date_idx = 0;
            for item in data_arr {
                if date_idx >= date_labels.len() {
                    break;
                }
                let nd = date_labels[date_idx];
                date_idx += 1;
                let val_str = match item.get("value").and_then(|v| v.as_str()) {
                    Some(s) if !s.is_empty() => s,
                    _ => continue,
                };
                let val: f64 = match val_str.parse() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let dt = chrono::Utc
                    .from_utc_datetime(&nd.and_hms_opt(0, 0, 0).unwrap());
                // Filter to [start, end].
                if dt < start || dt > end {
                    continue;
                }
                candles.push(eli_core::finance::Candle {
                    t: dt,
                    o: val,
                    h: val,
                    l: val,
                    c: val,
                    v: None,
                    kind: Some("point".to_string()),
                });
            }
        }
    }

    // Build TickerSeries results.
    let mut result = Vec::new();
    for (orig_ticker, _) in &series_map {
        let mut candles = ticker_candles.remove(orig_ticker).unwrap_or_default();
        // Deduplicate by date (later quarters may overlap).
        candles.sort_by_key(|c| c.t);
        candles.dedup_by_key(|c| c.t);
        if !candles.is_empty() {
            let upstream = orig_ticker
                .strip_prefix("CLEV:")
                .or_else(|| orig_ticker.strip_prefix("clev:"))
                .unwrap_or(orig_ticker)
                .to_string();
            result.push(eli_core::finance::TickerSeries {
                ticker: orig_ticker.clone(),
                candles,
                source: Some("cleveland_fed".to_string()),
                upstream_id: Some(upstream),
            });
        }
    }

    Ok(result)
}

async fn fetch_prediction_market_series_batch(
    requests: &[PredictionMarketRequest],
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> (
    Vec<eli_core::finance::TickerSeries>,
    Vec<eli_core::finance::TimeseriesError>,
) {
    // POLYMARKET:EVENT: tickers become one request per leg (+ a legend notice).
    let (requests, poly_event_notices) = poly_expand_event_requests(requests).await;
    // Kalshi requests share batch calls (≤100 markets each) plus archive and
    // event fallbacks; the rest fetch one market per request.
    let (kalshi_reqs, other_reqs): (Vec<PredictionMarketRequest>, Vec<PredictionMarketRequest>) =
        requests
            .iter()
            .cloned()
            .partition(|req| matches!(req.provider, PredictionMarketProvider::Kalshi));
    let (kalshi_out, results) = tokio::join!(
        async {
            if kalshi_reqs.is_empty() {
                (Vec::new(), Vec::new())
            } else {
                fetch_kalshi_series_batch(&kalshi_reqs, start, end, granularity).await
            }
        },
        join_all(other_reqs.iter().map(|req| async move {
            match req.provider {
                PredictionMarketProvider::Polymarket => {
                    fetch_polymarket_series_with_notices(req, start, end, granularity).await
                }
                _ => fetch_prediction_market_series(req, start, end, granularity)
                    .await
                    .map(|s| (s, Vec::new())),
            }
        }))
    );

    let (mut series, mut errors) = kalshi_out;
    errors.extend(poly_event_notices);

    for (req, result) in other_reqs.iter().zip(results) {
        match result {
            Ok((market_series, notices)) => {
                series.push(market_series);
                errors.extend(notices);
            }
            Err(err) => errors.push(eli_core::finance::TimeseriesError {
                ticker: prediction_market_request_label(req),
                stage: Some(req.provider.as_str().to_string()),
                message: err.to_string(),
            }),
        }
    }

    (series, errors)
}

fn prediction_market_request_label(req: &PredictionMarketRequest) -> String {
    match req.provider {
        PredictionMarketProvider::Kalshi => {
            format!("KALSHI:{}:{}", req.market, req.side.as_str().to_ascii_uppercase())
        }
        PredictionMarketProvider::Polymarket => format!(
            "POLYMARKET:{}:{}",
            req.market,
            req.side.as_str().to_ascii_uppercase()
        ),
    }
}

fn granularity_seconds(span: eli_core::finance::Span) -> i64 {
    span.approx_duration().num_seconds().max(60)
}

/// Kalshi's batch candlestick endpoint rejects any request whose window spans
/// more than 10,000 candles across all markets ("requested candlesticks across
/// all markets: 11520, max candlesticks: 10000"). One market per request, so a
/// page holds at most this many native bars; kept under the cap for headroom.
const KALSHI_MAX_CANDLES_PER_REQUEST: i64 = 9_000;
/// Hard ceiling on pages per series (~187 days of 1m bars). Beyond this a
/// sub-hour request is the wrong lens: use 1h.
const KALSHI_MAX_PAGES: i64 = 30;

/// One native Kalshi candlestick, already projected onto the requested side.
/// `traded` = the bar carried a trade print (`price.close_dollars` non-null);
/// otherwise o/h/l/c are the quote-derived fallback (yes bid/ask mid, then the
/// last trade before the bar).
#[derive(Clone, Copy, Debug, PartialEq)]
struct KalshiBar {
    end_ts: i64,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: Option<f64>,
    traded: bool,
}

/// Resample native Kalshi 1-minute bars (stamped by `end_ts`, the inclusive end
/// of the minute) into `step_secs` candles stamped at bucket START on absolute
/// UTC boundaries, with the same price semantics as Kalshi's native hourly bars:
/// - a bucket with at least one traded minute: open = first traded open,
///   high/low = traded extremes, close = last traded close (quote-only minutes
///   inside the bucket are ignored, exactly as the hourly `price` block does);
/// - a bucket with activity but no trades: flat at the last minute's quote
///   fallback (the hourly path's yes bid/ask mid), v = 0;
/// - a bucket with no native bar at all (Kalshi omits minutes where neither
///   trades nor quotes changed): flat carry-forward of the previous close,
///   v = 0, so thin stretches read as `{t, c}` after point-collapse;
/// Volume is summed. Buckets run from the first bar's bucket through the bucket
/// containing `end` (never past it); buckets ending before `start` are dropped
/// after they have seeded the carry-forward.
fn resample_kalshi_minute_bars(
    bars: &[KalshiBar],
    step_secs: i64,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Vec<eli_core::finance::Candle> {
    let step = step_secs.max(60);
    let mut sorted: Vec<KalshiBar> = bars.to_vec();
    // Traded bar first on a timestamp tie (e.g. the synthetic seed bar).
    sorted.sort_by_key(|b| (b.end_ts, !b.traded));
    sorted.dedup_by_key(|b| b.end_ts);

    // bucket start → bars in it (a 1m bar ending at T covers [T-60, T)).
    let mut buckets: BTreeMap<i64, Vec<KalshiBar>> = BTreeMap::new();
    for b in &sorted {
        let bar_start = b.end_ts - 60;
        buckets
            .entry(bar_start.div_euclid(step) * step)
            .or_default()
            .push(*b);
    }
    let Some((&first_bucket, _)) = buckets.iter().next() else {
        return Vec::new();
    };
    // First FULL bucket at/after `start` (a partial leading bucket would carry
    // only a few minutes); earlier buckets still seed the carry-forward.
    let start_bucket = (start.timestamp() + step - 1).div_euclid(step) * step;
    let last_bucket = (end.timestamp() - 1).div_euclid(step) * step;

    let mut out = Vec::new();
    let mut prev_close: Option<f64> = None;
    let mut t = first_bucket;
    while t <= last_bucket {
        let candle = match buckets.get(&t) {
            Some(in_bucket) => {
                let traded: Vec<&KalshiBar> = in_bucket.iter().filter(|b| b.traded).collect();
                let vol: Option<f64> = {
                    let vs: Vec<f64> = in_bucket.iter().filter_map(|b| b.v).collect();
                    (!vs.is_empty()).then(|| vs.iter().sum())
                };
                if let (Some(first), Some(last)) = (traded.first(), traded.last()) {
                    let h = traded.iter().map(|b| b.h).fold(f64::MIN, f64::max);
                    let l = traded.iter().map(|b| b.l).fold(f64::MAX, f64::min);
                    Some((first.o, h, l, last.c, vol))
                } else {
                    // Quote-only bucket: flat at the last quote fallback.
                    let c = in_bucket.last().map(|b| b.c).unwrap_or_default();
                    Some((c, c, c, c, Some(vol.unwrap_or(0.0))))
                }
            }
            None => prev_close.map(|c| (c, c, c, c, Some(0.0))),
        };
        if let Some((o, h, l, c, v)) = candle {
            prev_close = Some(c);
            if t >= start_bucket {
                if let Some(ts) = chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0) {
                    out.push(eli_core::finance::Candle {
                        t: ts,
                        o,
                        h,
                        l,
                        c,
                        v,
                        kind: None,
                    });
                }
            }
        }
        t += step;
    }
    out
}

const KALSHI_TRADE_API_URL: &str = "https://api.elections.kalshi.com/trade-api/v2";
/// The batch endpoint takes up to 100 tickers per call.
const KALSHI_BATCH_MAX_TICKERS: usize = 100;
/// Legs returned when a bare EVENT ticker is expanded (deepest first). A BTC
/// strike ladder has ~190 legs; 30 already spans the traded distribution.
const KALSHI_EVENT_MAX_LEGS: usize = 30;

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiBidAsk {
    // Live candles say close_dollars; the historical archive says close.
    #[serde(default, alias = "close")]
    close_dollars: Option<String>,
}

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiPrice {
    #[serde(default, alias = "open")]
    open_dollars: Option<String>,
    #[serde(default, alias = "low")]
    low_dollars: Option<String>,
    #[serde(default, alias = "high")]
    high_dollars: Option<String>,
    #[serde(default, alias = "close")]
    close_dollars: Option<String>,
    #[serde(default, alias = "previous")]
    previous_dollars: Option<String>,
}

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiCandle {
    end_period_ts: i64,
    #[serde(default)]
    price: Option<KalshiPrice>,
    #[serde(default)]
    yes_bid: Option<KalshiBidAsk>,
    #[serde(default)]
    yes_ask: Option<KalshiBidAsk>,
    // Live candles carry the fixed-point string `volume_fp` ("2963.10"
    // contracts); the archive carries `volume` as a string, older payloads
    // as an integer.
    #[serde(default)]
    volume_fp: Option<String>,
    #[serde(default)]
    volume: Option<serde_json::Value>,
}

impl KalshiCandle {
    fn volume_contracts(&self) -> Option<f64> {
        self.volume_fp.as_deref().and_then(parse_decimal).or_else(|| {
            match self.volume.as_ref()? {
                serde_json::Value::Number(n) => n.as_f64(),
                serde_json::Value::String(s) => parse_decimal(s),
                _ => None,
            }
        })
    }
}

/// How a requested window maps onto Kalshi's native intervals.
#[derive(Clone, Copy, Debug, PartialEq)]
struct KalshiWindow {
    start_ts: i64,
    end_ts: i64,
    requested_seconds: i64,
    api_interval_minutes: i64,
    needs_resample: bool,
    minute_path: bool,
}

impl KalshiWindow {
    fn page_span_secs(&self) -> i64 {
        KALSHI_MAX_CANDLES_PER_REQUEST * self.api_interval_minutes * 60
    }

    /// Native candles one ticker needs per page (bounded by the window).
    fn candles_per_ticker_page(&self) -> i64 {
        let span = (self.end_ts - self.start_ts).max(1).min(self.page_span_secs());
        span / (self.api_interval_minutes * 60) + 2
    }

    /// Tickers per batch call so that tickers × candles stays under the cap.
    fn tickers_per_call(&self) -> usize {
        ((KALSHI_MAX_CANDLES_PER_REQUEST / self.candles_per_ticker_page().max(1)).max(1) as usize)
            .min(KALSHI_BATCH_MAX_TICKERS)
    }
}

fn kalshi_window(
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<KalshiWindow> {
    let requested_seconds = granularity_seconds(granularity);
    // Kalshi's candlestick API supports period_interval = 1 (1m), 60 (1h) and
    // 1440 (1d); other values silently degrade to daily bars. Fetch at the
    // finest native interval at or below the target and resample locally.
    //   sub-hour (1m/5m/15m/30m): native 1m, paged under the 10k-candle cap;
    //     5m/15m/30m resample with hourly-consistent trade semantics.
    //   1h: native. 2h/4h/...: native 1h resampled. ≥1d: native daily.
    let (api_interval_minutes, needs_resample) = if requested_seconds >= 86400 {
        (1440_i64, false)
    } else if requested_seconds < 3600 {
        (1_i64, requested_seconds > 60)
    } else if requested_seconds == 3600 {
        (60_i64, false)
    } else {
        (60_i64, true)
    };
    let w = KalshiWindow {
        start_ts: start.timestamp(),
        end_ts: end.timestamp(),
        requested_seconds,
        api_interval_minutes,
        needs_resample,
        minute_path: api_interval_minutes == 1,
    };
    let pages_needed = ((w.end_ts - w.start_ts).max(1) + w.page_span_secs() - 1) / w.page_span_secs();
    if pages_needed > KALSHI_MAX_PAGES {
        anyhow::bail!(
            "kalshi {}m candles over this window need {} requests (cap {}); use --granularity 1h or a shorter --range",
            api_interval_minutes,
            pages_needed,
            KALSHI_MAX_PAGES
        );
    }
    Ok(w)
}

fn kalshi_ts_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("init kalshi candlestick client")
}

/// Live candles for many markets: the batch endpoint, chunked so each call
/// stays under Kalshi's 10k-candle cap, paged over time. A market absent
/// from the map was unknown to the live endpoint (settled before the
/// historical cutoff, an event ticker, or a typo).
async fn kalshi_fetch_live_candles(
    client: &reqwest::Client,
    tickers: &[String],
    w: &KalshiWindow,
) -> Result<std::collections::HashMap<String, Vec<KalshiCandle>>> {
    #[derive(serde::Deserialize)]
    struct BatchResp {
        #[serde(default)]
        markets: Vec<BatchMarket>,
    }
    #[derive(serde::Deserialize)]
    struct BatchMarket {
        market_ticker: String,
        #[serde(default)]
        candlesticks: Vec<KalshiCandle>,
    }

    let mut out: std::collections::HashMap<String, Vec<KalshiCandle>> =
        std::collections::HashMap::new();
    let interval_s = w.api_interval_minutes.to_string();
    for chunk in tickers.chunks(w.tickers_per_call()) {
        let joined = chunk.join(",");
        let mut page_start = w.start_ts;
        while page_start <= w.end_ts {
            let page_end = (page_start + w.page_span_secs() - 1).min(w.end_ts);
            let mut query = vec![
                ("market_tickers", joined.clone()),
                ("start_ts", page_start.to_string()),
                ("end_ts", page_end.to_string()),
                ("period_interval", interval_s.clone()),
            ];
            // Minute bars are sparse (Kalshi omits minutes with no trade and
            // no quote change), so seed the window with the last real bar
            // before it: Kalshi prepends a synthetic bar whose `previous` is
            // that bar's close.
            if w.minute_path && page_start == w.start_ts {
                query.push(("include_latest_before_start", "true".to_string()));
            }
            let body = kalshi_get_json_opt(client, KALSHI_CANDLESTICKS_URL, &query, "kalshi candlesticks")
                .await
                .map_err(|e| anyhow::anyhow!(e))?
                .unwrap_or_default();
            let parsed: BatchResp =
                serde_json::from_value(body).context("parse kalshi candlesticks response")?;
            for m in parsed.markets {
                out.entry(m.market_ticker).or_default().extend(m.candlesticks);
            }
            page_start = page_end + 1;
        }
    }
    Ok(out)
}

/// Candles from the historical archive (markets settled before Kalshi's
/// historical cutoff; the live endpoints no longer know them). None = the
/// archive does not have this ticker either.
async fn kalshi_fetch_historical_candles(
    client: &reqwest::Client,
    ticker: &str,
    w: &KalshiWindow,
) -> Result<Option<Vec<KalshiCandle>>> {
    #[derive(serde::Deserialize)]
    struct HistResp {
        #[serde(default)]
        candlesticks: Vec<KalshiCandle>,
    }
    let url = format!("{KALSHI_TRADE_API_URL}/historical/markets/{ticker}/candlesticks");
    let interval_s = w.api_interval_minutes.to_string();
    let mut out: Vec<KalshiCandle> = Vec::new();
    let mut page_start = w.start_ts;
    while page_start <= w.end_ts {
        let page_end = (page_start + w.page_span_secs() - 1).min(w.end_ts);
        let query = vec![
            ("start_ts", page_start.to_string()),
            ("end_ts", page_end.to_string()),
            ("period_interval", interval_s.clone()),
        ];
        let Some(body) = kalshi_get_json_opt(client, &url, &query, "kalshi historical candlesticks")
            .await
            .map_err(|e| anyhow::anyhow!(e))?
        else {
            return Ok(None);
        };
        let parsed: HistResp =
            serde_json::from_value(body).context("parse kalshi historical candlesticks")?;
        out.extend(parsed.candlesticks);
        page_start = page_end + 1;
    }
    Ok(Some(out))
}

/// Legs of a Kalshi EVENT ticker: the deepest KALSHI_EVENT_MAX_LEGS, in
/// Kalshi's order. Settled events
/// older than the cutoff have no nested markets; their legs come from the
/// archive. Returns (kept legs, total legs); None = not an event.
async fn kalshi_event_legs(
    client: &reqwest::Client,
    event_ticker: &str,
) -> Result<Option<(Vec<String>, usize)>> {
    #[derive(serde::Deserialize)]
    struct Leg {
        ticker: String,
        #[serde(default)]
        volume_fp: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct EventResp {
        #[serde(default)]
        markets: Vec<Leg>,
    }
    #[derive(serde::Deserialize)]
    struct MarketsResp {
        #[serde(default)]
        markets: Vec<Leg>,
    }
    let url = format!("{KALSHI_TRADE_API_URL}/events/{event_ticker}");
    let Some(body) = kalshi_get_json_opt(client, &url, &[], "kalshi event")
        .await
        .map_err(|e| anyhow::anyhow!(e))?
    else {
        return Ok(None);
    };
    let mut legs = serde_json::from_value::<EventResp>(body)
        .context("parse kalshi event")?
        .markets;
    if legs.is_empty() {
        let url = format!("{KALSHI_TRADE_API_URL}/historical/markets");
        let query = vec![
            ("event_ticker", event_ticker.to_string()),
            ("limit", "1000".to_string()),
        ];
        if let Some(body) = kalshi_get_json_opt(client, &url, &query, "kalshi historical markets")
            .await
            .map_err(|e| anyhow::anyhow!(e))?
        {
            legs = serde_json::from_value::<MarketsResp>(body)
                .context("parse kalshi historical markets")?
                .markets;
        }
    }
    let total = legs.len();
    let vol = |l: &Leg| l.volume_fp.as_deref().and_then(parse_decimal).unwrap_or(0.0);
    // Keep the deepest legs, then restore Kalshi's own (strike) order.
    let mut ranked: Vec<(usize, Leg)> = legs.into_iter().enumerate().collect();
    ranked.sort_by(|a, b| vol(&b.1).partial_cmp(&vol(&a.1)).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(KALSHI_EVENT_MAX_LEGS);
    ranked.sort_by_key(|(idx, _)| *idx);
    Ok(Some((ranked.into_iter().map(|(_, l)| l.ticker).collect(), total)))
}

/// Project native candles onto the requested side and window.
fn kalshi_series_from_candles(
    req: &PredictionMarketRequest,
    market_ticker: &str,
    raw_candles: Vec<KalshiCandle>,
    w: &KalshiWindow,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Result<eli_core::finance::TickerSeries> {
    let mut bars: Vec<KalshiBar> = Vec::new();
    for kc in raw_candles {
        let traded_close = kc
            .price
            .as_ref()
            .and_then(|p| p.close_dollars.as_deref())
            .and_then(parse_decimal);
        let yes_close = traded_close
            .or_else(|| {
                match (
                    kc.yes_bid
                        .as_ref()
                        .and_then(|v| v.close_dollars.as_deref())
                        .and_then(parse_decimal),
                    kc.yes_ask
                        .as_ref()
                        .and_then(|v| v.close_dollars.as_deref())
                        .and_then(parse_decimal),
                ) {
                    (Some(bid), Some(ask)) => Some((bid + ask) * 0.5),
                    (Some(v), None) | (None, Some(v)) => Some(v),
                    (None, None) => None,
                }
            })
            .or_else(|| {
                kc.price
                    .as_ref()
                    .and_then(|p| p.previous_dollars.as_deref())
                    .and_then(parse_decimal)
            });

        let Some(yes_close) = yes_close else {
            continue;
        };

        let yes_open = kc
            .price
            .as_ref()
            .and_then(|p| p.open_dollars.as_deref())
            .and_then(parse_decimal)
            .unwrap_or(yes_close);
        let yes_high = kc
            .price
            .as_ref()
            .and_then(|p| p.high_dollars.as_deref())
            .and_then(parse_decimal)
            .unwrap_or(yes_close);
        let yes_low = kc
            .price
            .as_ref()
            .and_then(|p| p.low_dollars.as_deref())
            .and_then(parse_decimal)
            .unwrap_or(yes_close);

        // Project onto the requested side. NO = 1 - YES flips the range, so
        // re-take max/min after projecting (YES high is NO low).
        let (a, b) = (req.side.apply(yes_high), req.side.apply(yes_low));
        bars.push(KalshiBar {
            end_ts: kc.end_period_ts,
            o: req.side.apply(yes_open),
            h: a.max(b),
            l: a.min(b),
            c: req.side.apply(yes_close),
            v: kc.volume_contracts(),
            traded: traded_close.is_some(),
        });
    }

    if bars.is_empty() {
        anyhow::bail!(
            "no usable kalshi candlesticks found for market {} in requested window",
            market_ticker
        );
    }

    bars.sort_by_key(|b| (b.end_ts, !b.traded));
    bars.dedup_by_key(|b| b.end_ts);

    let candles: Vec<eli_core::finance::Candle> = if w.minute_path {
        // Native 1m and resampled 5m/15m/30m share one path: start-of-bar
        // stamps on absolute UTC boundaries, gaps carried forward.
        resample_kalshi_minute_bars(&bars, w.requested_seconds, start, end)
    } else {
        let mut candles: Vec<eli_core::finance::Candle> = Vec::with_capacity(bars.len());
        for b in &bars {
            let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(b.end_ts, 0)
                .ok_or_else(|| anyhow::anyhow!("invalid kalshi candlestick timestamp"))?;
            candles.push(eli_core::finance::Candle {
                t: ts,
                o: b.o,
                h: b.h,
                l: b.l,
                c: b.c,
                v: b.v,
                kind: None,
            });
        }
        if w.needs_resample {
            // Kalshi timestamps are end-of-bar (the API field is `end_period_ts`).
            // Convert to start-of-bar ONLY for resampling so that the absolute-clock
            // resampler places each bar in the correct bucket.  We do NOT touch
            // timestamps for native 1h or 1d output — those are correct as-is.
            let native_step_secs = w.api_interval_minutes * 60;
            for c in &mut candles {
                c.t = c.t - chrono::Duration::seconds(native_step_secs);
            }
            let step = chrono::Duration::seconds(w.requested_seconds);
            candles = eli_core::finance::resample_candles(&candles, start, step);
        }
        candles
    };

    if candles.is_empty() {
        anyhow::bail!(
            "no usable kalshi candlesticks found for market {} in requested window",
            market_ticker
        );
    }

    Ok(eli_core::finance::TickerSeries {
        ticker: format!(
            "KALSHI:{}:{}",
            market_ticker,
            req.side.as_str().to_ascii_uppercase()
        ),
        candles,
        source: Some("kalshi".to_string()),
        upstream_id: Some(market_ticker.to_string()),
    })
}

/// Every Kalshi request in one pass: live markets through the batch endpoint
/// (≤100 per call), then for ids the live endpoint does not know: the
/// historical archive (markets settled before the cutoff), then event
/// expansion (a bare event ticker returns its deepest legs).
async fn fetch_kalshi_series_batch(
    requests: &[PredictionMarketRequest],
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> (
    Vec<eli_core::finance::TickerSeries>,
    Vec<eli_core::finance::TimeseriesError>,
) {
    let mut series = Vec::new();
    let mut errors = Vec::new();
    let fail_all = |errors: &mut Vec<eli_core::finance::TimeseriesError>, msg: String| {
        for req in requests {
            errors.push(eli_core::finance::TimeseriesError {
                ticker: prediction_market_request_label(req),
                stage: Some("kalshi".to_string()),
                message: msg.clone(),
            });
        }
    };
    let w = match kalshi_window(start, end, granularity) {
        Ok(w) => w,
        Err(e) => {
            fail_all(&mut errors, e.to_string());
            return (series, errors);
        }
    };
    let client = match kalshi_ts_client() {
        Ok(c) => c,
        Err(e) => {
            fail_all(&mut errors, e.to_string());
            return (series, errors);
        }
    };

    let mut tickers: Vec<String> = Vec::new();
    for req in requests {
        if !tickers.contains(&req.market) {
            tickers.push(req.market.clone());
        }
    }
    let mut live = match kalshi_fetch_live_candles(&client, &tickers, &w).await {
        Ok(map) => map,
        Err(e) => {
            fail_all(&mut errors, format!("kalshi candlesticks: {e}"));
            return (series, errors);
        }
    };

    // Ids the live endpoint did not return: archive, then event expansion.
    let mut archived: std::collections::HashMap<String, Vec<KalshiCandle>> =
        std::collections::HashMap::new();
    let mut expanded: std::collections::HashMap<String, (Vec<String>, usize)> =
        std::collections::HashMap::new();
    let mut unknown: HashSet<String> = HashSet::new();
    for ticker in tickers.iter().filter(|t| !live.contains_key(*t)) {
        match kalshi_fetch_historical_candles(&client, ticker, &w).await {
            Ok(Some(candles)) => {
                archived.insert(ticker.clone(), candles);
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                errors.push(eli_core::finance::TimeseriesError {
                    ticker: format!("KALSHI:{ticker}"),
                    stage: Some("kalshi_historical".to_string()),
                    message: e.to_string(),
                });
                continue;
            }
        }
        match kalshi_event_legs(&client, ticker).await {
            Ok(Some((legs, total))) if !legs.is_empty() => {
                expanded.insert(ticker.clone(), (legs, total));
            }
            Ok(_) => {
                unknown.insert(ticker.clone());
            }
            Err(e) => {
                errors.push(eli_core::finance::TimeseriesError {
                    ticker: format!("KALSHI:{ticker}"),
                    stage: Some("kalshi_event".to_string()),
                    message: e.to_string(),
                });
            }
        }
    }

    // Event legs: live batch first, archive for the ones it lacks.
    let leg_tickers: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for (legs, _) in expanded.values() {
            for l in legs {
                if !v.contains(l) && !live.contains_key(l) {
                    v.push(l.clone());
                }
            }
        }
        v
    };
    if !leg_tickers.is_empty() {
        match kalshi_fetch_live_candles(&client, &leg_tickers, &w).await {
            Ok(map) => live.extend(map),
            Err(e) => errors.push(eli_core::finance::TimeseriesError {
                ticker: "KALSHI:event_legs".to_string(),
                stage: Some("kalshi".to_string()),
                message: format!("kalshi candlesticks: {e}"),
            }),
        }
        for leg in leg_tickers.iter().filter(|l| !live.contains_key(*l)) {
            if let Ok(Some(candles)) = kalshi_fetch_historical_candles(&client, leg, &w).await {
                archived.insert(leg.clone(), candles);
            }
        }
    }

    let push = |req: &PredictionMarketRequest,
                    market: &str,
                    raw: Vec<KalshiCandle>,
                    series: &mut Vec<eli_core::finance::TickerSeries>,
                    errors: &mut Vec<eli_core::finance::TimeseriesError>| {
        match kalshi_series_from_candles(req, market, raw, &w, start, end) {
            Ok(s) => series.push(s),
            Err(e) => errors.push(eli_core::finance::TimeseriesError {
                ticker: format!("KALSHI:{}:{}", market, req.side.as_str().to_ascii_uppercase()),
                stage: Some("kalshi".to_string()),
                message: e.to_string(),
            }),
        }
    };
    for req in requests {
        let ticker = req.market.as_str();
        if let Some(raw) = live.get(ticker).or_else(|| archived.get(ticker)) {
            push(req, ticker, raw.clone(), &mut series, &mut errors);
        } else if let Some((legs, total)) = expanded.get(ticker) {
            for leg in legs {
                let raw = live.get(leg).or_else(|| archived.get(leg)).cloned().unwrap_or_default();
                push(req, leg, raw, &mut series, &mut errors);
            }
            if *total > legs.len() {
                errors.push(eli_core::finance::TimeseriesError {
                    ticker: prediction_market_request_label(req),
                    stage: Some("kalshi_event_legs".to_string()),
                    message: format!(
                        "event {ticker} has {total} legs; returned the {} with the most volume (pass leg tickers from finance_odds for others)",
                        legs.len()
                    ),
                });
            }
        } else if unknown.contains(ticker) {
            errors.push(eli_core::finance::TimeseriesError {
                ticker: prediction_market_request_label(req),
                stage: Some("kalshi".to_string()),
                message: format!(
                    "kalshi has no market, event or archived market named {ticker} (ids come from finance_odds markets[].ticker)"
                ),
            });
        }
    }
    (series, errors)
}

async fn fetch_kalshi_market_series(
    req: &PredictionMarketRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<eli_core::finance::TickerSeries> {
    let (series, errors) =
        fetch_kalshi_series_batch(std::slice::from_ref(req), start, end, granularity).await;
    let wanted = format!("KALSHI:{}:", req.market);
    if let Some(s) = series.into_iter().find(|s| s.ticker.starts_with(&wanted)) {
        return Ok(s);
    }
    Err(anyhow::anyhow!(errors
        .into_iter()
        .next()
        .map(|e| e.message)
        .unwrap_or_else(|| format!("kalshi returned no candlesticks for market {}", req.market))))
}

fn parse_decimal(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn parse_jsonish_string_list(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(json_value_to_string)
            .filter(|s| !s.trim().is_empty())
            .collect(),
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Vec::new();
            }
            if trimmed.starts_with('[') {
                if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
                    return parsed
                        .iter()
                        .map(json_value_to_string)
                        .filter(|v| !v.trim().is_empty())
                        .collect();
                }
            }
            vec![trimmed.to_string()]
        }
        other => vec![json_value_to_string(other)],
    }
}

fn json_value_to_string(value: &serde_json::Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if let Some(i) = value.as_i64() {
        return i.to_string();
    }
    if let Some(u) = value.as_u64() {
        return u.to_string();
    }
    if let Some(f) = value.as_f64() {
        return f.to_string();
    }
    value.to_string()
}

fn parse_json_number_i64(value: &serde_json::Value) -> Option<i64> {
    if let Some(v) = value.as_i64() {
        return Some(v);
    }
    if let Some(v) = value.as_u64() {
        return i64::try_from(v).ok();
    }
    value.as_str().and_then(|s| s.trim().parse::<i64>().ok())
}

fn parse_json_number_f64(value: &serde_json::Value) -> Option<f64> {
    if let Some(v) = value.as_f64() {
        return Some(v);
    }
    if let Some(v) = value.as_i64() {
        return Some(v as f64);
    }
    if let Some(v) = value.as_u64() {
        return Some(v as f64);
    }
    value.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}

fn expand_timeseries_preset(preset: &str) -> Result<Vec<String>> {
    let tickers: Vec<&str> = match preset {
        "macro" => vec![
            // Inflation
            "CPIAUCSL", "CPILFESL", "PCEPILFE", "PPIACO", "T10YIE",
            // Employment
            "UNRATE", "PAYEMS", "ICSA", "JTSJOL",
            // GDP
            "GDPC1", "INDPRO", "GDPNOW", "PCENOW",
            // Rates
            "FEDFUNDS", "DGS2", "DGS10", "DGS30", "T10Y2Y", "DFII10", "MORTGAGE30US",
            // Debt
            "GFDEGDQ188S", "FYGFGDQ188S", "GFDEBTN",
            // Money
            "M2SL", "WALCL",
            // Consumer
            "UMCSENT", "RSAFS", "PSAVERT", "CSUSHPISA", "HOUST", "TOTALSA",
            // Credit
            "BAMLH0A0HYM2",
            // Commodities
            "DCOILWTICO", "DTWEXBGS",
            // Live 24/7: the Kraken Futures index (oil and gold are Pyth's
            // 24/7 index as Kraken lists it).
            "KF:OIL", "KF:GOLD", "KF:BTC",
        ],
        "forex_majors" => vec![
            "EURUSD=X", "GBPUSD=X", "USDJPY=X", "USDCHF=X", "USDCAD=X",
            "AUDUSD=X", "NZDUSD=X", "USDSEK=X", "USDNOK=X",
        ],
        "yield_curve" => vec![
            "DGS1MO", "DGS3MO", "DGS6MO", "DGS1", "DGS2", "DGS3",
            "DGS5", "DGS7", "DGS10", "DGS20", "DGS30",
        ],
        "liquidity" => vec![
            "WALCL", "WTREGEN", "RRPONTSYD",
        ],
        // The Kraken Futures index, 24/7, one-minute history since 2022-03.
        "crypto" => vec![
            "KF:BTC", "KF:ETH", "KF:SOL",
        ],
        // Global sovereign yields via CNBC (GOVT: prefix): the 10Y anchors of every
        // curve that feeds the US Treasury term premium, plus the 2s/30s that show
        // whether a selloff is policy (front end) or fiscal/term premium (long end).
        "sovereigns" | "govt" | "global_yields" => vec![
            "GOVT:US2Y", "GOVT:US10Y", "GOVT:US30Y",
            "GOVT:DE2Y", "GOVT:DE10Y", "GOVT:DE30Y",
            "GOVT:FR2Y", "GOVT:FR10Y", "GOVT:FR30Y",
            "GOVT:IT10Y", "GOVT:ES10Y",
            "GOVT:GB10Y", "GOVT:GB30Y",
            "GOVT:JP10Y", "GOVT:JP30Y",
            "GOVT:CA10Y", "GOVT:AU10Y", "GOVT:CH10Y", "GOVT:CN10Y",
        ],
        // Funding / credit plumbing stress across the newer no-key providers:
        // private marks vs public price (PNAV), single-name AI-capex credit
        // (FINRA TRACE yields), Treasury buyback demand, repo tails, OFR stress,
        // HY + SPY creations/redemptions, VIX curve, equity put/call, crypto
        // leverage + vol, and CCC OAS as the anchor.
        "stress_plumbing" | "plumbing" => vec![
            "PNAV:OBDC", "PNAV:FSK", "PNAV:BXSL", "PNAV:ARCC",
            "BOND:21873SAG3:YLD",   // CoreWeave 9.75% 2031
            "BOND:68389XEC5:YLD",   // Oracle 6.85% 2066
            "BUYBACK:10-20Y:COVER", "BUYBACK:20-30Y:COVER",
            "NYFED:SRF", "NYFED:SOFR_P99",
            "OFR:FSI", "OFR:REPO-DVP_OV_TOT-P",
            "ETF:JNK:FLOW", "ETF:SPY:FLOW",
            "CBOE:VXCONTANGO", "CBOE:PCR_EQUITY",
            "HL:BTC:FUNDING_APR", "DERIBIT:DVOL:BTC",
            "FRED:BAMLH0A3HYC",
        ],
        // Credit spreads: ICE BofA OAS indices (daily, free, no API key)
        "credit" | "credit_spreads" | "spreads" => vec![
            "BAMLC0A0CM",     // IG Corporate OAS
            "BAMLC0A4CBBB",   // BBB Corporate OAS
            "BAMLH0A0HYM2",   // High Yield OAS
            "BAMLH0A1HYBB",   // BB High Yield OAS
            "BAMLH0A2HYB",    // B High Yield OAS
            "BAMLH0A3HYC",    // CCC & Lower OAS
            "BAMLEMCBPIOAS",   // EM Corporate OAS
        ],
        // Private credit / BDC complex: listed BDC equities as the liquid
        // proxy for private-credit stress, CLO tranche ETFs for the funding
        // stack, plus HY-vs-CCC public-credit reference points. The chain
        // (BDC tape + CCC OAS + credit ETFs) kept being assembled by hand in
        // research sessions — frozen here as a preset.
        "private_credit" | "bdc" => vec![
            "ARCC",           // Ares Capital (largest BDC)
            "OBDC",           // Blue Owl Capital Corp
            "FSK",            // FS KKR Capital
            "HTGC",           // Hercules Growth Capital (venture debt)
            "MAIN",           // Main Street Capital
            "BXSL",           // Blackstone Secured Lending
            "CLOI",           // VanEck CLO ETF (IG tranches)
            "JBBB",           // Janus Henderson B-BBB CLO ETF (mezz)
            "JAAA",           // Janus Henderson AAA CLO ETF
            "HYG",            // HY corporate bond ETF reference
            "BKLN",           // Leveraged loan ETF reference
            "BAMLH0A3HYC",    // CCC & Lower OAS (public-credit stress rung)
        ],
        // Financial conditions indices (weekly)
        "financial_conditions" | "conditions" | "nfci" => vec![
            "NFCI",           // Chicago Fed National Financial Conditions
            "ANFCI",          // Adjusted NFCI (removes biz cycle)
            "STLFSI4",        // St. Louis Financial Stress Index V4
            "VIXCLS",         // VIX daily close
        ],
        // Recession indicators
        "recession" | "recession_indicators" => vec![
            "SAHMREALTIME",   // Sahm Rule (real-time)
            "RECPROUSM156N",  // Smoothed Recession Probabilities (0-100%)
            "T10Y2Y",         // 10Y-2Y spread
            "T10Y3M",         // 10Y-3M spread (NY Fed model input)
            "ICSA",           // Initial claims (weekly)
            "UNRATE",         // Unemployment rate
            "INDPRO",         // Industrial production
            "CFNAI",          // Chicago Fed National Activity Index
        ],
        // Fed balance sheet and money supply
        "fed_balance_sheet" | "fed_bs" | "qe" | "qt" => vec![
            "WALCL",          // Fed total assets
            "TREAST",         // Fed Treasury holdings
            "WSHOMCB",        // Fed MBS holdings
            "WRBWFRBL",       // Reserve balances
            "RRPONTSYD",      // Reverse repo
            "WTREGEN",        // Treasury General Account
            "M2SL",           // M2 money supply
            "BOGMBASE",       // Monetary base
        ],
        // Housing market
        "housing" => vec![
            "CSUSHPISA",      // Case-Shiller National HPI
            "SPCS20RSA",      // Case-Shiller 20-City
            "HOUST",          // Housing starts
            "HOUST1F",        // Single-family starts
            "PERMIT",         // Building permits
            "HSN1F",          // New home sales
            "EXHOSLUSM495S",  // Existing home sales
            "MORTGAGE30US",   // 30Y mortgage rate
        ],
        // Labor market deep dive
        "labor" | "employment" => vec![
            "PAYEMS",         // Nonfarm payrolls
            "UNRATE",         // Unemployment rate
            "U6RATE",         // U-6 (broadest unemployment)
            "ICSA",           // Initial claims
            "CCSA",           // Continued claims
            "JTSJOL",         // JOLTS job openings
            "JTSQUL",         // JOLTS quits
            "CIVPART",        // Labor force participation
            "CES0500000003",  // Average hourly earnings
            "AWHAETP",        // Average weekly hours
        ],
        // Inflation deep dive
        "inflation" => vec![
            "CPIAUCSL",       // Headline CPI
            "CPILFESL",       // Core CPI
            "PCEPILFE",       // Core PCE (Fed's target)
            "PPIFIS",         // PPI Final Demand
            "T10YIE",         // 10Y breakeven inflation
            "T5YIFR",         // 5Y5Y forward inflation expectation
            "MICH",           // UMich 1Y inflation expectation
            "CORESTICKM157SFRBATL", // Atlanta Fed sticky CPI
            "MEDCPIM158SFRBCLE",    // Cleveland Fed median CPI
        ],
        // Real rates (TIPS yields)
        "real_rates" | "tips" => vec![
            "DFII5",          // 5Y TIPS yield
            "DFII7",          // 7Y TIPS yield
            "DFII10",         // 10Y TIPS yield
            "DFII20",         // 20Y TIPS yield
            "DFII30",         // 30Y TIPS yield
            "T5YIE",          // 5Y breakeven
            "T10YIE",         // 10Y breakeven
        ],
        // Consumer credit and banking
        "consumer_credit" | "banking" => vec![
            "TOTALSL",        // Total consumer credit
            "REVOLSL",        // Revolving (credit cards)
            "NONREVSL",       // Nonrevolving (auto/student)
            "DRCCLACBS",      // Credit card delinquency rate
            "DRSFRMACBS",     // Mortgage delinquency rate
            "BUSLOANS",       // C&I loans
            "DRTSCILM",      // SLOOS lending standards
        ],
        // Energy futures via IBKR (front month each)
        "energy" | "oil" => vec![
            "IBKR:FUT:CL:NYMEX",    // WTI Crude
            "IBKR:FUT:COIL:IPE",    // ICE Brent
            "IBKR:FUT:HO:NYMEX",    // Heating Oil
            "IBKR:FUT:RB:NYMEX",    // RBOB Gasoline
            "IBKR:FUT:NG:NYMEX",    // Natural Gas
            "IBKR:FUT:GOIL:IPE",    // ICE Gasoil
        ],
        // Broad commodities via IBKR
        "commodities" | "cmdty" => vec![
            "IBKR:FUT:CL:NYMEX",    // WTI Crude
            "IBKR:FUT:GC:COMEX",    // Gold
            "IBKR:FUT:SI:COMEX",    // Silver
            "IBKR:FUT:HG:COMEX",    // Copper
            "IBKR:FUT:ZC:CBOT",     // Corn
            "IBKR:FUT:ZW:CBOT",     // Wheat
            "IBKR:FUT:ZS:CBOT",     // Soybeans
            "IBKR:FUT:KC:NYBOT",    // Coffee
            "IBKR:FUT:CT:NYBOT",    // Cotton
            "IBKR:FUT:SB:NYBOT",    // Sugar
        ],
        // Treasury futures + VIX via IBKR
        "treasuries" | "treasury_futures" => vec![
            "IBKR:FUT:ZT:CBOT",     // 2-Year Note
            "IBKR:FUT:ZF:CBOT",     // 5-Year Note
            "IBKR:FUT:ZN:CBOT",     // 10-Year Note
            "IBKR:FUT:ZB:CBOT",     // 30-Year Bond
            "IBKR:FUT:VIX:CFE",     // VIX Futures
        ],
        other => anyhow::bail!(
            "unknown --preset '{other}' (run with preset=list to see all presets + their contents). Supported: macro, forex_majors, yield_curve, liquidity, crypto, sovereigns, stress_plumbing, credit, financial_conditions, recession, fed_balance_sheet, housing, labor, inflation, real_rates, consumer_credit, energy, commodities, treasuries. For futures curves, pass the root symbol directly as a ticker (e.g. --tickers CL,GC,ZN) and the tool auto-expands to the curve."
        ),
    };
    Ok(tickers.into_iter().map(String::from).collect())
}

/// Machine-readable catalog of every timeseries preset, returned by `preset=list`.
/// Preset names + one-line descriptions live here; the actual ticker expansion is pulled
/// from `expand_timeseries_preset` so the contents have a single source of truth. This is
/// the discovery path: presets were previously only learnable from the tool description.
fn list_timeseries_presets() -> serde_json::Value {
    // (canonical name, comma-separated aliases, one-line description)
    let catalog: &[(&str, &str, &str)] = &[
        ("macro", "", "34-series US macro dashboard: inflation, jobs, GDP, rates, debt, money, consumer, credit + live oil/gold/BTC (Kraken Futures index, 24/7)"),
        ("recession", "recession_indicators", "Recession gauges: Sahm rule, 10Y-2Y & 10Y-3M curves, claims, unemployment, industrial production, Chicago Fed NAI"),
        ("credit", "credit_spreads,spreads", "Full ICE BofA OAS stack: IG, BBB, HY, BB, B, CCC, EM corporate spreads"),
        ("private_credit", "bdc", "Private credit / BDC complex: ARCC, OBDC, FSK, HTGC, MAIN, BXSL + CLO ETFs (JAAA/JBBB/CLOI) + HYG/BKLN + CCC OAS"),
        ("financial_conditions", "conditions,nfci", "Financial conditions / stress: NFCI, ANFCI, St. Louis FSI, VIX"),
        ("inflation", "", "Inflation deep-dive: headline & core CPI, core PCE, PPI, breakevens, 5y5y forward, UMich, sticky & median CPI"),
        ("labor", "employment", "Labor market: payrolls, U-3/U-6, initial+continued claims, JOLTS openings/quits, participation, wages, hours"),
        ("housing", "", "Housing: Case-Shiller national + 20-city, starts, permits, new & existing sales, 30y mortgage rate"),
        ("yield_curve", "", "US Treasury par curve, 1mo through 30Y"),
        ("real_rates", "tips", "TIPS real yields 5/7/10/20/30Y + 5Y & 10Y breakevens"),
        ("consumer_credit", "banking", "Consumer credit & bank lending: total/revolving/nonrevolving, card & mortgage delinquency, C&I loans, SLOOS"),
        ("fed_balance_sheet", "fed_bs,qe,qt", "Fed plumbing: total assets, Treasury & MBS holdings, reserves, RRP, TGA, M2, monetary base"),
        ("liquidity", "", "Net-liquidity inputs: Fed total assets, Treasury General Account, ON RRP"),
        ("forex_majors", "", "Nine USD majors: EUR, GBP, JPY, CHF, CAD, AUD, NZD, SEK, NOK"),
        ("crypto", "", "BTC, ETH, SOL: the Kraken Futures index (24/7)"),
        ("sovereigns", "govt,global_yields", "Global sovereign yields (GOVT:, CNBC): US/DE/FR 2-10-30Y, IT/ES 10Y, UK & Japan 10/30Y, CA/AU/CH/CN 10Y — values are yields in %"),
        ("stress_plumbing", "plumbing", "Funding/credit plumbing (daily): BDC price/NAV (OBDC/FSK/BXSL/ARCC), CoreWeave + Oracle bond yields (TRACE), Treasury buyback cover 10-20Y/20-30Y, SRF usage + SOFR 99th pct, OFR FSI + DVP repo volume, JNK/SPY ETF flows, VIX contango, equity put/call, BTC perp funding + DVOL, CCC OAS"),
        ("energy", "oil", "Energy futures front-month via IBKR: WTI, Brent, heating oil, RBOB, natgas, gasoil (falls back to Yahoo =F when IBKR is down)"),
        ("commodities", "cmdty", "Broad commodity futures via IBKR: WTI, gold, silver, copper, corn, wheat, soybeans, coffee, cotton, sugar (Yahoo fallback)"),
        ("treasuries", "treasury_futures", "Treasury futures + VIX via IBKR: ZT, ZF, ZN, ZB, VIX (Yahoo fallback)"),
    ];
    let presets: Vec<serde_json::Value> = catalog
        .iter()
        .map(|(name, aliases, desc)| {
            let tickers = expand_timeseries_preset(name).unwrap_or_default();
            serde_json::json!({
                "preset": name,
                "aliases": aliases.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
                "description": desc,
                "n_tickers": tickers.len(),
                "tickers": tickers,
            })
        })
        .collect();
    serde_json::json!({
        "presets": presets,
        "count": presets.len(),
        "usage": "Pass preset=<name> to pull the whole basket. Composes with tickers in the same call (e.g. preset=macro + tickers=NVDA,SMH). IBKR-backed presets (energy/commodities/treasuries) auto-fall-back to Yahoo front-month when IB Gateway is down.",
    })
}

/// Map an IBKR:FUT:<root>:<exch> ticker to its Yahoo front-month continuous equivalent.
/// Keeps the energy/commodities/treasuries presets working when IB Gateway is down
/// (Yahoo futures are front-month continuous; for daily granularity the difference is
/// negligible). Returns None for IBKR tickers with no clean Yahoo proxy.
fn ibkr_to_yahoo_fallback(ibkr: &str) -> Option<&'static str> {
    let stripped = ibkr
        .strip_prefix("IBKR:FUT:")
        .or_else(|| ibkr.strip_prefix("ibkr:fut:"))?;
    let root = stripped.split(':').next()?;
    Some(match root.to_ascii_uppercase().as_str() {
        "CL" => "CL=F",
        "COIL" => "BZ=F",
        "HO" => "HO=F",
        "RB" => "RB=F",
        "NG" => "NG=F",
        "GOIL" => "BZ=F",
        "GC" => "GC=F",
        "SI" => "SI=F",
        "HG" => "HG=F",
        "ZC" => "ZC=F",
        "ZW" => "ZW=F",
        "ZS" => "ZS=F",
        "KC" => "KC=F",
        "CT" => "CT=F",
        "SB" => "SB=F",
        "ZT" => "ZT=F",
        "ZF" => "ZF=F",
        "ZN" => "ZN=F",
        "ZB" => "ZB=F",
        "VIX" => "^VIX",
        _ => return None,
    })
}

/// True when a timeseries fetch error indicates IB Gateway / TWS is unreachable
/// (vs. a genuine data error), so a preset can transparently fall back to Yahoo.
fn is_ibkr_unavailable(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("gateway")
        || m.contains("connection refused")
        || m.contains("reachable tws")
        || m.contains("no reachable")
        || m.contains("os error 61")
}

/// Heuristic: FRED series IDs are ALL-CAPS alphanumeric (plus underscore),
/// typically 3-20 chars, no dots/dashes/equals/carets that Yahoo tickers use.
/// Known FRED prefixes: DGS, BAML, FRED indicators like UNRATE, CPIAUCSL, etc.
/// Also accepts explicit `FRED:` / `fred:` prefix (stripped before classification).
fn is_fred_ticker(ticker: &str) -> bool {
    let t_trimmed = ticker.trim();
    // Strip explicit FRED:/fred: prefix before checking the bare ID against the heuristic.
    let t = t_trimmed
        .strip_prefix("FRED:")
        .or_else(|| t_trimmed.strip_prefix("fred:"))
        .unwrap_or(t_trimmed);
    if t.is_empty() || t.len() < 2 {
        return false;
    }
    // Pyth tickers handled separately
    if t.starts_with("PYTH:") {
        return false;
    }
    // Yahoo tickers contain special chars: = (futures), ^ (indices), - (crypto/classes), . (exchanges)
    if t.contains('=') || t.contains('^') || t.contains('.') {
        return false;
    }
    // BTC-USD, BRK-B etc are Yahoo — but BAMLH0A0HYM2 has no dash
    // Simple heuristic: if it contains a dash AND the part after dash is ≤3 chars, it's Yahoo
    if let Some(dash_pos) = t.find('-') {
        let suffix = &t[dash_pos + 1..];
        if suffix.len() <= 4 {
            return false; // BTC-USD, BRK-B, CL-F style
        }
    }
    // Known FRED patterns: all uppercase, alphanumeric + underscore, length 2-20
    let looks_fred = t.len() <= 25
        && t.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    // Distinguish short equity tickers from short all-alpha FRED macro series.
    // FRED series are otherwise >=6 chars (UNRATE, PAYEMS, INDPRO, GDPNOW) or
    // contain digits (DGS10, M2SL, T10Y2Y). Equity tickers are open-ended but
    // almost always <=5 all-alpha (SPY, QQQ, AAPL, GOOGL). So a short all-alpha
    // ticker is an equity (Yahoo) UNLESS it is one of the handful of short
    // all-alpha FRED series we recognize. This fixes GOOGL (was mis-routed to
    // FRED and returned empty) and also ICSA/CCSA/MICH (short FRED series the
    // old `len <= 4` rule wrongly sent to Yahoo).
    let all_alpha = t.chars().all(|c| c.is_ascii_uppercase());
    let has_digit = t.chars().any(|c| c.is_ascii_digit());
    const SHORT_FRED_ALPHA: &[&str] = &[
        "ICSA", "CCSA", "MICH", "CFNAI", "WALCL", "RSAFS", "HOUST", "PERMIT", "PPIFIS", "TREAST",
        "GDPNOW", "PCENOW", "UNRATE", "PAYEMS", "INDPRO", "JTSJOL", "JTSQUL",
    ];
    if all_alpha && !has_digit && t.len() <= 5 && !SHORT_FRED_ALPHA.contains(&t) {
        return false;
    }
    looks_fred
}

// ─────────────────────────────────────────────────────────────────────────────
// RATEPATH virtual provider
// ─────────────────────────────────────────────────────────────────────────────
//
// `RATEPATH:<MONTH><YEAR>:<bucket>` — historical probability path for a per-FOMC
// meeting outcome bucket, aggregated across constituent Polymarket+Kalshi
// markets and returned as a TickerSeries on the same multi-asset surface as
// stocks/FRED/Pyth/etc.
//
// Buckets: hold | cut | cut25 | cut50plus | hike
//
// Pipeline per ticker:
//   1. Parse → (year, month, bucket).
//   2. Resolve constituents at the live API (Kalshi event KXFEDDECISION-{YY}{MON}
//      and Polymarket event slug fed-decision-in-{month}{-suffix}).
//   3. For each constituent, fetch historical price candles with the existing
//      fetch_kalshi_market_series / fetch_polymarket_market_series helpers.
//   4. Filter constituents to the requested bucket (Kalshi by ticker suffix,
//      Polymarket by title classification).
//   5. Volume-weighted aggregate at each timestamp on the union grid; constituent
//      candle samples carry forward at each grid point until superseded by a
//      newer sample for that constituent, then mixed with weight = current vol.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RatePathBucket {
    Hold,
    Cut,        // Cut25 + Cut50Plus combined
    Cut25,
    Cut50Plus,
    Hike,
}

impl RatePathBucket {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hold" => Some(Self::Hold),
            "cut" => Some(Self::Cut),
            "cut25" | "cut_25" | "cut25bp" | "cut_25bp" => Some(Self::Cut25),
            "cut50plus" | "cut50p" | "cut50_plus" | "cut_50plus" | "cut50bp" | "cut50bp+" => {
                Some(Self::Cut50Plus)
            }
            "hike" => Some(Self::Hike),
            _ => None,
        }
    }

    /// Does the constituent's individual bucket contribute to the requested
    /// aggregate bucket? "Cut" rolls up Cut25 + Cut50Plus.
    fn includes(self, leaf: RatePathBucket) -> bool {
        match self {
            Self::Cut => matches!(leaf, Self::Cut25 | Self::Cut50Plus),
            other => other == leaf,
        }
    }
}

#[derive(Clone, Debug)]
struct RatePathTicker {
    raw: String,
    year_short: String, // "26"
    year_full: i32,     // 2026
    month_token: String, // "JUN"
    month_word_lower: String, // "june"
    bucket: RatePathBucket,
}

fn parse_ratepath_ticker(raw: &str) -> Option<RatePathTicker> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("RATEPATH:")
        .or_else(|| trimmed.strip_prefix("ratepath:"))?;
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let meeting = parts[0].to_ascii_uppercase();
    let bucket = RatePathBucket::parse(parts[1])?;
    if meeting.len() != 7 {
        return None;
    }
    let month_token = meeting.get(..3)?;
    let year_str = meeting.get(3..)?;
    let year_full: i32 = year_str.parse().ok()?;
    let month_word_lower = match month_token {
        "JAN" => "january",
        "FEB" => "february",
        "MAR" => "march",
        "APR" => "april",
        "MAY" => "may",
        "JUN" => "june",
        "JUL" => "july",
        "AUG" => "august",
        "SEP" => "september",
        "OCT" => "october",
        "NOV" => "november",
        "DEC" => "december",
        _ => return None,
    };
    let year_short = format!("{:02}", year_full % 100);
    Some(RatePathTicker {
        raw: trimmed.to_string(),
        year_short,
        year_full,
        month_token: month_token.to_string(),
        month_word_lower: month_word_lower.to_string(),
        bucket,
    })
}

/// Suffix codes encoded in Kalshi KXFEDDECISION-<YYMON>-<SUFFIX> tickers.
fn kalshi_suffix_to_bucket(suffix: &str) -> Option<RatePathBucket> {
    match suffix.to_ascii_uppercase().as_str() {
        "H0" => Some(RatePathBucket::Hold),
        "C25" => Some(RatePathBucket::Cut25),
        "C26" => Some(RatePathBucket::Cut50Plus),
        "H25" => Some(RatePathBucket::Hike),
        // H26 / H50 etc — anything that codes as "hike by >25bp" is still a Hike
        // for our 5-bucket mapping (we don't split hike granularity).
        s if s.starts_with('H') && s != "H0" => Some(RatePathBucket::Hike),
        _ => None,
    }
}

/// Title-based bucket classifier for Polymarket per-meeting markets.
/// Mirrors the logic in `eli-core/src/finance/rate_path/fetch/parse.rs::classify_bucket`
/// but suffix-free (Polymarket markets carry text titles, not suffix codes).
fn classify_polymarket_title(title: &str) -> Option<RatePathBucket> {
    let t = title.to_ascii_lowercase();
    if t.contains("hold") || t.contains("no change") || t.contains("maintain") {
        return Some(RatePathBucket::Hold);
    }
    if t.contains("hike") || t.contains("raise") || t.contains("increase") {
        return Some(RatePathBucket::Hike);
    }
    if t.contains("cut") || t.contains("decrease") || t.contains("lower") {
        // Polymarket variants: "50+ bps", "50 bp", "50bp", "0.50%", ">25bps", "half"
        if t.contains("50bp") || t.contains("50 bp") || t.contains("50+") || t.contains("0.50%")
            || t.contains(">25bps") || t.contains(">25 bps") || t.contains("half")
        {
            return Some(RatePathBucket::Cut50Plus);
        }
        return Some(RatePathBucket::Cut25);
    }
    None
}

#[derive(Clone, Debug)]
struct RatePathConstituent {
    request: PredictionMarketRequest,
    /// Constituent's leaf bucket (e.g. Cut25 for a -C25 Kalshi market). Kept on
    /// the struct for diagnostic round-trips even though aggregation only needs
    /// the (already-filtered) `request` + `volume`.
    #[allow(dead_code)]
    bucket: RatePathBucket,
    volume: i64,
}

const RATEPATH_KALSHI_MIN_VOLUME: i64 = 500;
const RATEPATH_POLYMARKET_MIN_VOLUME: i64 = 10_000;

async fn fetch_ratepath_constituents(
    client: &reqwest::Client,
    ticker: &RatePathTicker,
) -> Result<Vec<RatePathConstituent>> {
    let mut out: Vec<RatePathConstituent> = Vec::new();

    // ── Kalshi: KXFEDDECISION-{YY}{MON} ──
    let event_ticker = format!("KXFEDDECISION-{}{}", ticker.year_short, ticker.month_token);
    // Any status: a past meeting's path is the point of RATEPATH. Meetings
    // settled before Kalshi's historical cutoff are only in the archive.
    let mut kalshi_markets: Vec<serde_json::Value> = Vec::new();
    for kalshi_url in [
        format!("{KALSHI_TRADE_API_URL}/markets"),
        format!("{KALSHI_TRADE_API_URL}/historical/markets"),
    ] {
        let kalshi_resp = client
            .get(&kalshi_url)
            .query(&[("event_ticker", event_ticker.as_str()), ("limit", "20")])
            .venue_send()
            .await
            .with_context(|| format!("kalshi event lookup {event_ticker}"))?;
        if !kalshi_resp.status().is_success() {
            continue;
        }
        let body: serde_json::Value = kalshi_resp
            .json()
            .await
            .with_context(|| format!("kalshi event parse {event_ticker}"))?;
        kalshi_markets = body
            .get("markets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if !kalshi_markets.is_empty() {
            break;
        }
    }
    for m in &kalshi_markets {
        let market_ticker = m
            .get("ticker")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if market_ticker.is_empty() {
            continue;
        }
        let suffix = market_ticker.rsplit('-').next().unwrap_or("");
        let Some(leaf_bucket) = kalshi_suffix_to_bucket(suffix) else {
            continue;
        };
        if !ticker.bucket.includes(leaf_bucket) {
            continue;
        }
        let volume = m
            .get("volume_fp")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0) as i64;
        if volume < RATEPATH_KALSHI_MIN_VOLUME {
            continue;
        }
        out.push(RatePathConstituent {
            request: PredictionMarketRequest {
                provider: PredictionMarketProvider::Kalshi,
                market: market_ticker,
                side: PredictionMarketSide::Yes,
            },
            bucket: leaf_bucket,
            volume,
        });
    }

    // ── Polymarket: fed-decision-in-{month}[-N] ──
    // The exact slug suffix depends on Polymarket's internal discriminator; the
    // simplest reliable path is to iterate likely candidates.  June 2026 is
    // currently `fed-decision-in-june-825`, July 2026 is `fed-decision-in-july-181`,
    // others are bare `fed-decision-in-<month>`.  Try the slug list in order;
    // first hit with markets wins.
    let poly_slugs: Vec<String> = if ticker.month_word_lower == "june" {
        vec!["fed-decision-in-june-825".to_string(), "fed-decision-in-june".to_string()]
    } else if ticker.month_word_lower == "july" {
        vec!["fed-decision-in-july-181".to_string(), "fed-decision-in-july".to_string()]
    } else {
        vec![format!("fed-decision-in-{}", ticker.month_word_lower)]
    };

    for slug in &poly_slugs {
        let url = format!("{}/events?slug={}", POLYMARKET_GAMMA_URL, slug);
        let resp = match client.get(&url).venue_send().await {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            continue;
        };
        let event = match body.as_array().and_then(|a| a.first()) {
            Some(e) => e,
            None => continue,
        };
        let markets = event
            .get("markets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if markets.is_empty() {
            continue;
        }
        let mut added_any = false;
        for m in &markets {
            let title = m
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Skip joint multi-meeting compound markets.
            let lower = title.to_ascii_lowercase();
            if lower.contains("next three decisions")
                || lower.contains("pause–pause") || lower.contains("pause-pause")
                || lower.contains("cut–pause") || lower.contains("cut-pause")
            {
                continue;
            }
            // Title must reference the target meeting year (avoid 2027 contracts
            // sharing a slug).
            if !title.contains(&ticker.year_full.to_string()) {
                continue;
            }
            let Some(leaf_bucket) = classify_polymarket_title(&title) else {
                continue;
            };
            if !ticker.bucket.includes(leaf_bucket) {
                continue;
            }
            let volume = m
                .get("volumeNum")
                .and_then(|v| v.as_f64())
                .or_else(|| {
                    m.get("volume")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<f64>().ok())
                })
                .unwrap_or(0.0) as i64;
            if volume < RATEPATH_POLYMARKET_MIN_VOLUME {
                continue;
            }
            let market_id = m
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| m.get("conditionId").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            if market_id.is_empty() {
                continue;
            }
            out.push(RatePathConstituent {
                request: PredictionMarketRequest {
                    provider: PredictionMarketProvider::Polymarket,
                    market: market_id,
                    side: PredictionMarketSide::Yes,
                },
                bucket: leaf_bucket,
                volume,
            });
            added_any = true;
        }
        if added_any {
            break;
        }
    }

    Ok(out)
}

async fn fetch_ratepath_series_batch(
    tickers: &[String],
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> (
    Vec<eli_core::finance::TickerSeries>,
    Vec<eli_core::finance::TimeseriesError>,
) {
    let results = join_all(
        tickers
            .iter()
            .map(|t| fetch_ratepath_single_series(t, start, end, granularity)),
    )
    .await;

    let mut series = Vec::new();
    let mut errors = Vec::new();
    for (raw, result) in tickers.iter().zip(results) {
        match result {
            Ok(s) => series.push(s),
            Err(err) => errors.push(eli_core::finance::TimeseriesError {
                ticker: raw.clone(),
                stage: Some("rate_path".to_string()),
                message: err.to_string(),
            }),
        }
    }
    (series, errors)
}

async fn fetch_ratepath_single_series(
    raw: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<eli_core::finance::TickerSeries> {
    let parsed = parse_ratepath_ticker(raw)
        .ok_or_else(|| anyhow::anyhow!("invalid RATEPATH ticker '{raw}' (expected RATEPATH:<MON><YYYY>:<bucket>)"))?;

    // Constituent enumeration is one-shot; reuse the standard prediction-market
    // history fetchers for per-constituent candle pulls.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("init rate_path http client")?;

    let constituents = fetch_ratepath_constituents(&client, &parsed).await?;
    if constituents.is_empty() {
        anyhow::bail!(
            "no usable constituents for {raw} (event KXFEDDECISION-{}{} or fed-decision-in-{} returned no markets above volume floor)",
            parsed.year_short, parsed.month_token, parsed.month_word_lower
        );
    }

    // Fetch every constituent's history in parallel.
    let series_results = join_all(constituents.iter().map(|c| {
        // Each fetch returns a `TickerSeries` containing one constituent's
        // candle series.  We use Yes-side throughout — bucket aggregation is
        // additive on Yes probabilities.
        fetch_constituent_history(&c.request, start, end, granularity)
    }))
    .await;

    // Pair each successful fetch with its constituent metadata.
    let mut alive: Vec<(RatePathConstituent, eli_core::finance::TickerSeries)> = Vec::new();
    for (cons, res) in constituents.into_iter().zip(series_results.into_iter()) {
        if let Ok(s) = res {
            if !s.candles.is_empty() {
                alive.push((cons, s));
            }
        }
    }
    if alive.is_empty() {
        anyhow::bail!(
            "no constituent history available for {raw} in requested window"
        );
    }

    // Build union timestamp grid across all constituents.
    let mut ts_set: std::collections::BTreeSet<chrono::DateTime<chrono::Utc>> =
        std::collections::BTreeSet::new();
    for (_, s) in &alive {
        for c in &s.candles {
            ts_set.insert(c.t);
        }
    }
    let grid: Vec<chrono::DateTime<chrono::Utc>> = ts_set.into_iter().collect();
    if grid.is_empty() {
        anyhow::bail!("no timestamps in union grid for {raw}");
    }

    // For each constituent, build a sorted (ts, close) vector for forward-fill.
    struct ConsHistory {
        volume: i64,
        samples: Vec<(chrono::DateTime<chrono::Utc>, f64)>,
    }
    let mut cons_hist: Vec<ConsHistory> = Vec::with_capacity(alive.len());
    for (cons, s) in &alive {
        let mut samples: Vec<(chrono::DateTime<chrono::Utc>, f64)> = s
            .candles
            .iter()
            .map(|c| (c.t, c.c))
            .collect();
        samples.sort_by_key(|(t, _)| *t);
        cons_hist.push(ConsHistory {
            volume: cons.volume.max(0),
            samples,
        });
    }

    // For each grid timestamp, compute volume-weighted Yes probability.
    // Forward-fill: a constituent contributes its most-recent candle close at or
    // before the grid timestamp; constituents with no candle at-or-before are
    // excluded from that grid point.
    let mut candles: Vec<eli_core::finance::Candle> = Vec::with_capacity(grid.len());
    let mut indices: Vec<usize> = vec![0usize; cons_hist.len()];
    for grid_ts in &grid {
        let mut weighted_sum: f64 = 0.0;
        let mut weight_total: f64 = 0.0;
        for (i, h) in cons_hist.iter().enumerate() {
            // Advance index forward while the next sample is still <= grid_ts.
            while indices[i] + 1 < h.samples.len() && h.samples[indices[i] + 1].0 <= *grid_ts {
                indices[i] += 1;
            }
            // Skip constituents whose first sample is strictly after grid_ts.
            if h.samples[indices[i]].0 > *grid_ts {
                continue;
            }
            let p = h.samples[indices[i]].1;
            let w = h.volume as f64;
            if w <= 0.0 || !p.is_finite() {
                continue;
            }
            weighted_sum += p * w;
            weight_total += w;
        }
        if weight_total <= 0.0 {
            continue;
        }
        let p = (weighted_sum / weight_total).clamp(0.0, 1.0);
        candles.push(eli_core::finance::Candle {
            t: *grid_ts,
            o: p,
            h: p,
            l: p,
            c: p,
            v: None,
            kind: Some("point".to_string()),
        });
    }

    if candles.is_empty() {
        anyhow::bail!("rate_path aggregate empty for {raw}");
    }

    let upstream_id = format!(
        "{}{}:{}",
        parsed.month_token,
        parsed.year_full,
        match parsed.bucket {
            RatePathBucket::Hold => "hold",
            RatePathBucket::Cut => "cut",
            RatePathBucket::Cut25 => "cut25",
            RatePathBucket::Cut50Plus => "cut50plus",
            RatePathBucket::Hike => "hike",
        }
    );

    Ok(eli_core::finance::TickerSeries {
        ticker: parsed.raw.clone(),
        candles,
        source: Some("rate_path".to_string()),
        upstream_id: Some(upstream_id),
    })
}

/// Thin wrapper that routes a `PredictionMarketRequest` to the right fetch
/// helper. Lives next to `fetch_ratepath_constituents` so RATEPATH stays
/// independent of the higher-level `fetch_prediction_market_series` path
/// (which expects a slightly different ticker round-trip).
async fn fetch_constituent_history(
    req: &PredictionMarketRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<eli_core::finance::TickerSeries> {
    match req.provider {
        PredictionMarketProvider::Kalshi => {
            fetch_kalshi_market_series(req, start, end, granularity).await
        }
        PredictionMarketProvider::Polymarket => {
            fetch_polymarket_market_series(req, start, end, granularity).await
        }
    }
}

#[cfg(test)]
mod timeseries_tests {
    use super::*;
    use chrono::TimeZone;

    // 2026-11-03T23:00:00Z, a 15-minute boundary.
    const T0: i64 = 1_793_746_800;

    fn kbar(minute: i64, o: f64, h: f64, l: f64, c: f64, v: f64, traded: bool) -> KalshiBar {
        // A 1m bar covering [T0 + 60*minute, +60) is stamped at its END.
        KalshiBar { end_ts: T0 + 60 * (minute + 1), o, h, l, c, v: Some(v), traded }
    }

    fn quote(minute: i64, mid: f64) -> KalshiBar {
        kbar(minute, mid, mid, mid, mid, 0.0, false)
    }

    fn utc(ts: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.timestamp_opt(ts, 0).unwrap()
    }

    #[test]
    fn kalshi_resample_5m_uses_trades_only_and_sums_volume() {
        let bars = vec![
            quote(0, 0.50),                                 // quote-only: ignored for OHLC
            kbar(1, 0.52, 0.55, 0.51, 0.54, 10.0, true),
            kbar(3, 0.54, 0.60, 0.53, 0.58, 5.0, true),
            quote(4, 0.45),                                 // trailing quote: close stays last trade
        ];
        let out = resample_kalshi_minute_bars(&bars, 300, utc(T0), utc(T0 + 300));
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(c.t.timestamp(), T0, "stamped at bucket START");
        assert_eq!((c.o, c.h, c.l, c.c), (0.52, 0.60, 0.51, 0.58));
        assert_eq!(c.v, Some(15.0));
    }

    #[test]
    fn kalshi_resample_quote_only_bucket_is_flat_at_last_quote() {
        let bars = vec![quote(0, 0.40), quote(2, 0.42)];
        let out = resample_kalshi_minute_bars(&bars, 300, utc(T0), utc(T0 + 300));
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!((c.o, c.h, c.l, c.c, c.v), (0.42, 0.42, 0.42, 0.42, Some(0.0)));
    }

    #[test]
    fn kalshi_resample_carries_forward_missing_buckets_to_window_end() {
        // Trade in the first 5m bucket, then silence for 15 minutes.
        let bars = vec![kbar(2, 0.30, 0.31, 0.29, 0.31, 7.0, true)];
        let out = resample_kalshi_minute_bars(&bars, 300, utc(T0), utc(T0 + 1200));
        let ts: Vec<i64> = out.iter().map(|c| c.t.timestamp() - T0).collect();
        assert_eq!(ts, vec![0, 300, 600, 900]);
        for c in &out[1..] {
            assert_eq!((c.o, c.h, c.l, c.c, c.v), (0.31, 0.31, 0.31, 0.31, Some(0.0)));
        }
    }

    #[test]
    fn kalshi_native_1m_fills_gaps_and_keeps_bar_ohlc() {
        let bars = vec![
            kbar(0, 0.70, 0.72, 0.69, 0.71, 3.0, true),
            quote(3, 0.705),
        ];
        let out = resample_kalshi_minute_bars(&bars, 60, utc(T0), utc(T0 + 300));
        let got: Vec<(i64, f64, Option<f64>)> =
            out.iter().map(|c| (c.t.timestamp() - T0, c.c, c.v)).collect();
        assert_eq!(
            got,
            vec![
                (0, 0.71, Some(3.0)),
                (60, 0.71, Some(0.0)),   // gap: carried forward
                (120, 0.71, Some(0.0)),  // gap: carried forward
                (180, 0.705, Some(0.0)), // quote-only minute: quote mid
                (240, 0.705, Some(0.0)), // gap after the quote
            ]
        );
        assert_eq!((out[0].o, out[0].h, out[0].l), (0.70, 0.72, 0.69));
    }

    #[test]
    fn kalshi_resample_seed_before_start_only_seeds_carry_forward() {
        // Seed bar (last real bar before the window) sits in the bucket before
        // `start`; the window's first bucket has no bars and must carry it.
        let start = T0 + 300;
        let bars = vec![kbar(4, 0.62, 0.62, 0.62, 0.62, 1.0, true)];
        let out = resample_kalshi_minute_bars(&bars, 300, utc(start), utc(start + 600));
        let ts: Vec<i64> = out.iter().map(|c| c.t.timestamp() - T0).collect();
        assert_eq!(ts, vec![300, 600]);
        assert!(out.iter().all(|c| c.c == 0.62 && c.v == Some(0.0)));
    }

    #[test]
    fn kalshi_resample_partial_leading_bucket_dropped_and_traded_wins_tie() {
        // start falls mid-bucket: the partial [T0, T0+900) bucket is dropped.
        let mut bars = vec![
            quote(10, 0.20),
            kbar(16, 0.21, 0.23, 0.21, 0.22, 4.0, true),
        ];
        // Duplicate timestamp: quote-only copy of the traded minute must lose.
        bars.push(quote(16, 0.99));
        let out = resample_kalshi_minute_bars(&bars, 900, utc(T0 + 420), utc(T0 + 1800));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].t.timestamp(), T0 + 900);
        assert_eq!((out[0].o, out[0].h, out[0].l, out[0].c), (0.21, 0.23, 0.21, 0.22));
    }

    #[test]
    fn bare_kalshi_market_tickers_route_without_prefix() {
        for t in ["CONTROLS-2026-D", "SENATEOHS-26-R", "FED-23DEC-T5.25", "INXD-24JAN01-B4700"] {
            assert!(looks_like_bare_kalshi_market(t), "{t}");
            let req = parse_prediction_market_ticker(t).expect(t);
            assert!(matches!(req.provider, PredictionMarketProvider::Kalshi));
        }
        let no = parse_prediction_market_ticker("CONTROLS-2026-D:NO").unwrap();
        assert!(matches!(no.side, PredictionMarketSide::No));
        // Real Yahoo symbols stay with Yahoo.
        for t in ["BRK-B", "BTC-USD", "RDS-A", "^VIX", "CL=F", "USD/JPY=X", "000660.KS", "SPY", "CONTROLS-2026", "AB-CD-EF"] {
            assert!(parse_prediction_market_ticker(t).is_none(), "{t}");
        }
    }

    #[test]
    fn kalshi_window_sizes_batches_under_the_candle_cap() {
        let start = utc(T0);
        let w = kalshi_window(start, start + chrono::Duration::days(14), eli_core::finance::Span::parse("1h").unwrap()).unwrap();
        assert_eq!(w.api_interval_minutes, 60);
        // 14d of hourly bars ≈ 338 per ticker → 26 tickers per call.
        assert_eq!(w.tickers_per_call(), 26);
        let w = kalshi_window(start, start + chrono::Duration::days(1), eli_core::finance::Span::parse("1d").unwrap()).unwrap();
        assert_eq!(w.tickers_per_call(), KALSHI_BATCH_MAX_TICKERS);
        let w = kalshi_window(start, start + chrono::Duration::days(30), eli_core::finance::Span::parse("1m").unwrap()).unwrap();
        assert_eq!(w.tickers_per_call(), 1);
        assert!(kalshi_window(start, start + chrono::Duration::days(400), eli_core::finance::Span::parse("1m").unwrap()).is_err());
    }

    #[test]
    fn kalshi_archive_candles_parse_like_live_ones() {
        // The historical archive drops the _dollars suffixes and sends volume as a string.
        let archived: KalshiCandle = serde_json::from_value(serde_json::json!({
            "end_period_ts": T0 + 3600,
            "price": {"open": "0.7400", "high": "0.7500", "low": "0.7300", "close": "0.7500", "previous": "0.7400"},
            "volume": "403304.33",
            "yes_bid": {"close": "0.7400"}, "yes_ask": {"close": "0.7500"}
        }))
        .unwrap();
        let live: KalshiCandle = serde_json::from_value(serde_json::json!({
            "end_period_ts": T0 + 7200,
            "price": {"close_dollars": null, "previous_dollars": "0.7500"},
            "volume_fp": "0.00",
            "yes_bid": {"close_dollars": "0.7400"}, "yes_ask": {"close_dollars": "0.7600"}
        }))
        .unwrap();
        assert_eq!(archived.volume_contracts(), Some(403304.33));
        let req = parse_prediction_market_ticker("KXFEDDECISION-26JUL-H0").unwrap();
        let w = kalshi_window(utc(T0), utc(T0 + 7200), eli_core::finance::Span::parse("1h").unwrap()).unwrap();
        let s = kalshi_series_from_candles(&req, "KXFEDDECISION-26JUL-H0", vec![archived, live], &w, utc(T0), utc(T0 + 7200)).unwrap();
        assert_eq!(s.ticker, "KALSHI:KXFEDDECISION-26JUL-H0:YES");
        assert_eq!(s.candles.len(), 2);
        assert_eq!((s.candles[0].o, s.candles[0].h, s.candles[0].l, s.candles[0].c), (0.74, 0.75, 0.73, 0.75));
        // No trade in the second hour: flat at the bid/ask mid.
        assert!((s.candles[1].c - 0.75).abs() < 1e-9);
    }

    #[test]
    fn kalshi_resample_empty_input_is_empty() {
        assert!(resample_kalshi_minute_bars(&[], 300, utc(T0), utc(T0 + 600)).is_empty());
    }

    #[test]
    fn new_prefix_providers_route_to_their_own_bucket() {
        let cases = [
            ("PNAV:OBDC", ExtraProvider::Secnav),
            ("nav:fsk", ExtraProvider::Secnav),
            ("BOND:21873SAG3:YLD", ExtraProvider::FinraBonds),
            ("BUYBACK:10-20Y:COVER", ExtraProvider::Buyback),
            ("NYFED:SOFR_P99", ExtraProvider::Nyfed),
            ("OFR:FSI", ExtraProvider::Ofr),
            ("OFR:REPO-DVP_OV_TOT-P", ExtraProvider::Ofr),
            ("ETF:JNK:FLOW", ExtraProvider::EtfFlows),
            ("CBOE:VXCONTANGO", ExtraProvider::Cboe),
            ("CBOE:GEX:SPX", ExtraProvider::Cboe),
            ("HL:BTC:FUNDING_APR", ExtraProvider::Hyperliquid),
            ("DERIBIT:DVOL:BTC", ExtraProvider::Deribit),
        ];
        for (t, want) in cases {
            assert!(parse_prediction_market_ticker(t).is_none(), "{t} swallowed as odds");
            for provider in ["auto", "yahoo", "fred", "ibkr", "pyth", "binance", "govt"] {
                assert_eq!(
                    classify_timeseries_ticker(t, provider, true),
                    TimeseriesTickerBucket::Extra(want),
                    "{t} with --provider {provider}"
                );
            }
        }
        assert_eq!(
            classify_timeseries_ticker("SPY", "auto", false),
            TimeseriesTickerBucket::Main
        );
        assert_eq!(
            classify_timeseries_ticker("SPY", "cboe", false),
            TimeseriesTickerBucket::Extra(ExtraProvider::Cboe)
        );
    }

    #[test]
    fn explicit_prediction_market_requires_provider_and_market_together() {
        let err = parse_optional_prediction_market_request(Some("kalshi"), None, "yes")
            .expect_err("provider without market should fail");
        assert!(err.to_string().contains("must be provided together"));
    }

    #[test]
    fn picks_yes_no_outcome_index_when_available() {
        let outcomes = vec!["No".to_string(), "Yes".to_string()];
        let yes_idx =
            pick_polymarket_outcome_index(&outcomes, PredictionMarketSide::Yes, 2);
        let no_idx =
            pick_polymarket_outcome_index(&outcomes, PredictionMarketSide::No, 2);
        assert_eq!(yes_idx, 1);
        assert_eq!(no_idx, 0);
    }

    #[test]
    fn classify_timeseries_ticker_keeps_explicit_yahoo_out_of_fred_preset() {
        assert_eq!(
            classify_timeseries_ticker("DGS10", "auto", true),
            TimeseriesTickerBucket::Fred
        );
        assert_eq!(
            classify_timeseries_ticker("SPY", "auto", false),
            TimeseriesTickerBucket::Main
        );
    }

    #[test]
    fn ratepath_prefix_classifies_to_ratepath_bucket() {
        assert_eq!(
            classify_timeseries_ticker("RATEPATH:JUN2026:hold", "auto", false),
            TimeseriesTickerBucket::RatePath
        );
        assert_eq!(
            classify_timeseries_ticker("ratepath:dec2026:cut", "auto", false),
            TimeseriesTickerBucket::RatePath
        );
    }

    #[test]
    fn parse_ratepath_ticker_extracts_meeting_and_bucket() {
        let p = parse_ratepath_ticker("RATEPATH:JUN2026:hold").expect("parse");
        assert_eq!(p.year_short, "26");
        assert_eq!(p.year_full, 2026);
        assert_eq!(p.month_token, "JUN");
        assert_eq!(p.month_word_lower, "june");
        assert_eq!(p.bucket, RatePathBucket::Hold);

        let p2 = parse_ratepath_ticker("RATEPATH:DEC2027:cut50plus").expect("parse2");
        assert_eq!(p2.year_short, "27");
        assert_eq!(p2.month_token, "DEC");
        assert_eq!(p2.bucket, RatePathBucket::Cut50Plus);

        assert!(parse_ratepath_ticker("RATEPATH:invalid").is_none());
        assert!(parse_ratepath_ticker("RATEPATH:JUN2026:bogus").is_none());
        assert!(parse_ratepath_ticker("not_a_ratepath_ticker").is_none());
    }

    #[test]
    fn ratepath_bucket_cut_includes_cut25_and_cut50plus() {
        assert!(RatePathBucket::Cut.includes(RatePathBucket::Cut25));
        assert!(RatePathBucket::Cut.includes(RatePathBucket::Cut50Plus));
        assert!(!RatePathBucket::Cut.includes(RatePathBucket::Hold));
        assert!(!RatePathBucket::Cut.includes(RatePathBucket::Hike));
        assert!(RatePathBucket::Hold.includes(RatePathBucket::Hold));
        assert!(!RatePathBucket::Hold.includes(RatePathBucket::Cut25));
    }

    #[test]
    fn kalshi_suffix_maps_to_correct_bucket() {
        assert_eq!(kalshi_suffix_to_bucket("H0"), Some(RatePathBucket::Hold));
        assert_eq!(kalshi_suffix_to_bucket("C25"), Some(RatePathBucket::Cut25));
        assert_eq!(kalshi_suffix_to_bucket("C26"), Some(RatePathBucket::Cut50Plus));
        assert_eq!(kalshi_suffix_to_bucket("H25"), Some(RatePathBucket::Hike));
        assert_eq!(kalshi_suffix_to_bucket("H26"), Some(RatePathBucket::Hike));
        assert_eq!(kalshi_suffix_to_bucket("UNKNOWN"), None);
    }

    #[test]
    fn polymarket_title_classifier_handles_50_plus_variants() {
        // The "50+ bps" pattern from Polymarket June 2026 markets.
        assert_eq!(
            classify_polymarket_title("Will the Fed decrease interest rates by 50+ bps after the June 2026 meeting?"),
            Some(RatePathBucket::Cut50Plus)
        );
        assert_eq!(
            classify_polymarket_title("Will the Fed decrease interest rates by 25 bps after the June 2026 meeting?"),
            Some(RatePathBucket::Cut25)
        );
        assert_eq!(
            classify_polymarket_title("Will there be no change in Fed interest rates after the June 2026 meeting?"),
            Some(RatePathBucket::Hold)
        );
        assert_eq!(
            classify_polymarket_title("Will the Fed increase interest rates by 25 bps after the June 2026 meeting?"),
            Some(RatePathBucket::Hike)
        );
    }
}
