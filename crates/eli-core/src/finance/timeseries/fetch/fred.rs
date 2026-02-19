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

