use super::super::timeseries::fetch::{
    build_snapshot_analytics, fetch_yahoo_snapshots, generate_mock_snapshots,
};
use super::super::*;

async fn fetch_yahoo_snapshots_as_of(
    tickers: &[String],
    as_of: DateTime<Utc>,
    collected_at: DateTime<Utc>,
    freshness_policy: &crate::finance::policy::FreshnessPolicy,
) -> Result<(Vec<TickerSnapshot>, Vec<SnapshotError>)> {
    let (mut snapshots, errors) =
        fetch_yahoo_snapshots(tickers, collected_at, freshness_policy).await?;
    if snapshots.is_empty() {
        return Ok((snapshots, errors));
    }
    let valid_tickers: Vec<String> = snapshots.iter().map(|s| s.ticker.clone()).collect();
    let cache_dir = std::env::temp_dir().join("eli-finance-cache");
    let intraday_req = TimeseriesRequest {
        tickers: valid_tickers.clone(),
        range: Span {
            n: 7,
            unit: SpanUnit::Day,
        },
        granularity: Span {
            n: 5,
            unit: SpanUnit::Minute,
        },
        as_of: Some(as_of),
        provider: ProviderKind::Yahoo,
        max_points_per_ticker: None,
        ibkr: None,
    };
    let daily_req = TimeseriesRequest {
        tickers: valid_tickers,
        range: Span {
            n: 30,
            unit: SpanUnit::Day,
        },
        granularity: Span {
            n: 1,
            unit: SpanUnit::Day,
        },
        as_of: Some(as_of),
        provider: ProviderKind::Yahoo,
        max_points_per_ticker: None,
        ibkr: None,
    };

    let (intraday, daily) = tokio::join!(
        crate::finance::fetch_timeseries(intraday_req, &cache_dir),
        crate::finance::fetch_timeseries(daily_req, &cache_dir),
    );
    let intraday = intraday.ok();
    let daily = daily.ok();
    let intraday_map: std::collections::HashMap<String, TickerSeries> = intraday
        .map(|resp| {
            resp.series
                .into_iter()
                .map(|series| (series.ticker.clone(), series))
                .collect()
        })
        .unwrap_or_default();
    let daily_map: std::collections::HashMap<String, TickerSeries> = daily
        .map(|resp| {
            resp.series
                .into_iter()
                .map(|series| (series.ticker.clone(), series))
                .collect()
        })
        .unwrap_or_default();

    for snapshot in &mut snapshots {
        let Some(series) = intraday_map.get(&snapshot.ticker) else {
            snapshot.effective_at = Some(snapshot.freshness.observed_at);
            continue;
        };
        let Some(latest) = series.candles.last() else {
            snapshot.effective_at = Some(snapshot.freshness.observed_at);
            continue;
        };

        let latest_day = latest.t.date_naive();
        let session_candles: Vec<&Candle> = series
            .candles
            .iter()
            .filter(|c| c.t.date_naive() == latest_day)
            .collect();
        let previous_intraday_close = series
            .candles
            .iter()
            .rev()
            .find(|c| c.t.date_naive() < latest_day)
            .map(|c| c.c);
        let previous_daily_close = daily_map.get(&snapshot.ticker).and_then(|daily_series| {
            daily_series
                .candles
                .iter()
                .rev()
                .find(|c| c.t.date_naive() < latest_day)
                .map(|c| c.c)
        });

        let open = session_candles.first().map(|c| c.o).or(snapshot.open);
        let day_low = session_candles
            .iter()
            .map(|c| c.l)
            .reduce(f64::min)
            .or(snapshot.day_low);
        let day_high = session_candles
            .iter()
            .map(|c| c.h)
            .reduce(f64::max)
            .or(snapshot.day_high);
        let previous_close = previous_intraday_close
            .or(previous_daily_close)
            .or(snapshot.previous_close);
        let current_price = Some(latest.c);

        snapshot.current_price = current_price;
        snapshot.price = current_price;
        snapshot.open = open;
        snapshot.day_low = day_low;
        snapshot.day_high = day_high;
        snapshot.previous_close = previous_close;
        snapshot.daily_return = match (current_price, previous_close) {
            (Some(px), Some(prev)) if prev.is_finite() && prev != 0.0 => Some((px / prev) - 1.0),
            _ => None,
        };
        if let (Some(px), Some(shares)) = (current_price, snapshot.shares_outstanding) {
            if px.is_finite() && px > 0.0 {
                snapshot.market_cap = Some((px * shares as f64).round() as u64);
            }
        }
        snapshot.freshness = Freshness::new(
            latest.t,
            collected_at,
            FreshnessState::Historical,
            FreshnessOrigin::Derived,
            FreshnessQuality::Estimated,
        );
        snapshot.price_source_kind = "historical_bar_reconstruction".to_string();
        snapshot.session_state = "historical_locked".to_string();
        snapshot.market_closed_fallback = false;
        snapshot.effective_at = Some(latest.t);
    }

    Ok((snapshots, errors))
}

pub async fn fetch_snapshot(req: SnapshotRequest) -> Result<SnapshotResponse> {
    let started = std::time::Instant::now();
    let tickers = normalize_tickers(&req.tickers);
    if tickers.is_empty() {
        return Err(Error::InvalidInput(
            "at least one ticker is required".to_string(),
        ));
    }

    let generated_at = Utc::now();
    let resolved_policy = crate::finance::policy::load_policy(None, PolicyMode::Observe)?;
    let provider = req.provider.clone();
    // decision_trace records CHOICES that altered the response (partial_results,
    // fallbacks, etc.). Empty by default; populated only when something noteworthy fired.
    let mut decision_trace: Vec<String> = Vec::new();
    let (snapshots, snapshot_errors) = match provider {
        ProviderKind::Mock => (generate_mock_snapshots(&tickers), Vec::new()),
        ProviderKind::Yahoo => match req.as_of {
            Some(as_of) => {
                fetch_yahoo_snapshots_as_of(
                    &tickers,
                    as_of,
                    generated_at,
                    &resolved_policy.policy.freshness,
                )
                .await?
            }
            None => {
                fetch_yahoo_snapshots(&tickers, generated_at, &resolved_policy.policy.freshness)
                    .await?
            }
        },
        ProviderKind::Ibkr => (crate::finance::fetch_ibkr_snapshot(&req).await?, Vec::new()),
        ProviderKind::Fred => {
            return Err(Error::InvalidInput(
                "fred provider does not support snapshot (use timeseries for macro series IDs)"
                    .to_string(),
            ))
        }
        ProviderKind::Pyth => {
            return Err(Error::InvalidInput(
                "pyth provider does not support snapshot (use timeseries with PYTH: prefix)"
                    .to_string(),
            ))
        }
        ProviderKind::Kalshi | ProviderKind::Polymarket => {
            return Err(Error::InvalidInput(
                "prediction market providers do not support snapshot (use timeseries with --odds-provider)"
                    .to_string(),
            ))
        }
        ProviderKind::Binance | ProviderKind::Eia | ProviderKind::Ecb => {
            return Err(Error::InvalidInput(format!(
                "{:?} provider does not support snapshot (use timeseries)",
                provider
            )))
        }
    };
    let analytics = build_snapshot_analytics(&snapshots);
    let valid_tickers: Vec<String> = snapshots.iter().map(|s| s.ticker.clone()).collect();
    let partial_failure = !snapshot_errors.is_empty();
    if partial_failure {
        decision_trace.push(format!("partial_results={}", !snapshots.is_empty()));
    }
    let data_as_of = snapshots.iter().map(|s| s.freshness.observed_at).max();
    let max_age_seconds = snapshots.iter().map(|s| s.freshness.age_seconds).max();
    let stale_count = snapshots
        .iter()
        .filter(|s| matches!(s.freshness.state, FreshnessState::Stale))
        .count();
    let snapshot_count = snapshots.len();
    let market_closed_fallback_count = snapshots
        .iter()
        .filter(|s| s.market_closed_fallback)
        .count();
    let status = if partial_failure {
        Some(if snapshots.is_empty() {
            "error".to_string()
        } else {
            "partial".to_string()
        })
    } else {
        None
    };
    let error = if partial_failure {
        Some(ToolErrorInfo {
            error: if snapshots.is_empty() {
                "TickerFetchFailed".to_string()
            } else {
                "TickerFetchPartial".to_string()
            },
            message: if snapshots.is_empty() {
                "all requested tickers failed to fetch snapshot data".to_string()
            } else {
                "one or more tickers failed to fetch snapshot data; partial results returned"
                    .to_string()
            },
            hint: Some("Check .errors[] for per-ticker failures.".to_string()),
            debug: None,
        })
    } else {
        None
    };

    Ok(SnapshotResponse {
        provider,
        tickers,
        generated_at,
        snapshots,
        schema_version: "finance.snapshot.v2".to_string(),
        freshness_summary: FreshnessSummary {
            data_as_of,
            max_age_seconds,
            stale_count,
        },
        applied_policy: AppliedPolicy {
            mode: resolved_policy.mode,
            sources: resolved_policy.sources,
        },
        decision_trace,
        run_meta: RunMeta {
            latency_ms: started.elapsed().as_millis() as u64,
            stdout_chars: 0,
            stored_bytes: 0,
            coverage_counts: std::collections::BTreeMap::from([
                ("snapshots".to_string(), snapshot_count),
                ("errors".to_string(), snapshot_errors.len()),
            ]),
            token_efficiency: None,
        },
        market_closed_fallback_count,
        has_market_closed_fallback: market_closed_fallback_count > 0,
        status,
        error,
        errors: if snapshot_errors.is_empty() {
            None
        } else {
            Some(snapshot_errors)
        },
        valid_tickers: if partial_failure && !valid_tickers.is_empty() {
            Some(valid_tickers)
        } else {
            None
        },
        analytics: Some(analytics),
        trailing_returns: None,
    })
}
