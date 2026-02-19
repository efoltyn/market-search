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
    let mut end = req.as_of.unwrap_or(now);
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
        ProviderKind::Fred => fetch_fred_series(&tickers, start, end, req.granularity).await?,
    };

    if !errors.is_empty() {
        let valid_tickers: Vec<String> = series.iter().map(|s| s.ticker.clone()).collect();
        let error = ToolErrorInfo {
            error: "TickerFetchFailed".to_string(),
            message: "One or more tickers failed to fetch timeseries data; no series returned."
                .to_string(),
            hint: Some("All requested tickers must be valid for this provider.".to_string()),
            debug: None,
        };
        return Ok(TimeseriesResponse {
            provider: req.provider,
            tickers: tickers.clone(),
            granularity: req.granularity,
            range: req.range,
            start,
            end,
            generated_at,
            series: Vec::new(),
            status: Some("error".to_string()),
            error: Some(error),
            errors: Some(errors),
            valid_tickers: if valid_tickers.is_empty() {
                None
            } else {
                Some(valid_tickers)
            },
            analytics: None,
            cache: None,
        });
    }

    let resp = TimeseriesResponse {
        provider: req.provider,
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
    }

    let mut tickers_sorted: Vec<&str> = tickers.iter().map(|s| s.as_str()).collect();
    tickers_sorted.sort_unstable();

    let key = Key {
        v: 1,
        provider: &req.provider,
        tickers: tickers_sorted,
        range: req.range.to_string_compact(),
        granularity: req.granularity.to_string_compact(),
        start,
        end,
        max_points_per_ticker: req.max_points_per_ticker.map(|v| v.max(2)),
    };

    let raw = serde_json::to_vec(&key)?;
    let mut hasher = Sha256::new();
    hasher.update(raw);
    Ok(format!("{:x}", hasher.finalize()))
}

