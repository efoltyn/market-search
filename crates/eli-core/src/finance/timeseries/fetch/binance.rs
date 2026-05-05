/// Binance public klines (candlestick) provider.
///
/// Endpoint: https://api.binance.us/api/v3/klines?symbol={PAIR}&interval={1d}&limit=1000
///
/// Coverage: crypto pairs (BTCUSDT, ETHUSDT, SOLUSDT, etc.)
/// Depth: binance.us back to Sept 2019, binance.com back to 2017 (geo-blocked from US)
/// No auth required. Rate limit: 1200 weight/min, klines = 2 weight/req.
///
/// Response: array of 12-element arrays (positional):
///   [0] open_time_ms, [1] open, [2] high, [3] low, [4] close,
///   [5] volume, [6] close_time_ms, [7] quote_volume, ...

const BINANCE_US_BASE: &str = "https://api.binance.us/api/v3/klines";

fn binance_interval(granularity: Span) -> Option<&'static str> {
    let secs = granularity.approx_duration().num_seconds();
    match secs {
        60 => Some("1m"),
        180 => Some("3m"),
        300 => Some("5m"),
        900 => Some("15m"),
        1800 => Some("30m"),
        3600 => Some("1h"),
        7200 => Some("2h"),
        14400 => Some("4h"),
        21600 => Some("6h"),
        28800 => Some("8h"),
        43200 => Some("12h"),
        86400 => Some("1d"),
        259200 => Some("3d"),
        604800 => Some("1w"),
        s if s >= 2_592_000 => Some("1M"),
        _ => None,
    }
}

/// Best Binance interval <= requested, or exact match.
fn binance_best_interval(granularity: Span) -> (&'static str, i64) {
    let requested = granularity.approx_duration().num_seconds().max(1);
    let candidates: &[(&str, i64)] = &[
        ("1m", 60), ("3m", 180), ("5m", 300), ("15m", 900), ("30m", 1800),
        ("1h", 3600), ("2h", 7200), ("4h", 14400), ("6h", 21600),
        ("8h", 28800), ("12h", 43200), ("1d", 86400), ("3d", 259200),
        ("1w", 604800), ("1M", 2592000),
    ];
    // Exact match first.
    for &(interval, secs) in candidates {
        if secs == requested {
            return (interval, secs);
        }
    }
    // Largest that divides evenly.
    let mut best = ("1d", 86400_i64);
    for &(interval, secs) in candidates {
        if secs > requested || secs <= 0 { continue; }
        if requested % secs != 0 { continue; }
        if secs >= best.1 || best.1 > requested {
            best = (interval, secs);
        }
    }
    best
}

/// Translate user ticker to Binance symbol.
/// Accepts: BN:BTCUSDT, BN:BTC, BINANCE:ETHUSDT, or bare BTCUSDT.
fn binance_symbol(ticker: &str) -> String {
    let t = ticker.trim();
    let raw = if t.to_ascii_uppercase().starts_with("BN:") {
        &t[3..]
    } else if t.to_ascii_uppercase().starts_with("BINANCE:") {
        &t[8..]
    } else {
        t
    };
    let upper = raw.to_ascii_uppercase();
    // If user provided a full pair (BTCUSD, ETHUSDT, SOLBTC), use as-is.
    // If user provided a bare base currency (BTC, ETH, SOL), append USD.
    // Heuristic: a full pair is >= 6 chars and ends with a known quote currency.
    let is_full_pair = upper.len() >= 6
        && (upper.ends_with("USDT") || upper.ends_with("BUSD") || upper.ends_with("USD")
            || upper.ends_with("BTC") || upper.ends_with("ETH"));
    if is_full_pair {
        upper
    } else {
        format!("{}USD", upper)
    }
}

pub(crate) async fn fetch_binance_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let client = &*crate::finance::shared_client::GENERAL;
    let (interval, interval_secs) = binance_best_interval(granularity);
    let needs_resample = interval_secs != granularity.approx_duration().num_seconds();

    let mut out = Vec::new();
    let mut errors = Vec::new();

    for ticker in tickers {
        let symbol = binance_symbol(ticker);
        let mut all_candles: Vec<Candle> = Vec::new();
        let mut cursor_ms = start.timestamp_millis();
        let end_ms = end.timestamp_millis();

        // Paginate: max 1000 candles per request.
        for _page in 0..50 {
            if cursor_ms >= end_ms { break; }

            let url = format!(
                "{}?symbol={}&interval={}&startTime={}&endTime={}&limit=1000",
                BINANCE_US_BASE, symbol, interval, cursor_ms, end_ms
            );

            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    errors.push(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("fetch".to_string()),
                        message: format!("binance request failed: {e}"),
                    });
                    break;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: format!("binance returned http {status}: {}", body.chars().take(200).collect::<String>()),
                });
                break;
            }

            let body: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    errors.push(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("parse".to_string()),
                        message: format!("binance json parse failed: {e}"),
                    });
                    break;
                }
            };

            let rows = match body.as_array() {
                Some(arr) => arr,
                None => {
                    errors.push(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("parse".to_string()),
                        message: "binance response is not an array".to_string(),
                    });
                    break;
                }
            };

            if rows.is_empty() { break; }

            for row in rows {
                let arr = match row.as_array() {
                    Some(a) if a.len() >= 6 => a,
                    _ => continue,
                };
                let open_time_ms = arr[0].as_i64().unwrap_or(0);
                let close_time_ms = arr[6].as_i64().unwrap_or(0);
                let o: f64 = arr[1].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let h: f64 = arr[2].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let l: f64 = arr[3].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let c: f64 = arr[4].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let v: f64 = arr[5].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);

                let t = DateTime::<Utc>::from_timestamp_millis(open_time_ms)
                    .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());

                if o == 0.0 && c == 0.0 { continue; }

                all_candles.push(Candle {
                    t,
                    o, h, l, c,
                    v: if v > 0.0 { Some(v) } else { None },
                    kind: None,
                });

                // Advance cursor past this candle for pagination.
                if close_time_ms + 1 > cursor_ms {
                    cursor_ms = close_time_ms + 1;
                }
            }

            if rows.len() < 1000 { break; } // Last page.

            // Rate limit: ~100ms between pages.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        if all_candles.is_empty() && errors.iter().all(|e| e.ticker != *ticker) {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: format!("binance returned no candles for '{symbol}'"),
            });
            continue;
        }

        all_candles.sort_by_key(|c| c.t);
        all_candles.dedup_by_key(|c| c.t);

        // Resample if needed.
        if needs_resample && !all_candles.is_empty() {
            let step = granularity.approx_duration();
            all_candles = resample_candles(&all_candles, start, step);
        }

        if !all_candles.is_empty() {
            let upstream = ticker
                .strip_prefix("BN:")
                .or_else(|| ticker.strip_prefix("BINANCE:"))
                .unwrap_or(ticker)
                .to_string();
            out.push(TickerSeries {
                ticker: ticker.clone(),
                candles: all_candles,
                source: Some("binance".to_string()),
                upstream_id: Some(upstream),
            });
        }

        // Rate limit between tickers.
        if tickers.len() > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    Ok((out, errors))
}

/// Returns true if the ticker should route to Binance.
pub fn is_binance_ticker(ticker: &str) -> bool {
    let t = ticker.trim().to_ascii_uppercase();
    t.starts_with("BN:") || t.starts_with("BINANCE:")
}
