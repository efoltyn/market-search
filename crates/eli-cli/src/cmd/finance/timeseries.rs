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
                sources: Vec::new(),
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
    let mut binance_tickers = Vec::new();
    let mut ibkr_tickers = Vec::new();
    let mut cleveland_tickers = Vec::new();
    let mut yahoo_tickers = Vec::new();
    for t in &preset_stock_tickers {
        match classify_timeseries_ticker(t, &provider_str, fred_preset) {
            TimeseriesTickerBucket::Pyth => pyth_tickers.push(t.clone()),
            TimeseriesTickerBucket::Fred => fred_tickers.push(t.clone()),
            TimeseriesTickerBucket::Binance => binance_tickers.push(t.clone()),
            TimeseriesTickerBucket::Ibkr => ibkr_tickers.push(t.clone()),
            TimeseriesTickerBucket::ClevelandFed => cleveland_tickers.push(t.clone()),
            TimeseriesTickerBucket::Main => yahoo_tickers.push(t.clone()),
        }
    }
    for t in &tickers {
        match classify_timeseries_ticker(t, &provider_str, false) {
            TimeseriesTickerBucket::Pyth => pyth_tickers.push(t.clone()),
            TimeseriesTickerBucket::Fred => fred_tickers.push(t.clone()),
            TimeseriesTickerBucket::Binance => binance_tickers.push(t.clone()),
            TimeseriesTickerBucket::Ibkr => ibkr_tickers.push(t.clone()),
            TimeseriesTickerBucket::ClevelandFed => cleveland_tickers.push(t.clone()),
            TimeseriesTickerBucket::Main => yahoo_tickers.push(t.clone()),
        }
    }
    let has_pyth = !pyth_tickers.is_empty();
    let has_fred = !fred_tickers.is_empty();
    let has_binance = !binance_tickers.is_empty();
    let has_ibkr = !ibkr_tickers.is_empty();
    let has_cleveland = !cleveland_tickers.is_empty();

    let provider = match provider_str.as_str() {
        "auto" | "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "mock" => eli_core::finance::ProviderKind::Mock,
        "fred" => eli_core::finance::ProviderKind::Fred,
        "ibkr" => eli_core::finance::ProviderKind::Ibkr,
        "pyth" => eli_core::finance::ProviderKind::Pyth,
        "binance" => eli_core::finance::ProviderKind::Binance,
        other => {
            anyhow::bail!(
                "unsupported --provider '{other}' (supported: auto, yahoo, fred, ibkr, pyth, binance)"
            )
        }
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
        && ibkr_tickers.is_empty();
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

        // Merge prediction market series if present.
        if !prediction_markets.is_empty() {
            let (market_series, _market_errors) =
                fetch_prediction_market_series_batch(&prediction_markets, start, end, granularity)
                    .await;
            all_series.extend(market_series);
        }

        let analytics =
            eli_core::finance::build_timeseries_analytics(&all_series, granularity);
        let tickers_out: Vec<String> = all_series.iter().map(|s| s.ticker.clone()).collect();
        let resp = eli_core::finance::TimeseriesResponse {
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
            errors: None,
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
                    format!("provider=cleveland_fed"),
                    format!("tickers={}", cleveland_tickers.join(",")),
                ],
            )?;
            println!(
                "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
                serde_json::to_string(&wr.out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&wr.meta_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
            );
        } else {
            let json = serde_json::to_string_pretty(&resp)?;
            println!("{json}");
        }
        return Ok(());
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

    let mut resp = eli_core::finance::fetch_timeseries(req, &cache_dir)
        .await
        .map(|mut r| {
            if fred_granularity_downgraded {
                let warn = format!(
                    "granularity downgraded from requested {:?} to 1d for FRED provider (sub-daily not supported)",
                    granularity.unit
                );
                r.errors.get_or_insert_with(Vec::new).push(
                    eli_core::finance::TimeseriesError {
                        ticker: main_tickers.join(","),
                        stage: Some("granularity_downgrade".to_string()),
                        message: warn,
                    },
                );
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
        })
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

    // If mixed tickers: fetch IBKR tickers separately and merge.
    // Skip if IBKR is already the main provider (all tickers were IBKR).
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
        let ibkr_req = eli_core::finance::TimeseriesRequest {
            tickers: stripped,
            range,
            granularity,
            as_of,
            provider: eli_core::finance::ProviderKind::Ibkr,
            max_points_per_ticker: args.max_points_per_ticker,
            ibkr: Some(ibkr_conn),
        };
        match eli_core::finance::fetch_timeseries(ibkr_req, &cache_dir).await {
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
                resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
                    &resp.series,
                    resp.granularity,
                ));
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

    // If mixed tickers: fetch Cleveland Fed tickers separately and merge.
    if has_cleveland && !only_cleveland {
        let now = chrono::Utc::now();
        let clev_end = as_of.unwrap_or(now).min(now);
        let clev_start = clev_end
            .checked_sub_signed(range.approx_duration())
            .ok_or_else(|| anyhow::anyhow!("range underflow"))?;
        match fetch_cleveland_fed_series(&cleveland_tickers, clev_start, clev_end).await {
            Ok(clev_series) => {
                resp.series.extend(clev_series);
                resp.tickers.extend(cleveland_tickers.clone());
                resp.analytics = Some(eli_core::finance::build_timeseries_analytics(
                    &resp.series,
                    resp.granularity,
                ));
            }
            Err(e) => {
                eprintln!("warning: Cleveland Fed fetch failed: {e}");
                resp.errors
                    .get_or_insert_with(Vec::new)
                    .push(eli_core::finance::TimeseriesError {
                        ticker: cleveland_tickers.join(","),
                        stage: Some("cleveland_fed".to_string()),
                        message: format!("Cleveland Fed provider failed: {e}"),
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

    // Populate top-level `sources` with distinct providers actually present in series.
    // The legacy `provider` field reflects only the dispatch-time main bucket and lies
    // on mixed calls; `sources` is the truth.
    {
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
    }

    // Silent-drop detection: every requested ticker must appear in either resp.series
    // or resp.errors. If a ticker fell through both buckets without an explicit error
    // (e.g., classified to a bucket that returned data for siblings but skipped this one),
    // surface it as a stage="silent_drop" error so the caller knows.
    {
        use std::collections::HashSet;
        let mut requested: HashSet<String> = HashSet::new();
        for t in &preset_stock_tickers {
            requested.insert(t.clone());
        }
        for t in &tickers {
            requested.insert(t.clone());
        }
        for pm in &prediction_markets {
            requested.insert(pm.market.clone());
        }
        let mut accounted: HashSet<String> = HashSet::new();
        for s in &resp.series {
            // Series ticker often differs from input (PYTH:BTC ↔ PYTH:BTC, 906975 ↔
            // POLYMARKET:906975:YES, IBKR:FUT:CL:NYMEX ↔ FUT:CL:NYMEX). Mark any
            // requested ticker that appears as substring of the series ticker
            // OR that contains the series ticker as accounted-for.
            for req in &requested {
                if s.ticker == *req
                    || s.ticker.contains(req.as_str())
                    || req.contains(s.ticker.as_str())
                {
                    accounted.insert(req.clone());
                }
            }
        }
        if let Some(errs) = &resp.errors {
            for e in errs {
                for req in &requested {
                    if e.ticker == *req
                        || e.ticker.contains(req.as_str())
                        || req.contains(e.ticker.as_str())
                    {
                        accounted.insert(req.clone());
                    }
                }
            }
        }
        let dropped: Vec<String> = requested.difference(&accounted).cloned().collect();
        if !dropped.is_empty() {
            let drop_errors: Vec<eli_core::finance::TimeseriesError> = dropped
                .into_iter()
                .map(|t| eli_core::finance::TimeseriesError {
                    ticker: t,
                    stage: Some("silent_drop".to_string()),
                    message:
                        "no provider returned data and no error was recorded for this ticker"
                            .to_string(),
                })
                .collect();
            resp.errors.get_or_insert_with(Vec::new).extend(drop_errors);
            if resp.status.is_none() && !resp.series.is_empty() {
                resp.status = Some("partial".to_string());
            }
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
    Binance,
    Ibkr,
    ClevelandFed,
    Main,
}

fn classify_timeseries_ticker(
    ticker: &str,
    provider_str: &str,
    auto_prefers_fred: bool,
) -> TimeseriesTickerBucket {
    if ticker.starts_with("CLEV:") || ticker.starts_with("clev:") {
        return TimeseriesTickerBucket::ClevelandFed;
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
            kind: None,
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
        source: Some("kalshi".to_string()),
        upstream_id: Some(market.market_ticker.clone()),
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
            kind: None,
        });
    }

    Ok(eli_core::finance::TickerSeries {
        ticker: format!(
            "POLYMARKET:{market_id}:{}",
            req.side.as_str().to_ascii_uppercase()
        ),
        candles,
        source: Some("polymarket".to_string()),
        upstream_id: Some(market_id.to_string()),
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
            "unknown --preset '{other}' (supported: macro, forex_majors, yield_curve, liquidity, crypto, credit, financial_conditions, recession, fed_balance_sheet, housing, labor, inflation, real_rates, consumer_credit, energy, commodities, treasuries). For futures curves, pass the root symbol directly as a ticker (e.g. --tickers CL,GC,ZN) and the tool auto-expands to the curve."
        ),
    };
    Ok(tickers.into_iter().map(String::from).collect())
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
