use super::super::*;
use tokio::time::{sleep, Duration as TokioDuration};

fn granularity_suggestion(seconds_per_step: i64) -> String {
    let sec = seconds_per_step.max(60);
    let minute = 60;
    let hour = 60 * minute;
    let day = 24 * hour;
    let week = 7 * day;
    let month = 30 * day;
    let year = 365 * day;

    if sec >= year {
        format!("{}y", ((sec + year - 1) / year).max(1))
    } else if sec >= month {
        format!("{}mo", ((sec + month - 1) / month).max(1))
    } else if sec >= week {
        format!("{}w", ((sec + week - 1) / week).max(1))
    } else if sec >= day {
        format!("{}d", ((sec + day - 1) / day).max(1))
    } else if sec >= hour {
        format!("{}h", ((sec + hour - 1) / hour).max(1))
    } else {
        format!("{}min", ((sec + minute - 1) / minute).max(1))
    }
}

fn periods_per_year(granularity: Span) -> f64 {
    let n = granularity.n.max(1) as f64;
    match granularity.unit {
        SpanUnit::Minute => (252.0 * 24.0 * 60.0) / n,
        SpanUnit::Hour => (252.0 * 24.0) / n,
        SpanUnit::Day => 252.0 / n,
        SpanUnit::Week => 52.0 / n,
        SpanUnit::Month => 12.0 / n,
        SpanUnit::Year => 1.0 / n,
    }
}

fn default_risk_free_rate_annual() -> f64 {
    0.04
}

pub(crate) fn build_snapshot_analytics(snapshots: &[TickerSnapshot]) -> SnapshotAnalytics {
    let mut market_caps: BTreeMap<String, u64> = BTreeMap::new();
    for snap in snapshots {
        if let Some(cap) = snap.market_cap {
            market_caps.insert(snap.ticker.clone(), cap);
        }
    }
    let total_market_cap = if market_caps.is_empty() {
        None
    } else {
        Some(market_caps.values().sum())
    };
    let mut market_cap_weights: BTreeMap<String, f64> = BTreeMap::new();
    if let Some(total) = total_market_cap {
        if total > 0 {
            let denom = total as f64;
            for (ticker, cap) in &market_caps {
                market_cap_weights.insert(ticker.clone(), (*cap as f64) / denom);
            }
        }
    }

    let mut daily_returns: BTreeMap<String, f64> = BTreeMap::new();
    for snap in snapshots {
        if let (Some(curr), Some(prev)) = (snap.current_price, snap.previous_close) {
            if prev != 0.0 {
                daily_returns.insert(snap.ticker.clone(), (curr / prev) - 1.0);
            }
        }
    }
    let daily_returns = if daily_returns.is_empty() {
        None
    } else {
        Some(daily_returns)
    };

    let relative_strength = daily_returns.as_ref().and_then(|dr| {
        if dr.is_empty() {
            return None;
        }
        let mean = dr.values().sum::<f64>() / dr.len() as f64;
        let mut rs: BTreeMap<String, f64> = BTreeMap::new();
        for (ticker, r) in dr {
            rs.insert(ticker.clone(), *r - mean);
        }
        Some(rs)
    });

    // Detect when market is likely closed (all returns exactly 0.0)
    let market_note = daily_returns.as_ref().and_then(|dr| {
        if dr.len() >= 2 && dr.values().all(|r| *r == 0.0) {
            Some(
                "market may be closed — all daily returns are 0.0 (current_price == previous_close)"
                    .to_string(),
            )
        } else {
            None
        }
    });

    SnapshotAnalytics {
        market_caps,
        total_market_cap,
        market_cap_weights,
        daily_returns,
        relative_strength,
        market_note,
    }
}

fn build_timeseries_analytics(series: &[TickerSeries], granularity: Span) -> TimeseriesAnalytics {
    let mut dates: BTreeSet<DateTime<Utc>> = BTreeSet::new();
    for s in series {
        for candle in &s.candles {
            dates.insert(candle.t);
        }
    }
    let aligned_dates: Vec<DateTime<Utc>> = dates.into_iter().collect();

    let mut aligned_returns: BTreeMap<String, Vec<Option<f64>>> = BTreeMap::new();
    let mut stats: BTreeMap<String, TimeseriesStats> = BTreeMap::new();

    let per_year = periods_per_year(granularity);
    let rf_annual = default_risk_free_rate_annual();
    let rf_per_period = if per_year > 0.0 {
        rf_annual / per_year
    } else {
        0.0
    };

    for s in series {
        let mut price_map: HashMap<DateTime<Utc>, f64> = HashMap::new();
        for candle in &s.candles {
            price_map.insert(candle.t, candle.c);
        }
        let mut prices: Vec<Option<f64>> = Vec::with_capacity(aligned_dates.len());
        for d in &aligned_dates {
            prices.push(price_map.get(d).copied());
        }

        // Align by union of timestamps; forward-fill gaps once the series has started.
        let mut last: Option<f64> = None;
        for p in &mut prices {
            if p.is_some() {
                last = *p;
            } else if let Some(v) = last {
                *p = Some(v);
            }
        }

        let mut returns: Vec<Option<f64>> = Vec::with_capacity(prices.len());
        for i in 0..prices.len() {
            if i == 0 {
                returns.push(None);
                continue;
            }
            match (prices[i], prices[i - 1]) {
                (Some(curr), Some(prev)) if prev != 0.0 => returns.push(Some((curr / prev) - 1.0)),
                _ => returns.push(None),
            }
        }

        let first = prices.iter().find_map(|v| *v);
        let last = prices.iter().rev().find_map(|v| *v);
        let total_return = match (first, last) {
            (Some(f), Some(l)) if f != 0.0 => Some((l / f) - 1.0),
            _ => None,
        };

        let valid_returns: Vec<f64> = returns.iter().filter_map(|v| *v).collect();
        let (annualized_vol, sharpe_ratio) = if valid_returns.len() >= 2 {
            let mean = valid_returns.iter().sum::<f64>() / valid_returns.len() as f64;
            let mut var = 0.0;
            for r in &valid_returns {
                var += (*r - mean) * (*r - mean);
            }
            let denom = (valid_returns.len() as f64 - 1.0).max(1.0);
            let std = (var / denom).sqrt();
            if std > 0.0 {
                let ann_vol = std * per_year.sqrt();
                let sharpe = (mean - rf_per_period) * per_year.sqrt() / std;
                (Some(ann_vol), Some(sharpe))
            } else {
                (Some(0.0), None)
            }
        } else {
            (None, None)
        };

        aligned_returns.insert(s.ticker.clone(), returns);
        stats.insert(
            s.ticker.clone(),
            TimeseriesStats {
                total_return,
                annualized_vol,
                sharpe_ratio,
                relative_strength: None,
            },
        );
    }

    // Relative strength: outperformance vs the mean total return of the request.
    let mean_total_return = {
        let vals: Vec<f64> = stats.values().filter_map(|s| s.total_return).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<f64>() / vals.len() as f64)
        }
    };
    if let Some(mean) = mean_total_return {
        for s in stats.values_mut() {
            if let Some(tr) = s.total_return {
                s.relative_strength = Some(tr - mean);
            }
        }
    }

    let mut correlation_matrix: BTreeMap<String, BTreeMap<String, Option<f64>>> = BTreeMap::new();
    let tickers: Vec<String> = aligned_returns.keys().cloned().collect();
    for t1 in &tickers {
        let mut row: BTreeMap<String, Option<f64>> = BTreeMap::new();
        for t2 in &tickers {
            let r1 = aligned_returns.get(t1).cloned().unwrap_or_default();
            let r2 = aligned_returns.get(t2).cloned().unwrap_or_default();
            let mut xs: Vec<f64> = Vec::new();
            let mut ys: Vec<f64> = Vec::new();
            let n = r1.len().min(r2.len());
            for i in 0..n {
                if let (Some(a), Some(b)) = (r1[i], r2[i]) {
                    xs.push(a);
                    ys.push(b);
                }
            }
            row.insert(t2.clone(), correlation(&xs, &ys));
        }
        correlation_matrix.insert(t1.clone(), row);
    }

    TimeseriesAnalytics {
        stats,
        correlation_matrix,
        periods_per_year: per_year,
        risk_free_rate_annual: rf_annual,
    }
}

fn correlation(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < 2 || ys.len() < 2 || xs.len() != ys.len() {
        return None;
    }
    let mean_x = xs.iter().sum::<f64>() / xs.len() as f64;
    let mean_y = ys.iter().sum::<f64>() / ys.len() as f64;
    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    for i in 0..xs.len() {
        let dx = xs[i] - mean_x;
        let dy = ys[i] - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }
    if var_x == 0.0 || var_y == 0.0 {
        return None;
    }
    Some(cov / (var_x.sqrt() * var_y.sqrt()))
}

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

fn cache_path(cache_dir: &Path, key: &str) -> PathBuf {
    cache_dir
        .join("finance")
        .join("timeseries")
        .join(format!("{key}.json"))
}

fn debug_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ELI_DEBUG_DIR") {
        return PathBuf::from(dir);
    }
    std::env::temp_dir().join("eli-debug")
}

pub(crate) fn write_debug_payload(tool: &str, request: &str, payload: &str) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(request.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let ts = Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let filename = format!("{tool}_{ts}_{}.json", &hash[..12.min(hash.len())]);
    let path = debug_dir().join(tool).join(filename);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return None;
        }
    }
    if std::fs::write(&path, payload).is_err() {
        return None;
    }
    Some(path.display().to_string())
}

fn generate_mock_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
) -> Vec<TickerSeries> {
    tickers
        .iter()
        .map(|ticker| TickerSeries {
            ticker: ticker.clone(),
            candles: generate_mock_candles(ticker, start, end, step),
        })
        .collect()
}

fn generate_mock_candles(
    ticker: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
) -> Vec<Candle> {
    let mut rng = XorShift64::new(seed_from_str(ticker));
    let mut t = start;
    let mut price = base_price_from_seed(rng.next_u64());
    let mut out = Vec::new();

    while t <= end {
        let open = price;
        let move_pct = (rng.next_f64() - 0.5) * 0.02; // +/-1%
        price = (price * (1.0 + move_pct)).max(0.01);
        let close = price;

        let wick = rng.next_f64() * 0.005; // up to 0.5%
        let high = open.max(close) * (1.0 + wick);
        let low = open.min(close) * (1.0 - wick).max(0.0);
        let vol = Some((rng.next_f64() * 1_000_000.0).round());

        out.push(Candle {
            t,
            o: round_4(open),
            h: round_4(high),
            l: round_4(low),
            c: round_4(close),
            v: vol,
        });

        match t.checked_add_signed(step) {
            Some(next) => t = next,
            None => break,
        }
    }

    out
}

fn seed_from_str(s: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let bytes = hasher.finalize();
    let mut seed = 0u64;
    for b in bytes[..8].iter() {
        seed = (seed << 8) | (*b as u64);
    }
    seed
}

fn base_price_from_seed(seed: u64) -> f64 {
    let v = (seed % 20_000) as f64;
    10.0 + v / 10.0 // 10..2010
}

pub(crate) fn generate_mock_snapshots(tickers: &[String]) -> Vec<TickerSnapshot> {
    tickers
        .iter()
        .map(|ticker| {
            let seed = seed_from_str(ticker);
            let price = round_4(base_price_from_seed(seed));
            // 0.1B .. 20.1B shares
            let shares = 100_000_000u64 + (seed % 20_000_000_000u64);
            let market_cap = (price * (shares as f64)).round() as u64;

            TickerSnapshot {
                ticker: ticker.clone(),
                currency: Some("USD".to_string()),
                exchange: Some("MOCK".to_string()),
                short_name: Some(format!("{ticker} Corp")),
                long_name: Some(format!("{ticker} Corporation")),
                current_price: Some(price),
                previous_close: Some(round_4(price * 0.995)),
                open: Some(round_4(price * 1.002)),
                day_low: Some(round_4(price * 0.99)),
                day_high: Some(round_4(price * 1.01)),
                market_cap: Some(market_cap),
                enterprise_value: Some(market_cap as i64),
                shares_outstanding: Some(shares),
                float_shares: Some(shares.saturating_sub(shares / 10)),
                last_split_factor: None,
                last_split_date: None,
            }
        })
        .collect()
}

pub(crate) async fn fetch_yahoo_snapshots(tickers: &[String]) -> Result<Vec<TickerSnapshot>> {
    let mut connector = yahoo_finance_api::YahooConnector::new()
        .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

    let mut out = Vec::with_capacity(tickers.len());
    for ticker in tickers {
        let info = connector.get_ticker_info(ticker).await.map_err(|e| {
            Error::Provider(format!("yahoo quote summary failed for '{ticker}': {e}"))
        })?;

        let qs = info.quote_summary.ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary missing for '{ticker}'"))
        })?;
        let result = qs.result.ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary result missing for '{ticker}'"))
        })?;
        let first = result.get(0).ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary result empty for '{ticker}'"))
        })?;

        let quote_type = first.quote_type.as_ref();
        let summary = first.summary_detail.as_ref();
        let stats = first.default_key_statistics.as_ref();
        let fin = first.financial_data.as_ref();

        let currency = summary.and_then(|s| s.currency.clone());
        let exchange = quote_type.and_then(|q| q.exchange.clone());
        let short_name = quote_type.and_then(|q| q.short_name.clone());
        let long_name = quote_type.and_then(|q| q.long_name.clone());

        let mut current_price = fin.and_then(|f| f.current_price);
        let previous_close =
            summary.and_then(|s| s.regular_market_previous_close.or(s.previous_close));
        let open = summary.and_then(|s| s.regular_market_open.or(s.open));
        let day_low = summary.and_then(|s| s.regular_market_day_low.or(s.day_low));
        let day_high = summary.and_then(|s| s.regular_market_day_high.or(s.day_high));

        if current_price.is_none() {
            current_price = previous_close
                .or(open)
                .or_else(|| match (day_low, day_high) {
                    (Some(lo), Some(hi)) if lo.is_finite() && hi.is_finite() => {
                        Some((lo + hi) / 2.0)
                    }
                    _ => None,
                });
        }

        let enterprise_value = stats.and_then(|s| s.enterprise_value);
        let shares_outstanding = stats.and_then(|s| s.shares_outstanding);
        let float_shares = stats.and_then(|s| s.float_shares);

        let mut market_cap = summary.and_then(|s| s.market_cap);
        if market_cap.is_none() {
            if let (Some(px), Some(sh)) = (current_price, shares_outstanding) {
                if px.is_finite() && px > 0.0 {
                    market_cap = Some((px * (sh as f64)).round() as u64);
                }
            }
        }

        let last_split_factor = stats.and_then(|s| s.last_split_factor.clone());
        let last_split_date = stats
            .and_then(|s| s.last_split_date)
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single());

        out.push(TickerSnapshot {
            ticker: ticker.clone(),
            currency,
            exchange,
            short_name,
            long_name,
            current_price,
            previous_close,
            open,
            day_low,
            day_high,
            market_cap,
            enterprise_value,
            shares_outstanding,
            float_shares,
            last_split_factor,
            last_split_date,
        });
    }

    Ok(out)
}

fn yahoo_alias_ticker(ticker: &str) -> Option<&'static str> {
    match ticker.trim().to_ascii_uppercase().as_str() {
        "DXY" => Some("DX-Y.NYB"),
        _ => None,
    }
}

async fn fetch_yahoo_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
    max_points_per_ticker: Option<usize>,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let (interval, base_span) = yahoo_base_interval(granularity);
    let include_prepost = matches!(base_span.unit, SpanUnit::Minute | SpanUnit::Hour);

    let base_step = base_span.approx_duration();
    let base_step_seconds = base_step.num_seconds();
    if base_step_seconds <= 0 {
        return Err(Error::InvalidInput("invalid base interval".to_string()));
    }

    let approx_points = ((end - start).num_seconds() / base_step_seconds).max(1) as usize + 1;
    if let Some(max_points_per_ticker) = max_points_per_ticker {
        if approx_points > max_points_per_ticker {
            return Err(Error::InvalidInput(format!(
                "requested ~{approx_points} raw points per ticker exceeds limit {max_points_per_ticker}; increase granularity or shrink range"
            )));
        }
    }

    let start_ts = time::OffsetDateTime::from_unix_timestamp(start.timestamp())
        .map_err(|e| Error::Provider(format!("invalid start timestamp: {e}")))?;
    let request_end = end + Duration::days(1);
    let end_ts = time::OffsetDateTime::from_unix_timestamp(request_end.timestamp())
        .map_err(|e| Error::Provider(format!("invalid end timestamp: {e}")))?;

    let requested_step = granularity.approx_duration();
    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();

    for ticker in tickers {
        let request_ticker = yahoo_alias_ticker(ticker).unwrap_or(ticker.as_str());
        let quotes = match yahoo_fetch_quotes_retry(
            request_ticker,
            start_ts,
            end_ts,
            interval,
            include_prepost,
        )
        .await
        {
            Ok(quotes) => quotes,
            Err(err) => {
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: err.to_string(),
                });
                continue;
            }
        };

        let mut candles = Vec::with_capacity(quotes.len());
        let mut invalid_timestamp = None;
        for q in quotes {
            let Some(t) = Utc.timestamp_opt(q.timestamp as i64, 0).single() else {
                invalid_timestamp = Some(q.timestamp);
                break;
            };
            if t < start || t > end {
                continue;
            }
            candles.push(Candle {
                t,
                o: q.open,
                h: q.high,
                l: q.low,
                c: q.close,
                v: Some(q.volume as f64),
            });
        }

        if let Some(ts) = invalid_timestamp {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message: format!("yahoo invalid timestamp: {ts}"),
            });
            continue;
        }

        candles.sort_by_key(|c| c.t);
        let candles = if requested_step == base_step {
            candles
        } else {
            resample_candles(&candles, start, requested_step)
        };

        if candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: "yahoo returned no data points in the requested range".to_string(),
            });
            continue;
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
        });
    }

    Ok((out, errors))
}

async fn yahoo_fetch_quotes_retry(
    ticker: &str,
    start: time::OffsetDateTime,
    end: time::OffsetDateTime,
    interval: &str,
    include_prepost: bool,
) -> Result<Vec<yahoo_finance_api::Quote>> {
    const MAX_ATTEMPTS: usize = 3;
    let mut last_err: Option<String> = None;

    for attempt in 0..MAX_ATTEMPTS {
        let connector = yahoo_finance_api::YahooConnector::new()
            .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

        let resp = if include_prepost {
            connector
                .get_quote_history_interval_prepost(ticker, start, end, interval, true)
                .await
        } else {
            connector
                .get_quote_history_interval(ticker, start, end, interval)
                .await
        };

        match resp {
            Ok(resp) => match resp.quotes() {
                Ok(quotes) if !quotes.is_empty() => return Ok(quotes),
                Ok(_) => last_err = Some("yahoo returned 0 quotes".to_string()),
                Err(e) => last_err = Some(e.to_string()),
            },
            Err(e) => {
                last_err = Some(e.to_string());

                let retryable = matches!(
                    e,
                    yahoo_finance_api::YahooError::ConnectionFailed(_)
                        | yahoo_finance_api::YahooError::FetchFailed(_)
                        | yahoo_finance_api::YahooError::DeserializeFailed(_)
                        | yahoo_finance_api::YahooError::DeserializeFailedDebug(_)
                        | yahoo_finance_api::YahooError::TooManyRequests(_)
                        | yahoo_finance_api::YahooError::Unauthorized
                        | yahoo_finance_api::YahooError::InvalidCrumb
                        | yahoo_finance_api::YahooError::NoCookies
                        | yahoo_finance_api::YahooError::InvalidCookie
                );

                if !retryable {
                    break;
                }
            }
        }

        if attempt + 1 < MAX_ATTEMPTS {
            let backoff_ms = 250u64.saturating_mul((attempt as u64) + 1);
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }
    }

    Err(Error::Provider(format!(
        "yahoo returned no data for '{ticker}' ({})",
        last_err.unwrap_or_else(|| "unknown error".to_string())
    )))
}

fn yahoo_base_interval(granularity: Span) -> (&'static str, Span) {
    // Largest supported interval <= requested granularity.
    // Supported intervals: 1m,2m,5m,15m,30m,90m,1h,1d,5d,1wk,1mo,3mo
    let requested = granularity.approx_duration().num_seconds().max(1);
    let mut best: (&'static str, Span, i64) = (
        "1m",
        Span {
            n: 1,
            unit: SpanUnit::Minute,
        },
        60,
    );

    let candidates: &[(&str, Span)] = &[
        (
            "1m",
            Span {
                n: 1,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "2m",
            Span {
                n: 2,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "5m",
            Span {
                n: 5,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "15m",
            Span {
                n: 15,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "30m",
            Span {
                n: 30,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "90m",
            Span {
                n: 90,
                unit: SpanUnit::Minute,
            },
        ),
        (
            "1h",
            Span {
                n: 1,
                unit: SpanUnit::Hour,
            },
        ),
        (
            "1d",
            Span {
                n: 1,
                unit: SpanUnit::Day,
            },
        ),
        (
            "5d",
            Span {
                n: 5,
                unit: SpanUnit::Day,
            },
        ),
        (
            "1wk",
            Span {
                n: 1,
                unit: SpanUnit::Week,
            },
        ),
        (
            "1mo",
            Span {
                n: 1,
                unit: SpanUnit::Month,
            },
        ),
        (
            "3mo",
            Span {
                n: 3,
                unit: SpanUnit::Month,
            },
        ),
    ];

    for (interval, span) in candidates {
        let secs = span.approx_duration().num_seconds();
        if secs <= 0 || secs > requested {
            continue;
        }
        if secs >= best.2 {
            best = (*interval, *span, secs);
        }
    }

    (best.0, best.1)
}

pub(crate) async fn fetch_fred_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    if matches!(granularity.unit, SpanUnit::Minute | SpanUnit::Hour) {
        return Err(Error::InvalidInput(
            "fred provider does not support sub-daily granularity".to_string(),
        ));
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| Error::Provider(format!("fred client init failed: {e}")))?;
    let start_date = start.date_naive().format("%Y-%m-%d").to_string();
    let end_date = end.date_naive().format("%Y-%m-%d").to_string();
    let step = granularity.approx_duration();

    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();
    for series_id in tickers {
        let mut body: Option<String> = None;
        let mut last_err: Option<String> = None;
        for attempt in 0..3 {
            let resp = client
                .get("https://fred.stlouisfed.org/graph/fredgraph.csv")
                .query(&[
                    ("id", series_id.as_str()),
                    ("cosd", start_date.as_str()),
                    ("coed", end_date.as_str()),
                ])
                .send()
                .await;

            let resp = match resp {
                Ok(resp) => resp,
                Err(e) => {
                    last_err = Some(format!("fred fetch failed: {e}"));
                    if attempt < 2 {
                        sleep(TokioDuration::from_millis(250 * (attempt + 1) as u64)).await;
                        continue;
                    }
                    break;
                }
            };

            if !resp.status().is_success() {
                last_err = Some(format!("fred fetch failed: http {}", resp.status()));
                if attempt < 2 {
                    sleep(TokioDuration::from_millis(300 * (attempt + 1) as u64)).await;
                    continue;
                }
                break;
            }

            match resp.text().await {
                Ok(txt) => {
                    body = Some(txt);
                    break;
                }
                Err(e) => {
                    last_err = Some(format!("fred read failed: {e}"));
                    if attempt < 2 {
                        sleep(TokioDuration::from_millis(250 * (attempt + 1) as u64)).await;
                        continue;
                    }
                    break;
                }
            }
        }
        let body = match body {
            Some(b) => b,
            None => {
                errors.push(TimeseriesError {
                    ticker: series_id.clone(),
                    stage: Some("fetch".to_string()),
                    message: last_err.unwrap_or_else(|| "fred fetch failed".to_string()),
                });
                continue;
            }
        };

        let mut candles = Vec::new();
        for (idx, line) in body.lines().enumerate() {
            if idx == 0 {
                continue;
            }
            let mut parts = line.splitn(2, ',');
            let date_raw = parts.next().unwrap_or("").trim();
            let val_raw = parts.next().unwrap_or("").trim();
            if date_raw.is_empty() || val_raw.is_empty() || val_raw == "." {
                continue;
            }

            let date = match chrono::NaiveDate::parse_from_str(date_raw, "%Y-%m-%d") {
                Ok(date) => date,
                Err(_) => {
                    errors.push(TimeseriesError {
                        ticker: series_id.clone(),
                        stage: Some("parse".to_string()),
                        message: format!("fred invalid date '{date_raw}'"),
                    });
                    candles.clear();
                    break;
                }
            };
            let t = DateTime::<Utc>::from_naive_utc_and_offset(
                date.and_hms_opt(0, 0, 0)
                    .ok_or_else(|| Error::Provider("fred invalid datetime".to_string()))?,
                Utc,
            );
            if t < start || t > end {
                continue;
            }

            let v: f64 = match val_raw.parse() {
                Ok(v) => v,
                Err(_) => {
                    errors.push(TimeseriesError {
                        ticker: series_id.clone(),
                        stage: Some("parse".to_string()),
                        message: format!("fred invalid value '{val_raw}'"),
                    });
                    candles.clear();
                    break;
                }
            };
            candles.push(Candle {
                t,
                o: v,
                h: v,
                l: v,
                c: v,
                v: None,
            });
        }

        candles.sort_by_key(|c| c.t);
        let candles = resample_candles(&candles, start, step);

        if candles.is_empty() {
            if !errors.iter().any(|e| e.ticker == series_id.as_str()) {
                errors.push(TimeseriesError {
                    ticker: series_id.clone(),
                    stage: Some("parse".to_string()),
                    message: "fred returned no data points in the requested range".to_string(),
                });
            }
            continue;
        }

        out.push(TickerSeries {
            ticker: series_id.clone(),
            candles,
        });
    }

    Ok((out, errors))
}

fn resample_candles(candles: &[Candle], start: DateTime<Utc>, step: Duration) -> Vec<Candle> {
    let step_seconds = step.num_seconds().max(1);
    let mut out = Vec::new();

    let mut current_bucket: Option<i64> = None;
    let mut bucket: Vec<&Candle> = Vec::new();

    for candle in candles {
        let delta = candle.t - start;
        let bucket_idx = delta.num_seconds().div_euclid(step_seconds);
        if bucket_idx < 0 {
            continue;
        }

        match current_bucket {
            None => {
                current_bucket = Some(bucket_idx);
                bucket.push(candle);
            }
            Some(b) if b == bucket_idx => bucket.push(candle),
            Some(_) => {
                if let Some(agg) = aggregate_bucket(&bucket) {
                    out.push(agg);
                }
                bucket.clear();
                current_bucket = Some(bucket_idx);
                bucket.push(candle);
            }
        }
    }

    if let Some(agg) = aggregate_bucket(&bucket) {
        out.push(agg);
    }

    out
}

fn aggregate_bucket(bucket: &[&Candle]) -> Option<Candle> {
    let first = bucket.first()?;
    let last = bucket.last()?;

    let mut high = first.h;
    let mut low = first.l;
    let mut vol_sum = 0.0;
    let mut saw_vol = false;

    for c in bucket {
        if c.h > high {
            high = c.h;
        }
        if c.l < low {
            low = c.l;
        }
        if let Some(v) = c.v {
            vol_sum += v;
            saw_vol = true;
        }
    }

    Some(Candle {
        t: first.t,
        o: first.o,
        h: high,
        l: low,
        c: last.c,
        v: saw_vol.then_some(vol_sum),
    })
}

fn round_4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let seed = if seed == 0 { 0x9e3779b97f4a7c15 } else { seed };
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        let v = self.next_u64();
        (v as f64) / (u64::MAX as f64)
    }
}
