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

    // FRED's Akamai CDN fingerprints TLS clients. reqwest (even with native-tls
    // feature) produces a different fingerprint than system curl — Akamai accepts
    // the handshake but silently ghosts the HTTP response. curl uses the real
    // macOS SecureTransport stack with a browser-compatible fingerprint.
    // Use curl as primary, NOT reqwest.
    let start_date = start.date_naive().format("%Y-%m-%d").to_string();
    let end_date = end.date_naive().format("%Y-%m-%d").to_string();
    let step = granularity.approx_duration();

    use futures::stream::{self as fstream, StreamExt as _};
    let results: Vec<std::result::Result<TickerSeries, TimeseriesError>> =
        fstream::iter(tickers.iter().map(|series_id| {
            let series_id = series_id.clone();
            let start_date = start_date.clone();
            let end_date = end_date.clone();
            async move {
                let url = format!(
                    "https://fred.stlouisfed.org/graph/fredgraph.csv?id={}&cosd={}&coed={}",
                    series_id, start_date, end_date
                );
                let output = tokio::process::Command::new("curl")
                    .args(["--silent", "--fail", "--max-time", "15", "--retry", "2", &url])
                    .output()
                    .await;
                let body = match output {
                    Ok(o) if o.status.success() => {
                        String::from_utf8_lossy(&o.stdout).into_owned()
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        return Err(TimeseriesError {
                            ticker: series_id.clone(),
                            stage: Some("fetch".to_string()),
                            message: format!(
                                "fred fetch failed (exit {}): {}",
                                o.status.code().unwrap_or(-1),
                                stderr.trim()
                            ),
                        });
                    }
                    Err(e) => {
                        return Err(TimeseriesError {
                            ticker: series_id.clone(),
                            stage: Some("fetch".to_string()),
                            message: format!("curl not found or failed to spawn: {e}"),
                        });
                    }
                };

                let mut candles = Vec::new();
                let mut parse_err: Option<TimeseriesError> = None;
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
                            parse_err = Some(TimeseriesError {
                                ticker: series_id.clone(),
                                stage: Some("parse".to_string()),
                                message: format!("fred invalid date '{date_raw}'"),
                            });
                            candles.clear();
                            break;
                        }
                    };
                    let t = match date.and_hms_opt(0, 0, 0) {
                        Some(ndt) => DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc),
                        None => {
                            parse_err = Some(TimeseriesError {
                                ticker: series_id.clone(),
                                stage: Some("parse".to_string()),
                                message: "fred invalid datetime".to_string(),
                            });
                            candles.clear();
                            break;
                        }
                    };
                    if t < start || t > end {
                        continue;
                    }

                    let v: f64 = match val_raw.parse() {
                        Ok(v) => v,
                        Err(_) => {
                            parse_err = Some(TimeseriesError {
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

                if let Some(err) = parse_err {
                    return Err(err);
                }

                candles.sort_by_key(|c| c.t);
                let candles = resample_candles(&candles, start, step);

                if candles.is_empty() {
                    return Err(TimeseriesError {
                        ticker: series_id.clone(),
                        stage: Some("parse".to_string()),
                        message: "fred returned no data points in the requested range".to_string(),
                    });
                }

                Ok(TickerSeries {
                    ticker: series_id.clone(),
                    candles,
                })
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(series) => out.push(series),
            Err(err) => errors.push(err),
        }
    }

    Ok((out, errors))
}

pub fn resample_candles(candles: &[Candle], _start: DateTime<Utc>, step: Duration) -> Vec<Candle> {
    let step_seconds = step.num_seconds().max(1);
    let mut out = Vec::new();

    let mut current_bucket: Option<i64> = None;
    let mut bucket: Vec<&Candle> = Vec::new();

    for candle in candles {
        // Absolute UTC bucketing: floor to clean clock boundaries regardless of
        // request start time.  This ensures that a 2h bucket is always
        // [00:00,02:00), [02:00,04:00), … not offset by the query window.
        let bucket_ts = candle.t.timestamp().div_euclid(step_seconds);

        match current_bucket {
            None => {
                current_bucket = Some(bucket_ts);
                bucket.push(candle);
            }
            Some(b) if b == bucket_ts => bucket.push(candle),
            Some(_) => {
                if let Some(agg) = aggregate_bucket(current_bucket.unwrap_or(0), step_seconds, &bucket) {
                    out.push(agg);
                }
                bucket.clear();
                current_bucket = Some(bucket_ts);
                bucket.push(candle);
            }
        }
    }

    if let Some(agg) = aggregate_bucket(current_bucket.unwrap_or(0), step_seconds, &bucket) {
        out.push(agg);
    }

    out
}

fn aggregate_bucket(bucket_ts: i64, step_seconds: i64, bucket: &[&Candle]) -> Option<Candle> {
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

    // Use the clean bucket boundary as the candle timestamp, not the first
    // raw candle time.  This prevents e.g. a 2h bucket showing 13:30 when
    // the bucket really represents [12:00, 14:00).
    let bucket_start_epoch = bucket_ts * step_seconds;
    let t = DateTime::<Utc>::from_timestamp(bucket_start_epoch, 0).unwrap_or(first.t);

    Some(Candle {
        t,
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
