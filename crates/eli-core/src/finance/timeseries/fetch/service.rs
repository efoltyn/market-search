pub async fn fetch_timeseries(
    req: TimeseriesRequest,
    cache_dir: &Path,
) -> Result<TimeseriesResponse> {
    let tickers = normalize_tickers(&req.tickers);
    if tickers.is_empty() {
        return Err(Error::InvalidInput(
            "at least one ticker is required".to_string(),
        ));
    }

    let max_points = req.max_points_per_ticker.map(|v| v.max(2));

    let now = Utc::now();
    // When `as_of` is unset, the request implicitly says "now" — but the actual fetch
    // and the cache key both use this end timestamp. Without bucketing, every call
    // gets a fresh `now()` → fresh cache key → cache never hits in default usage.
    // Floor to the nearest granularity step so repeated calls within the same bucket
    // share a cache key. Real fetch still uses precise data for the bucket window.
    let mut end = if let Some(asof) = req.as_of {
        asof
    } else {
        floor_to_bucket(now, req.granularity)
    };
    if end > now {
        end = now;
    }
    let start = end
        .checked_sub_signed(req.range.approx_duration())
        .ok_or_else(|| Error::InvalidInput("range underflow".to_string()))?;

    let step = req.granularity.approx_duration();
    if step.num_seconds() <= 0 {
        return Err(Error::InvalidInput("granularity must be > 0".to_string()));
    }

    let approx_points = ((end - start).num_seconds() / step.num_seconds()).max(1) as usize + 1;
    if let Some(max_points) = max_points {
        if approx_points > max_points {
            let window_seconds = (end - start).num_seconds().max(1);
            let min_step_seconds =
                ((window_seconds as f64) / ((max_points - 1).max(1) as f64)).ceil() as i64;
            let suggested = granularity_suggestion(min_step_seconds);
            return Err(Error::InvalidInput(format!(
                "requested ~{approx_points} points per ticker exceeds limit {max_points}; try --granularity {suggested} (or larger) for this window, or reduce range"
            )));
        }
    }

    let cache_key = cache_key(&req, &tickers, start, end)?;
    let cache_path = cache_path(cache_dir, &cache_key);

    if cache_path.exists() {
        let raw = tokio::fs::read_to_string(&cache_path)
            .await
            .map_err(|e| Error::Provider(format!("cache read failed: {e}")))?;
        let mut resp: TimeseriesResponse = serde_json::from_str(&raw)?;
        if resp.analytics.is_none() {
            resp.analytics = Some(build_timeseries_analytics(&resp.series, resp.granularity));
            if let Ok(json) = serde_json::to_string_pretty(&resp) {
                let _ = tokio::fs::write(&cache_path, json).await;
            }
        }
        resp.cache = Some(CacheInfo {
            hit: true,
            path: cache_path.display().to_string(),
            key: cache_key,
        });
        return Ok(resp);
    }

    tokio::fs::create_dir_all(cache_path.parent().unwrap_or(cache_dir))
        .await
        .map_err(|e| Error::Provider(format!("cache dir create failed: {e}")))?;

    let generated_at = Utc::now();
    let (series, errors) = match req.provider {
        ProviderKind::Mock => (generate_mock_series(&tickers, start, end, step), Vec::new()),
        ProviderKind::Yahoo => {
            fetch_yahoo_series(&tickers, start, end, req.granularity, max_points).await?
        }
        ProviderKind::Fred if is_h15_yield_curve_request(&tickers) => {
            // Route yield curve tickers to the Fed H.15 endpoint (federalreserve.gov)
            // instead of FRED (fred.stlouisfed.org). Faster, no Akamai, primary source.
            match fetch_h15_yield_curve(&tickers, start, end, req.granularity).await {
                Ok(result) => result,
                Err(_) => {
                    // Fall back to FRED if H.15 is unavailable
                    fetch_fred_series(&tickers, start, end, req.granularity).await?
                }
            }
        }
        ProviderKind::Fred => fetch_fred_series(&tickers, start, end, req.granularity).await?,
        ProviderKind::Ibkr => crate::finance::fetch_ibkr_timeseries(&req).await?,
        ProviderKind::Pyth => fetch_pyth_series(&tickers, start, end, req.granularity).await?,
        ProviderKind::Binance => fetch_binance_series(&tickers, start, end, req.granularity).await?,
        ProviderKind::Eia | ProviderKind::Ecb => {
            return Err(Error::InvalidInput(
                "EIA/ECB timeseries providers are not yet integrated into the core fetch path; use the dedicated CLI tools".to_string(),
            ));
        }
        ProviderKind::Kalshi | ProviderKind::Polymarket => {
            return Err(Error::InvalidInput(
                "Kalshi/Polymarket timeseries is handled by the CLI layer (use --ticker KX* or --odds-provider)".to_string(),
            ));
        }
    };

    if !errors.is_empty() {
        let valid_tickers: Vec<String> = series.iter().map(|s| s.ticker.clone()).collect();
        // Partial failure: return whatever series succeeded alongside the errors.
        let (status, error_info) = if series.is_empty() {
            (
                "error".to_string(),
                Some(ToolErrorInfo {
                    error: "TickerFetchFailed".to_string(),
                    message: "All tickers failed to fetch timeseries data.".to_string(),
                    hint: Some(
                        "All requested tickers must be valid for this provider. An empty result is also expected when a short intraday window (e.g. range=1d with 15m/1h granularity) falls entirely on a weekend or US market holiday — widen to range=5d or use granularity=1d to capture the last trading session.".to_string(),
                    ),
                    debug: None,
                }),
            )
        } else {
            (
                "partial".to_string(),
                Some(ToolErrorInfo {
                    error: "PartialTickerFetchFailed".to_string(),
                    message: format!(
                        "{} of {} tickers failed; {} series returned.",
                        errors.len(),
                        tickers.len(),
                        series.len()
                    ),
                    hint: Some("Some tickers had no data in the requested range.".to_string()),
                    debug: None,
                }),
            )
        };
        let analytics = if series.is_empty() {
            None
        } else {
            Some(build_timeseries_analytics(&series, req.granularity))
        };
        return Ok(TimeseriesResponse {
            provider: req.provider,
            sources: Vec::new(),
            tickers: tickers.clone(),
            granularity: req.granularity,
            range: req.range,
            start,
            end,
            generated_at,
            series,
            status: Some(status),
            error: error_info,
            errors: Some(errors),
            valid_tickers: if valid_tickers.is_empty() {
                None
            } else {
                Some(valid_tickers)
            },
            analytics,
            cache: None,
        });
    }

    let resp = TimeseriesResponse {
        provider: req.provider,
        sources: Vec::new(),
        tickers: tickers.clone(),
        granularity: req.granularity,
        range: req.range,
        start,
        end,
        generated_at,
        series,
        status: None,
        error: None,
        errors: None,
        valid_tickers: None,
        analytics: None,
        cache: Some(CacheInfo {
            hit: false,
            path: cache_path.display().to_string(),
            key: cache_key.clone(),
        }),
    };

    let mut resp = resp;
    resp.analytics = Some(build_timeseries_analytics(&resp.series, resp.granularity));

    let json = serde_json::to_string_pretty(&resp)?;
    tokio::fs::write(&cache_path, json)
        .await
        .map_err(|e| Error::Provider(format!("cache write failed: {e}")))?;

    Ok(resp)
}

/// Floor a timestamp to the nearest granularity bucket boundary.
/// Caps bucket size at 1 day so weekly/monthly granularity still hits the cache
/// once per day rather than once per week/month (which would be too coarse for
/// freshness).
fn floor_to_bucket(dt: DateTime<Utc>, granularity: Span) -> DateTime<Utc> {
    let raw_secs = granularity.approx_duration().num_seconds();
    let bucket_secs = raw_secs.clamp(60, 86400);
    let ts = dt.timestamp();
    let floored = (ts / bucket_secs) * bucket_secs;
    DateTime::from_timestamp(floored, 0).unwrap_or(dt)
}

fn cache_key(
    req: &TimeseriesRequest,
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<String> {
    #[derive(Serialize)]
    struct Key<'a> {
        v: u32,
        provider: &'a ProviderKind,
        tickers: Vec<&'a str>,
        range: String,
        granularity: String,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        max_points_per_ticker: Option<usize>,
        ibkr_account: Option<&'a str>,
        ibkr_market_data_type: Option<i32>,
    }

    let mut tickers_sorted: Vec<&str> = tickers.iter().map(|s| s.as_str()).collect();
    tickers_sorted.sort_unstable();

    let key = Key {
        // v: bump when TickerSeries / Candle / response shape changes so cache
        // entries from old binaries are invalidated. v=2 added source + upstream_id
        // to TickerSeries. v=3 added kind field to Candle (point vs OHLC).
        v: 3,
        provider: &req.provider,
        tickers: tickers_sorted,
        range: req.range.to_string_compact(),
        granularity: req.granularity.to_string_compact(),
        start,
        end,
        max_points_per_ticker: req.max_points_per_ticker.map(|v| v.max(2)),
        ibkr_account: req.ibkr.as_ref().and_then(|v| v.account.as_deref()),
        ibkr_market_data_type: req.ibkr.as_ref().and_then(|v| v.market_data_type),
    };

    let raw = serde_json::to_vec(&key)?;
    let mut hasher = Sha256::new();
    hasher.update(raw);
    Ok(format!("{:x}", hasher.finalize()))
}
