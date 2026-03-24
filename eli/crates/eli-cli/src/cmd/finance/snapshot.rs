async fn cmd_finance_snapshot(args: FinanceSnapshotArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }
    let return_windows = parse_snapshot_return_windows(&args.returns)?;
    let as_of = match args.as_of.as_deref() {
        Some(raw) => Some(
            eli_core::finance::parse_as_of(raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --as-of")?,
        ),
        None => None,
    };

    let mut tickers = args.tickers;
    if let Some(path) = args.tickers_file {
        let raw = std::fs::read_to_string(&path).context("read tickers_file")?;
        for line in raw.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            tickers.push(t.to_string());
        }
    }

    let provider = match args.provider.trim().to_ascii_lowercase().as_str() {
        "mock" => eli_core::finance::ProviderKind::Mock,
        "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "ibkr" => eli_core::finance::ProviderKind::Ibkr,
        other => anyhow::bail!("unsupported --provider '{other}' (supported: mock, yahoo, ibkr)"),
    };

    let ibkr = matches!(provider, eli_core::finance::ProviderKind::Ibkr).then(|| {
        build_ibkr_connection_config(
            args.ibkr_account.clone(),
            args.ibkr_host.clone(),
            args.ibkr_port,
            args.ibkr_client_id,
            args.ibkr_market_data_type,
            None,
        )
    });

    let req = eli_core::finance::SnapshotRequest {
        tickers,
        as_of: as_of.clone(),
        provider,
        ibkr,
    };
    let mut resp = eli_core::finance::fetch_snapshot(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch snapshot")?;

    // When market is closed, Yahoo's previousClose is unreliable (can lag a full session)
    // and even currentPrice is typically stale. Fetch daily timeseries and patch affected
    // snapshots with the actual last two closes.
    // We share this fetch with the --returns path when both are needed.
    // Always run the timeseries patch for Yahoo. The Yahoo quote API intermittently
    // returns current_price=None for some ETFs in a batch, causing non-deterministic
    // previous_close_fallback. Timeseries data is reliable and consistent.
    let needs_ts_patch = matches!(resp.provider, eli_core::finance::ProviderKind::Yahoo);
    let needs_returns = !return_windows.is_empty()
        && matches!(resp.provider, eli_core::finance::ProviderKind::Yahoo)
        && !resp.snapshots.is_empty();

    if needs_ts_patch || needs_returns {
        let snapshot_tickers: Vec<String> =
            resp.snapshots.iter().map(|snapshot| snapshot.ticker.clone()).collect();
        if !snapshot_tickers.is_empty() {
            let longest_for_returns = return_windows
                .iter()
                .max_by_key(|(_, span)| span.approx_duration().num_seconds())
                .map(|(_, span)| *span);

            // When patching closed-market prices without --returns, 1mo is always sufficient
            // to reach the last two trading days regardless of holidays.
            let fetch_range = match longest_for_returns {
                Some(span) => padded_snapshot_return_fetch_range(span),
                None => eli_core::finance::Span {
                    n: 1,
                    unit: eli_core::finance::SpanUnit::Month,
                },
            };

            let cache_dir = default_finance_cache_dir()?;
            let ts_req = eli_core::finance::TimeseriesRequest {
                tickers: snapshot_tickers,
                range: fetch_range,
                granularity: eli_core::finance::Span {
                    n: 1,
                    unit: eli_core::finance::SpanUnit::Day,
                },
                as_of: as_of.clone().or(Some(resp.generated_at)),
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: None,
                ibkr: None,
            };

            match eli_core::finance::fetch_timeseries(ts_req, &cache_dir).await {
                Ok(ts_resp) => {
                // Patch closed-market fallback prices from actual candle closes.
                if needs_ts_patch {
                    let ts_map: std::collections::HashMap<String, _> = ts_resp
                        .series
                        .iter()
                        .map(|s| (s.ticker.clone(), s))
                        .collect();
                    let mut corrected_count = 0usize;
                    let today = chrono::Utc::now().date_naive();
                    for snapshot in resp.snapshots.iter_mut() {
                        let Some(series) = ts_map.get(&snapshot.ticker) else {
                            continue;
                        };
                        // Find the last candle with a valid (positive, finite) close price.
                        let last = series
                            .candles
                            .iter()
                            .rev()
                            .find(|c| c.c > 0.0 && c.c.is_finite());
                        let Some(last) = last else {
                            continue;
                        };
                        // Only apply timeseries correction if the candle is from today.
                        // During regular session with stale Yahoo data, a candle from
                        // yesterday is no better than the stale snapshot price.
                        let candle_day = last.t.date_naive();
                        let is_regular = snapshot.session_state == "regular";
                        if is_regular && candle_day < today {
                            continue;
                        }
                        // Find the previous trading day's close for daily_return calc.
                        let last_day = last.t.date_naive();
                        let prev_close = series
                            .candles
                            .iter()
                            .rev()
                            .find(|c| c.t.date_naive() < last_day && c.c > 0.0 && c.c.is_finite())
                            .map(|c| c.c);
                        snapshot.current_price = Some(last.c);
                        snapshot.price = Some(last.c);
                        snapshot.previous_close = prev_close.or(snapshot.previous_close);
                        snapshot.daily_return = match (Some(last.c), prev_close) {
                            (Some(px), Some(prev)) if prev.is_finite() && prev != 0.0 => {
                                Some((px / prev) - 1.0)
                            }
                            _ => snapshot.daily_return,
                        };
                        // Recompute market_cap from the corrected price.
                        if let Some(shares) = snapshot.shares_outstanding {
                            snapshot.market_cap = Some((last.c * shares as f64).round() as u64);
                        }
                        snapshot.price_source_kind = "timeseries_corrected".to_string();
                        if snapshot.market_closed_fallback {
                            snapshot.market_closed_fallback = false;
                            corrected_count += 1;
                        }
                        // Clear stale warning — the corrected price is from today's candle.
                        snapshot.integrity_note = None;
                    }
                    if corrected_count > 0 {
                        resp.market_closed_fallback_count = resp
                            .market_closed_fallback_count
                            .saturating_sub(corrected_count);
                        resp.has_market_closed_fallback = resp.market_closed_fallback_count > 0;
                    }
                    // Retry individual tickers that still have stale data.
                    // Yahoo's batch timeseries intermittently omits today's candle for
                    // some tickers; single-ticker fetches are more reliable.
                    let still_stale: Vec<String> = resp
                        .snapshots
                        .iter()
                        .filter(|s| s.market_closed_fallback)
                        .map(|s| s.ticker.clone())
                        .collect();
                    for stale_ticker in &still_stale {
                        let retry_req = eli_core::finance::TimeseriesRequest {
                            tickers: vec![stale_ticker.clone()],
                            range: eli_core::finance::Span {
                                n: 1,
                                unit: eli_core::finance::SpanUnit::Month,
                            },
                            granularity: eli_core::finance::Span {
                                n: 1,
                                unit: eli_core::finance::SpanUnit::Day,
                            },
                            as_of: None,
                            provider: eli_core::finance::ProviderKind::Yahoo,
                            max_points_per_ticker: None,
                            ibkr: None,
                        };
                        if let Ok(retry_resp) = eli_core::finance::fetch_timeseries(retry_req, &cache_dir).await {
                            for retry_series in &retry_resp.series {
                                if retry_series.ticker != *stale_ticker {
                                    continue;
                                }
                                let last = retry_series
                                    .candles
                                    .iter()
                                    .rev()
                                    .find(|c| c.c > 0.0 && c.c.is_finite());
                                let Some(last) = last else { continue };
                                let candle_day = last.t.date_naive();
                                if candle_day < today {
                                    continue;
                                }
                                let last_day = last.t.date_naive();
                                let prev_close = retry_series
                                    .candles
                                    .iter()
                                    .rev()
                                    .find(|c| c.t.date_naive() < last_day && c.c > 0.0 && c.c.is_finite())
                                    .map(|c| c.c);
                                if let Some(snapshot) = resp.snapshots.iter_mut().find(|s| s.ticker == *stale_ticker) {
                                    snapshot.current_price = Some(last.c);
                                    snapshot.price = Some(last.c);
                                    snapshot.previous_close = prev_close.or(snapshot.previous_close);
                                    snapshot.daily_return = match (Some(last.c), prev_close) {
                                        (Some(px), Some(prev)) if prev.is_finite() && prev != 0.0 => {
                                            Some((px / prev) - 1.0)
                                        }
                                        _ => snapshot.daily_return,
                                    };
                                    if let Some(shares) = snapshot.shares_outstanding {
                                        snapshot.market_cap = Some((last.c * shares as f64).round() as u64);
                                    }
                                    snapshot.price_source_kind = "timeseries_corrected".to_string();
                                    snapshot.market_closed_fallback = false;
                                    snapshot.integrity_note = None;
                                    resp.market_closed_fallback_count = resp.market_closed_fallback_count.saturating_sub(1);
                                    resp.has_market_closed_fallback = resp.market_closed_fallback_count > 0;
                                }
                            }
                        }
                    }
                    // Always recompute analytics with the freshest prices.
                    let fresh_analytics = eli_core::finance::build_snapshot_analytics(&resp.snapshots);
                    resp.market_note = fresh_analytics.market_note.clone();
                    resp.analytics = Some(fresh_analytics);
                }

                // Compute trailing returns if --returns was requested.
                if needs_returns {
                    let mut trailing: std::collections::BTreeMap<
                        String,
                        std::collections::BTreeMap<String, f64>,
                    > = std::collections::BTreeMap::new();
                    for series in &ts_resp.series {
                        let Some(latest) = series.candles.last() else {
                            continue;
                        };
                        if latest.c <= 0.0 {
                            continue;
                        }
                        let mut per_period = std::collections::BTreeMap::new();
                        for (label, span) in &return_windows {
                            let target = latest.t - span.approx_duration();
                            if let Some(anchor) =
                                series.candles.iter().rev().find(|c| c.t <= target)
                            {
                                if anchor.c > 0.0 {
                                    per_period
                                        .insert(label.clone(), (latest.c / anchor.c) - 1.0);
                                }
                            }
                        }
                        if !per_period.is_empty() {
                            trailing.insert(series.ticker.clone(), per_period);
                        }
                    }
                    if !trailing.is_empty() {
                        resp.trailing_returns = Some(trailing);
                    }
                }
                }
                Err(_) => {
                    // Timeseries fetch failed — snapshots retain Yahoo's stale prices.
                    // Add a decision trace note so consumers can see that correction was attempted.
                    resp.decision_trace
                        .push("timeseries_patch=failed".to_string());
                }
            }
        }
    }

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.snapshot",
            &[format!("provider={}", args.provider)],
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
    Ok(())
}

fn padded_snapshot_return_fetch_range(longest: eli_core::finance::Span) -> eli_core::finance::Span {
    match longest.unit {
        eli_core::finance::SpanUnit::Year => eli_core::finance::Span {
            // Add one month of padding so 1y anchors can land on a prior trading day.
            n: longest.n.saturating_mul(12).saturating_add(1),
            unit: eli_core::finance::SpanUnit::Month,
        },
        eli_core::finance::SpanUnit::Month => eli_core::finance::Span {
            // One month of lookback padding is enough for month-based trailing windows.
            n: longest.n.saturating_add(1),
            unit: eli_core::finance::SpanUnit::Month,
        },
        _ => longest,
    }
}

fn parse_snapshot_return_windows(
    raw_windows: &[String],
) -> Result<Vec<(String, eli_core::finance::Span)>> {
    let mut out: Vec<(String, eli_core::finance::Span)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for w in raw_windows {
        let normalized = w.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            continue;
        }
        if !seen.insert(normalized.clone()) {
            continue;
        }
        let span = match normalized.as_str() {
            "1mo" => eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "3mo" => eli_core::finance::Span {
                n: 3,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "6mo" => eli_core::finance::Span {
                n: 6,
                unit: eli_core::finance::SpanUnit::Month,
            },
            "1y" => eli_core::finance::Span {
                n: 1,
                unit: eli_core::finance::SpanUnit::Year,
            },
            other => {
                anyhow::bail!("invalid --returns window '{other}' (supported: 1mo,3mo,6mo,1y)")
            }
        };
        out.push((normalized, span));
    }
    Ok(out)
}
