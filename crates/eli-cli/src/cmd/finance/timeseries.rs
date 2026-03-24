use futures::future::join_all;
use std::collections::{BTreeMap, HashSet};

const KALSHI_CANDLESTICKS_URL: &str =
    "https://api.elections.kalshi.com/trade-api/v2/markets/candlesticks";
const POLYMARKET_GAMMA_URL: &str = "https://gamma-api.polymarket.com";
const POLYMARKET_CLOB_HISTORY_URL: &str = "https://clob.polymarket.com/prices-history";

async fn cmd_finance_timeseries(args: FinanceTimeseriesArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    // Expand --preset into tickers (merged with any explicit --ticker values).
    let mut preset_tickers: Vec<String> = Vec::new();
    let preset_name = args.preset.as_deref().map(|s| s.trim().to_ascii_lowercase());
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

    // Standalone prediction market mode: only prediction-market tickers.
    if tickers.is_empty() {
        if !prediction_markets.is_empty() {
            let now = chrono::Utc::now();
            let end = as_of.unwrap_or(now).min(now);
            let start = end
                .checked_sub_signed(range.approx_duration())
                .ok_or_else(|| anyhow::anyhow!("range underflow"))?;
            let (market_series, market_errors) =
                fetch_prediction_market_series_batch(&prediction_markets, start, end, granularity)
                    .await;
            if market_series.is_empty() {
                let err_msg = market_errors
                    .first()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "no usable prediction market series found".to_string());
                anyhow::bail!("{err_msg}");
            }
            let series = market_series;
            let analytics =
                eli_core::finance::build_timeseries_analytics(&series, granularity);
            let provider = prediction_market_response_provider(&prediction_markets);
            let tickers: Vec<String> = series.iter().map(|s| s.ticker.clone()).collect();
            let resp = eli_core::finance::TimeseriesResponse {
                provider,
                tickers,
                granularity,
                range,
                start,
                end,
                generated_at: now,
                series,
                status: if market_errors.is_empty() {
                    None
                } else {
                    Some("partial".to_string())
                },
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

            if let Some(out_path) = args.out {
                let wr = write_json_out_with_meta(
                    out_path,
                    &resp,
                    "finance.timeseries",
                    &[
                        format!("range={}", args.range),
                        format!("granularity={}", args.granularity),
                        format!("prediction_markets={}", prediction_markets.len()),
                    ],
                )?;
                println!(
                    "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
                    serde_json::to_string(&wr.out_path.display().to_string())
                        .unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(&wr.meta_path.display().to_string())
                        .unwrap_or_else(|_| "\"\"".to_string()),
                );
                return Ok(());
            }

            let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
            println!("{json}");
            return Ok(());
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
    let mut stooq_tickers = Vec::new();
    let mut binance_tickers = Vec::new();
    let mut yahoo_tickers = Vec::new();
    for t in &preset_stock_tickers {
        match classify_timeseries_ticker(t, &provider_str, fred_preset) {
            TimeseriesTickerBucket::Pyth => pyth_tickers.push(t.clone()),
            TimeseriesTickerBucket::Fred => fred_tickers.push(t.clone()),
            TimeseriesTickerBucket::Stooq => stooq_tickers.push(t.clone()),
            TimeseriesTickerBucket::Binance => binance_tickers.push(t.clone()),
            TimeseriesTickerBucket::Main => yahoo_tickers.push(t.clone()),
        }
    }
    for t in &tickers {
        match classify_timeseries_ticker(t, &provider_str, false) {
            TimeseriesTickerBucket::Pyth => pyth_tickers.push(t.clone()),
            TimeseriesTickerBucket::Fred => fred_tickers.push(t.clone()),
            TimeseriesTickerBucket::Stooq => stooq_tickers.push(t.clone()),
            TimeseriesTickerBucket::Binance => binance_tickers.push(t.clone()),
            TimeseriesTickerBucket::Main => yahoo_tickers.push(t.clone()),
        }
    }
    let has_pyth = !pyth_tickers.is_empty();
    let has_fred = !fred_tickers.is_empty();
    let has_stooq = !stooq_tickers.is_empty();
    let has_binance = !binance_tickers.is_empty();

    let provider = match provider_str.as_str() {
        "auto" | "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "mock" => eli_core::finance::ProviderKind::Mock,
        "fred" => eli_core::finance::ProviderKind::Fred,
        "ibkr" => eli_core::finance::ProviderKind::Ibkr,
        "pyth" => eli_core::finance::ProviderKind::Pyth,
        "stooq" => eli_core::finance::ProviderKind::Stooq,
        "binance" => eli_core::finance::ProviderKind::Binance,
        other => {
            anyhow::bail!(
                "unsupported --provider '{other}' (supported: auto, mock, yahoo, fred, ibkr, pyth, stooq, binance)"
            )
        }
    };
    let use_ibkr = matches!(provider, eli_core::finance::ProviderKind::Ibkr);

    let cache_dir = if let Some(path) = args.cache_dir {
        path
    } else {
        let paths = Paths::discover().context("discover paths")?;
        paths.ensure_dirs().context("ensure dirs")?;
        paths.cache_dir
    };

    // Route to the right provider based on ticker types.
    // Priority: FRED tickers are the "main" request when present (most common preset case).
    // Pyth and Yahoo are merged in separately.
    let (main_tickers, main_provider) = if has_fred {
        // FRED as main, Pyth and Yahoo merged separately
        (fred_tickers.clone(), eli_core::finance::ProviderKind::Fred)
    } else if has_stooq && yahoo_tickers.is_empty() && !has_pyth && !has_binance {
        // All Stooq
        (stooq_tickers.clone(), eli_core::finance::ProviderKind::Stooq)
    } else if has_binance && yahoo_tickers.is_empty() && !has_pyth && !has_stooq {
        // All Binance
        (binance_tickers.clone(), eli_core::finance::ProviderKind::Binance)
    } else if has_pyth && yahoo_tickers.is_empty() && !has_stooq && !has_binance {
        // All Pyth
        (pyth_tickers.clone(), eli_core::finance::ProviderKind::Pyth)
    } else {
        // Yahoo (default) — other providers merge in separately
        (yahoo_tickers.clone(), provider.clone())
    };

    let req = eli_core::finance::TimeseriesRequest {
        tickers: main_tickers.clone(),
        range,
        granularity,
        as_of,
        provider: main_provider,
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

    let mut resp = eli_core::finance::fetch_timeseries(req, &cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch timeseries")?;

    // If FRED is main and there are Yahoo tickers, fetch Yahoo separately and merge.
    if has_fred && !yahoo_tickers.is_empty() {
        let yahoo_req = eli_core::finance::TimeseriesRequest {
            tickers: yahoo_tickers.clone(),
            range,
            granularity,
            as_of,
            provider: eli_core::finance::ProviderKind::Yahoo,
            max_points_per_ticker: args.max_points_per_ticker,
            ibkr: None,
        };
        match eli_core::finance::fetch_timeseries(yahoo_req, &cache_dir).await {
            Ok(yahoo_resp) => {
                resp.series.extend(yahoo_resp.series);
                resp.tickers.extend(yahoo_tickers.clone());
                resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
                    &resp.series,
                    resp.granularity,
                ));
            }
            Err(e) => {
                eprintln!("warning: Yahoo fetch failed for mixed request: {e}");
            }
        }
    }

    // (FRED merge into Yahoo is not needed — when has_fred, FRED IS the main provider.
    //  Yahoo tickers merge into FRED above.)

    // If mixed tickers: fetch Pyth tickers separately and merge into the response.
    if has_pyth && (has_fred || !yahoo_tickers.is_empty()) {
        let pyth_req = eli_core::finance::TimeseriesRequest {
            tickers: pyth_tickers.clone(),
            range,
            granularity,
            as_of,
            provider: eli_core::finance::ProviderKind::Pyth,
            max_points_per_ticker: args.max_points_per_ticker,
            ibkr: None,
        };
        match eli_core::finance::fetch_timeseries(pyth_req, &cache_dir).await {
            Ok(pyth_resp) => {
                resp.series.extend(pyth_resp.series);
                resp.tickers.extend(pyth_tickers.clone());
                if let Some(ref pyth_errors) = pyth_resp.errors {
                    resp.errors
                        .get_or_insert_with(Vec::new)
                        .extend(pyth_errors.clone());
                }
                // Recompute analytics with all series (Yahoo/FRED + Pyth).
                resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
                    &resp.series,
                    resp.granularity,
                ));
            }
            Err(e) => {
                eprintln!("warning: Pyth fetch failed: {e}");
                resp.errors
                    .get_or_insert_with(Vec::new)
                    .push(eli_core::finance::TimeseriesError {
                        ticker: pyth_tickers.join(","),
                        stage: Some("pyth".to_string()),
                        message: format!("Pyth provider failed: {e}"),
                    });
            }
        }
    }

    // If mixed tickers: fetch Stooq tickers separately and merge.
    if has_stooq && !stooq_tickers.iter().all(|t| main_tickers.contains(t)) {
        let stooq_req = eli_core::finance::TimeseriesRequest {
            tickers: stooq_tickers.clone(),
            range,
            granularity,
            as_of,
            provider: eli_core::finance::ProviderKind::Stooq,
            max_points_per_ticker: args.max_points_per_ticker,
            ibkr: None,
        };
        match eli_core::finance::fetch_timeseries(stooq_req, &cache_dir).await {
            Ok(stooq_resp) => {
                resp.series.extend(stooq_resp.series);
                resp.tickers.extend(stooq_tickers.clone());
                if let Some(ref stooq_errors) = stooq_resp.errors {
                    resp.errors
                        .get_or_insert_with(Vec::new)
                        .extend(stooq_errors.clone());
                }
                resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
                    &resp.series,
                    resp.granularity,
                ));
            }
            Err(e) => {
                eprintln!("warning: Stooq fetch failed: {e}");
                resp.errors
                    .get_or_insert_with(Vec::new)
                    .push(eli_core::finance::TimeseriesError {
                        ticker: stooq_tickers.join(","),
                        stage: Some("stooq".to_string()),
                        message: format!("Stooq provider failed: {e}"),
                    });
            }
        }
    }

    // If mixed tickers: fetch Binance tickers separately and merge.
    if has_binance && !binance_tickers.iter().all(|t| main_tickers.contains(t)) {
        let binance_req = eli_core::finance::TimeseriesRequest {
            tickers: binance_tickers.clone(),
            range,
            granularity,
            as_of,
            provider: eli_core::finance::ProviderKind::Binance,
            max_points_per_ticker: args.max_points_per_ticker,
            ibkr: None,
        };
        match eli_core::finance::fetch_timeseries(binance_req, &cache_dir).await {
            Ok(binance_resp) => {
                resp.series.extend(binance_resp.series);
                resp.tickers.extend(binance_tickers.clone());
                if let Some(ref binance_errors) = binance_resp.errors {
                    resp.errors
                        .get_or_insert_with(Vec::new)
                        .extend(binance_errors.clone());
                }
                resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
                    &resp.series,
                    resp.granularity,
                ));
            }
            Err(e) => {
                eprintln!("warning: Binance fetch failed: {e}");
                resp.errors
                    .get_or_insert_with(Vec::new)
                    .push(eli_core::finance::TimeseriesError {
                        ticker: binance_tickers.join(","),
                        stage: Some("binance".to_string()),
                        message: format!("Binance provider failed: {e}"),
                    });
            }
        }
    }

    // Auto-fallback: if in auto mode and Yahoo returned errors, retry failed tickers with FRED.
    // Also re-fetch valid Yahoo tickers individually so their data isn't lost (the core drops
    // all series when any ticker fails).
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
                max_points_per_ticker: args.max_points_per_ticker,
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
                max_points_per_ticker: args.max_points_per_ticker,
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
            // If all data came from FRED (no Yahoo successes), label provider as fred;
            // if mixed, label as yahoo (primary) — the data speaks for itself.
            if valid_tickers.is_empty() {
                resp.provider = eli_core::finance::ProviderKind::Fred;
            }
            // Preserve any Pyth series that were already merged before auto-fallback.
            let pyth_series: Vec<_> = resp
                .series
                .drain(..)
                .filter(|s| eli_core::finance::is_pyth_ticker(&s.ticker))
                .collect();
            merged_series.extend(pyth_series);
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
            // Recompute analytics with the merged series.
            resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
                &resp.series,
                resp.granularity,
            ));
        }
    }

    if !prediction_markets.is_empty() {
        let (market_series, market_errors) =
            fetch_prediction_market_series_batch(&prediction_markets, resp.start, resp.end, granularity)
                .await;
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
            resp.status = match resp.errors.as_ref() {
                Some(errors) if !errors.is_empty() => Some("partial".to_string()),
                _ => None,
            };
            resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
                &resp.series,
                resp.granularity,
            ));
        }
    }

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.timeseries",
            &[
                format!("range={}", args.range),
                format!("granularity={}", args.granularity),
            ],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"cache\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&resp.cache).unwrap_or_else(|_| "null".to_string())
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
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
    Stooq,
    Binance,
    Main,
}

fn classify_timeseries_ticker(
    ticker: &str,
    provider_str: &str,
    auto_prefers_fred: bool,
) -> TimeseriesTickerBucket {
    if eli_core::finance::is_pyth_ticker(ticker) || provider_str == "pyth" {
        return TimeseriesTickerBucket::Pyth;
    }
    if eli_core::finance::is_stooq_ticker(ticker) || provider_str == "stooq" {
        return TimeseriesTickerBucket::Stooq;
    }
    if eli_core::finance::is_binance_ticker(ticker) || provider_str == "binance" {
        return TimeseriesTickerBucket::Binance;
    }
    if provider_str == "fred" || (provider_str == "auto" && (auto_prefers_fred || is_fred_ticker(ticker))) {
        return TimeseriesTickerBucket::Fred;
    }
    TimeseriesTickerBucket::Main
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
        let (market, side) = split_prediction_market_side(rest)?;
        return Some(PredictionMarketRequest {
            provider: PredictionMarketProvider::Polymarket,
            market: market.to_string(),
            side,
        });
    }

    if let Some((market, side)) = trimmed.rsplit_once(':') {
        let side = parse_prediction_market_side(side)?;
        if market.starts_with("KX") && market.contains('-') {
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

    if trimmed.starts_with("KX") && trimmed.contains('-') {
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

async fn fetch_prediction_market_series_batch(
    requests: &[PredictionMarketRequest],
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> (
    Vec<eli_core::finance::TickerSeries>,
    Vec<eli_core::finance::TimeseriesError>,
) {
    let results = join_all(
        requests
            .iter()
            .map(|req| fetch_prediction_market_series(req, start, end, granularity)),
    )
    .await;

    let mut series = Vec::new();
    let mut errors = Vec::new();

    for (req, result) in requests.iter().zip(results) {
        match result {
            Ok(market_series) => series.push(market_series),
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

fn granularity_minutes(span: eli_core::finance::Span) -> i64 {
    ((granularity_seconds(span) + 59) / 60).max(1)
}

async fn fetch_kalshi_market_series(
    req: &PredictionMarketRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<eli_core::finance::TickerSeries> {
    #[derive(serde::Deserialize)]
    struct KalshiResp {
        #[serde(default)]
        markets: Vec<KalshiMarketCandles>,
    }

    #[derive(serde::Deserialize)]
    struct KalshiMarketCandles {
        market_ticker: String,
        #[serde(default)]
        candlesticks: Vec<KalshiCandle>,
    }

    #[derive(serde::Deserialize)]
    struct KalshiBidAsk {
        #[serde(default)]
        close_dollars: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct KalshiPrice {
        #[serde(default)]
        open_dollars: Option<String>,
        #[serde(default)]
        low_dollars: Option<String>,
        #[serde(default)]
        high_dollars: Option<String>,
        #[serde(default)]
        close_dollars: Option<String>,
        #[serde(default)]
        previous_dollars: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct KalshiCandle {
        end_period_ts: i64,
        #[serde(default)]
        price: Option<KalshiPrice>,
        #[serde(default)]
        yes_bid: Option<KalshiBidAsk>,
        #[serde(default)]
        yes_ask: Option<KalshiBidAsk>,
        #[serde(default)]
        volume: Option<i64>,
    }

    let requested_seconds = granularity_seconds(granularity);

    // Kalshi's candlestick API only truly supports period_interval=60 (1h) and
    // 1440 (daily).  All other values silently degrade to daily bars.
    // Strategy: fetch at the finest supported native interval, then resample
    // locally to the target granularity.
    let (api_interval_minutes, needs_resample) = if requested_seconds >= 86400 {
        // Daily or coarser: use native daily.
        (1440_i64, false)
    } else if requested_seconds < 3600 {
        // Sub-hour: Kalshi doesn't support it.  Fail explicitly rather than
        // inventing fake bars.
        anyhow::bail!(
            "kalshi candlesticks do not support sub-hourly granularity (requested {}s); \
             use --granularity 1h or coarser",
            requested_seconds
        );
    } else if requested_seconds == 3600 {
        // Exact 1h: native.
        (60_i64, false)
    } else {
        // 2h, 3h, 4h, 6h, etc.: fetch 1h, resample locally.
        (60_i64, true)
    };

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("init kalshi candlestick client")?;

    let start_ts = start.timestamp();
    let end_ts = end.timestamp();
    let start_ts_s = start_ts.to_string();
    let end_ts_s = end_ts.to_string();
    let interval_s = api_interval_minutes.to_string();

    let resp = client
        .get(KALSHI_CANDLESTICKS_URL)
        .query(&[
            ("market_tickers", req.market.as_str()),
            ("start_ts", start_ts_s.as_str()),
            ("end_ts", end_ts_s.as_str()),
            ("period_interval", interval_s.as_str()),
        ])
        .send()
        .await
        .context("request kalshi candlesticks")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "kalshi candlesticks failed for market {}: http {}: {}",
            req.market,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let parsed: KalshiResp = resp
        .json()
        .await
        .context("parse kalshi candlesticks response")?;
    let market = parsed.markets.into_iter().next().ok_or_else(|| {
        anyhow::anyhow!("kalshi returned no candlesticks for market {}", req.market)
    })?;

    let mut candles: Vec<eli_core::finance::Candle> = Vec::new();
    for kc in market.candlesticks {
        let yes_close = kc
            .price
            .as_ref()
            .and_then(|p| p.close_dollars.as_deref())
            .and_then(parse_decimal)
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

        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(kc.end_period_ts, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid kalshi candlestick timestamp"))?;

        candles.push(eli_core::finance::Candle {
            t: ts,
            o: req.side.apply(yes_open),
            h: req.side.apply(yes_high),
            l: req.side.apply(yes_low),
            c: req.side.apply(yes_close),
            v: kc.volume.map(|v| v as f64),
        });
    }

    if candles.is_empty() {
        anyhow::bail!(
            "no usable kalshi candlesticks found for market {} in requested window",
            req.market
        );
    }

    candles.sort_by_key(|c| c.t);

    // Resample 1h bars → target granularity if needed.
    if needs_resample {
        // Kalshi timestamps are end-of-bar (the API field is `end_period_ts`).
        // Convert to start-of-bar ONLY for resampling so that the absolute-clock
        // resampler places each bar in the correct bucket.  We do NOT touch
        // timestamps for native 1h or 1d output — those are correct as-is.
        let native_step_secs = api_interval_minutes * 60;
        for c in &mut candles {
            c.t = c.t - chrono::Duration::seconds(native_step_secs);
        }
        let step = chrono::Duration::seconds(requested_seconds);
        candles = eli_core::finance::resample_candles(&candles, start, step);
    }

    Ok(eli_core::finance::TickerSeries {
        ticker: format!(
            "KALSHI:{}:{}",
            market.market_ticker,
            req.side.as_str().to_ascii_uppercase()
        ),
        candles,
    })
}

async fn fetch_polymarket_market_series(
    req: &PredictionMarketRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<eli_core::finance::TickerSeries> {
    #[derive(serde::Deserialize)]
    struct PolyHistoryPoint {
        t: serde_json::Value,
        p: serde_json::Value,
    }

    #[derive(serde::Deserialize)]
    struct PolyHistoryResp {
        #[serde(default)]
        history: Vec<PolyHistoryPoint>,
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("init polymarket history client")?;

    let market_meta = fetch_polymarket_market_meta(&client, &req.market).await?;
    let market_id = market_meta
        .get("id")
        .map(json_value_to_string)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| req.market.clone());

    let outcomes = market_meta
        .get("outcomes")
        .map(parse_jsonish_string_list)
        .unwrap_or_default();
    let token_ids = market_meta
        .get("clobTokenIds")
        .map(parse_jsonish_string_list)
        .unwrap_or_default();

    if token_ids.is_empty() {
        anyhow::bail!("polymarket market {} has no clob token IDs", req.market);
    }

    let outcome_index = pick_polymarket_outcome_index(&outcomes, req.side, token_ids.len());
    let token_id = token_ids
        .get(outcome_index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("invalid polymarket outcome index for {}", req.market))?;
    let fidelity = granularity_minutes(granularity).max(1);
    let fidelity_s = fidelity.to_string();

    let resp = client
        .get(POLYMARKET_CLOB_HISTORY_URL)
        .query(&[
            ("market", token_id.as_str()),
            ("interval", "max"),
            ("fidelity", fidelity_s.as_str()),
        ])
        .send()
        .await
        .context("request polymarket price history")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "polymarket price history failed for market {} (token {}): http {}: {}",
            req.market,
            token_id,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let parsed: PolyHistoryResp = resp
        .json()
        .await
        .context("parse polymarket price history response")?;

    let step_seconds = granularity_seconds(granularity).max(1);
    let start_ts = start.timestamp();
    let end_ts = end.timestamp();

    #[derive(Clone, Copy)]
    struct Ohlc {
        o: f64,
        h: f64,
        l: f64,
        c: f64,
        first_ts: i64,
        last_ts: i64,
        points: usize,
    }

    let mut buckets: BTreeMap<i64, Ohlc> = BTreeMap::new();

    for point in parsed.history {
        let ts = parse_json_number_i64(&point.t).unwrap_or_default();
        let price = parse_json_number_f64(&point.p).unwrap_or_default();
        if ts <= 0 || !price.is_finite() {
            continue;
        }
        if ts < start_ts || ts > end_ts {
            continue;
        }

        let bucket = ts.div_euclid(step_seconds) * step_seconds;
        let adj_price = req.side.apply(price);

        match buckets.get_mut(&bucket) {
            Some(entry) => {
                if ts < entry.first_ts {
                    entry.first_ts = ts;
                    entry.o = adj_price;
                }
                if ts >= entry.last_ts {
                    entry.last_ts = ts;
                    entry.c = adj_price;
                }
                if adj_price > entry.h {
                    entry.h = adj_price;
                }
                if adj_price < entry.l {
                    entry.l = adj_price;
                }
                entry.points += 1;
            }
            None => {
                buckets.insert(
                    bucket,
                    Ohlc {
                        o: adj_price,
                        h: adj_price,
                        l: adj_price,
                        c: adj_price,
                        first_ts: ts,
                        last_ts: ts,
                        points: 1,
                    },
                );
            }
        }
    }

    if buckets.is_empty() {
        anyhow::bail!(
            "no polymarket price history points found for market {} in requested window",
            req.market
        );
    }

    let mut candles = Vec::with_capacity(buckets.len());
    for (bucket, ohlc) in buckets {
        // Use bucket START as the candle timestamp, not bucket + step (end).
        // The old code stamped bucket + step, which made March 23 data appear
        // as a March 24 candle at daily granularity.
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(bucket, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid polymarket bucket timestamp"))?;
        candles.push(eli_core::finance::Candle {
            t: ts,
            o: ohlc.o,
            h: ohlc.h,
            l: ohlc.l,
            c: ohlc.c,
            v: None,
        });
    }

    Ok(eli_core::finance::TickerSeries {
        ticker: format!(
            "POLYMARKET:{market_id}:{}",
            req.side.as_str().to_ascii_uppercase()
        ),
        candles,
    })
}

async fn fetch_polymarket_market_meta(
    client: &reqwest::Client,
    market: &str,
) -> Result<serde_json::Value> {
    let direct_url = format!("{POLYMARKET_GAMMA_URL}/markets/{market}");
    let direct = client
        .get(&direct_url)
        .send()
        .await
        .context("request polymarket market metadata")?;

    if direct.status().is_success() {
        let value: serde_json::Value = direct
            .json()
            .await
            .context("parse polymarket market metadata")?;
        if value.is_object() {
            return Ok(value);
        }
    } else if direct.status() != reqwest::StatusCode::NOT_FOUND {
        let status = direct.status();
        let body = direct.text().await.unwrap_or_default();
        anyhow::bail!(
            "polymarket market metadata failed for {}: http {}: {}",
            market,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let fallback = client
        .get(format!("{POLYMARKET_GAMMA_URL}/markets"))
        .query(&[("slug", market), ("limit", "1")])
        .send()
        .await
        .context("request polymarket market metadata by slug")?;

    if !fallback.status().is_success() {
        let status = fallback.status();
        let body = fallback.text().await.unwrap_or_default();
        anyhow::bail!(
            "polymarket market metadata (slug) failed for {}: http {}: {}",
            market,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let value: serde_json::Value = fallback
        .json()
        .await
        .context("parse polymarket market metadata by slug")?;

    let Some(first) = value.as_array().and_then(|items| items.first()).cloned() else {
        anyhow::bail!("polymarket market '{}' not found by id or slug", market);
    };

    Ok(first)
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

fn pick_polymarket_outcome_index(
    outcomes: &[String],
    side: PredictionMarketSide,
    token_count: usize,
) -> usize {
    if token_count <= 1 {
        return 0;
    }

    let target = match side {
        PredictionMarketSide::Yes => "yes",
        PredictionMarketSide::No => "no",
    };

    if let Some(idx) = outcomes
        .iter()
        .position(|o| o.trim().eq_ignore_ascii_case(target))
    {
        return idx.min(token_count - 1);
    }

    match side {
        PredictionMarketSide::Yes => 0,
        PredictionMarketSide::No => 1.min(token_count - 1),
    }
}

fn expand_timeseries_preset(preset: &str) -> Result<Vec<String>> {
    let tickers: Vec<&str> = match preset {
        "macro" => vec![
            // Inflation
            "CPIAUCSL", "CPILFESL", "PCEPILFE", "PPIACO", "T10YIE",
            // Employment
            "UNRATE", "PAYEMS", "ICSA", "JTSJOL",
            // GDP
            "GDPC1", "INDPRO",
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
            // Live 24/7 via Pyth
            "PYTH:OIL", "PYTH:GOLD", "PYTH:BTC",
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
        "crypto" => vec![
            "PYTH:BTC", "PYTH:ETH", "PYTH:SOL",
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
        other => anyhow::bail!(
            "unknown --preset '{other}' (supported: macro, forex_majors, yield_curve, liquidity, crypto, credit, financial_conditions, recession, fed_balance_sheet, housing, labor, inflation, real_rates, consumer_credit)"
        ),
    };
    Ok(tickers.into_iter().map(String::from).collect())
}

/// Heuristic: FRED series IDs are ALL-CAPS alphanumeric (plus underscore),
/// typically 3-20 chars, no dots/dashes/equals/carets that Yahoo tickers use.
/// Known FRED prefixes: DGS, BAML, FRED indicators like UNRATE, CPIAUCSL, etc.
fn is_fred_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
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
    // Exclude common short Yahoo tickers (SPY, QQQ, AAPL, etc.)
    // FRED tickers tend to be longer or have digits mixed in (DGS10, M2SL, WALCL)
    if looks_fred && t.len() <= 4 && t.chars().all(|c| c.is_ascii_uppercase()) {
        // Short all-alpha tickers (SPY, QQQ, AAPL, GLD, XLE) are almost certainly Yahoo
        return false;
    }
    looks_fred
}

#[cfg(test)]
mod timeseries_tests {
    use super::*;
    use chrono::TimeZone;

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

}
