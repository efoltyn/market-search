/// Sovereign bond yield provider (CNBC public chart bars).
///
/// Endpoint: https://ts-api.cnbc.com/harmony/app/bars/{SYM}/{BAR}/{start}/{end}/adjusted/UTC.json
///   start/end formatted YYYYMMDDHHMMSS (UTC because of the trailing tz segment).
///
/// Coverage: government benchmark yields, quoted in percent (o/h/l/c are yields,
/// not prices). US uses bare symbols (US2Y, US10Y, US30Y, US3M); every other
/// country is {CC}{tenor}-{CC} (FR10Y-FR, DE10Y-DE, JP30Y-JP, GB2Y-GB, ...).
/// No auth required.
///
/// Native bars: 1M, 5M, 30M, 1H, 1D, 1W, 1MO (15M and 4H are rejected upstream).
/// Depth: daily back to 2000. Intraday requests are silently truncated to roughly
/// the last ~100 days of the requested window, so intraday ranges are chunked.
/// Intraday history is patchy beyond ~1y (some years return nothing).
///
/// Timestamps: tradeTimeinMills is true UTC epoch ms. Daily/weekly/monthly bars
/// are stamped at midnight US/Eastern (04:00 or 05:00 UTC); they are floored to
/// 00:00 UTC of the same calendar date so they line up with Yahoo/FRED dailies.
/// Weekend quote-cache bars are dropped (see fetch loop). Weekly bars are stamped
/// Sunday and kept as-is. Euro-area daily closes are the ~15:30 UTC official cash
/// close; intraday bars keep trading until ~20:00 UTC, so the last hourly close
/// of a day can differ from that day's daily close.

const GOVT_BARS_BASE: &str = "https://ts-api.cnbc.com/harmony/app/bars";
const GOVT_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0 Safari/537.36";

/// Best native CNBC bar <= requested granularity: (bar code, seconds, chunk days).
/// Chunk days keeps each intraday request inside the upstream ~100-day truncation.
fn govt_best_bar(granularity: Span) -> (&'static str, i64, Option<i64>) {
    let requested = granularity.approx_duration().num_seconds().max(1);
    let candidates: &[(&str, i64, Option<i64>)] = &[
        ("1M", 60, Some(5)),
        ("5M", 300, Some(30)),
        ("30M", 1800, Some(60)),
        ("1H", 3600, Some(60)),
        ("1D", 86400, None),
        ("1W", 604800, None),
    ];
    let mut best = candidates[0];
    for &c in candidates {
        if c.1 <= requested && requested % c.1 == 0 {
            best = c;
        }
    }
    best
}

/// Translate a user ticker to a CNBC symbol.
/// Accepts GOVT:FR10Y, GOVT:FR10Y-FR, GOVT:US10Y, GOVT:us30y.
fn govt_symbol(ticker: &str) -> String {
    let t = ticker.trim();
    let raw = if t.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("GOVT:")) {
        &t[5..]
    } else {
        t
    };
    let upper = raw.trim().to_ascii_uppercase();
    if upper.contains('-') || upper.starts_with("US") || upper.len() < 3 {
        return upper;
    }
    match upper.get(..2) {
        Some(cc) if cc.chars().all(|c| c.is_ascii_alphabetic()) => format!("{upper}-{cc}"),
        _ => upper,
    }
}

pub(crate) async fn fetch_govt_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let client = &*crate::finance::shared_client::GENERAL;
    let (bar, bar_secs, chunk_days) = govt_best_bar(granularity);
    let daily_or_coarser = bar_secs >= 86400;
    let needs_resample = bar_secs != granularity.approx_duration().num_seconds();

    // CNBC anchors intraday bar boundaries to the request start (a 18:30 start
    // yields :30 hourly bars), so floor the start to a clean bar boundary.
    let fetch_start = DateTime::<Utc>::from_timestamp(
        start.timestamp().div_euclid(bar_secs.min(86400)) * bar_secs.min(86400),
        0,
    )
    .unwrap_or(start);

    // Build request windows: one for daily+, ~chunk_days slices for intraday.
    let mut windows = Vec::new();
    match chunk_days {
        Some(days) => {
            let step = Duration::days(days);
            let mut cursor = fetch_start;
            while cursor < end {
                let next = (cursor + step).min(end);
                windows.push((cursor, next));
                cursor = next;
            }
        }
        None => windows.push((fetch_start, end)),
    }

    let mut out = Vec::new();
    let mut errors = Vec::new();

    for ticker in tickers {
        let symbol = govt_symbol(ticker);
        let mut all_candles: Vec<Candle> = Vec::new();
        let mut failed = false;

        for (w_start, w_end) in &windows {
            let url = format!(
                "{}/{}/{}/{}/{}/adjusted/UTC.json",
                GOVT_BARS_BASE,
                symbol,
                bar,
                w_start.format("%Y%m%d%H%M%S"),
                w_end.format("%Y%m%d%H%M%S"),
            );
            let resp = match client
                .get(&url)
                .header(reqwest::header::USER_AGENT, GOVT_USER_AGENT)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    errors.push(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("fetch".to_string()),
                        message: format!("govt (cnbc) request failed: {e}"),
                    });
                    failed = true;
                    break;
                }
            };
            if !resp.status().is_success() {
                let status = resp.status();
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: format!("govt (cnbc) returned http {status} for '{symbol}'"),
                });
                failed = true;
                break;
            }
            let body: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    errors.push(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("parse".to_string()),
                        message: format!("govt (cnbc) json parse failed: {e}"),
                    });
                    failed = true;
                    break;
                }
            };
            if body.get("status").and_then(|s| s.as_str()) == Some("ERROR") {
                let msg = body
                    .get("statusMessage")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown error");
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: format!(
                        "govt (cnbc) rejected '{symbol}': {msg}. Symbols look like US10Y, FR10Y, DE2Y, JP30Y (country code + tenor)."
                    ),
                });
                failed = true;
                break;
            }
            let bars = body
                .pointer("/barData/priceBars")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for b in &bars {
                let ms = match b.get("tradeTimeinMills").and_then(|v| v.as_i64()) {
                    Some(ms) => ms,
                    None => continue,
                };
                let num = |k: &str| -> Option<f64> {
                    b.get(k).and_then(|v| match v {
                        serde_json::Value::String(s) => s.parse().ok(),
                        other => other.as_f64(),
                    })
                };
                let (Some(o), Some(h), Some(l), Some(c)) =
                    (num("open"), num("high"), num("low"), num("close"))
                else {
                    continue;
                };
                let Some(mut t) = DateTime::<Utc>::from_timestamp_millis(ms) else {
                    continue;
                };
                if daily_or_coarser {
                    // Midnight US/Eastern -> 00:00 UTC of the same date.
                    t = t.date_naive().and_hms_opt(0, 0, 0).map(|n| n.and_utc()).unwrap_or(t);
                }
                if t < start - Duration::days(1) || t > end {
                    continue;
                }
                // CNBC keeps contributing quote-cache ticks over the weekend for
                // non-US bonds (Saturday-dated daily bars with ranges). No session
                // trades then: drop weekend dailies, and intraday bars from Saturday
                // until Sunday 20:00 UTC (the Asia-Pacific Monday open is kept).
                use chrono::{Datelike, Timelike};
                let wd = t.weekday();
                if bar_secs <= 86400
                    && (wd == chrono::Weekday::Sat
                        || (wd == chrono::Weekday::Sun && (daily_or_coarser || t.hour() < 20)))
                {
                    continue;
                }
                all_candles.push(Candle { t, o, h, l, c, v: None, kind: None });
            }
            if windows.len() > 1 {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            }
        }

        if failed {
            continue;
        }
        if all_candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: format!(
                    "govt (cnbc) returned no {bar} bars for '{symbol}' in this window (intraday history is patchy beyond ~1y; try 1d or a shorter range)"
                ),
            });
            continue;
        }

        all_candles.sort_by_key(|c| c.t);
        all_candles.dedup_by_key(|c| c.t);
        if needs_resample {
            all_candles = resample_candles(&all_candles, start, granularity.approx_duration());
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles: all_candles,
            source: Some("cnbc".to_string()),
            upstream_id: Some(symbol),
        });
    }

    Ok((out, errors))
}

/// Returns true if the ticker should route to the sovereign-yield (CNBC) provider.
pub fn is_govt_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    ticker_has_prefix(t, "GOVT:")
}

#[cfg(test)]
mod govt_tests {
    use super::*;

    #[test]
    fn govt_symbol_mapping() {
        assert_eq!(govt_symbol("GOVT:FR10Y"), "FR10Y-FR");
        assert_eq!(govt_symbol("govt:de2y"), "DE2Y-DE");
        assert_eq!(govt_symbol("GOVT:FR10Y-FR"), "FR10Y-FR");
        assert_eq!(govt_symbol("GOVT:US10Y"), "US10Y");
        assert_eq!(govt_symbol("GOVT:US3M"), "US3M");
        assert_eq!(govt_symbol("GOVT:JP30Y"), "JP30Y-JP");
        assert!(is_govt_ticker("GOVT:IT10Y"));
        assert!(is_govt_ticker("govt:IT10Y"));
        assert!(!is_govt_ticker("GOVT:"));
        assert!(!is_govt_ticker("SPY"));
    }

    #[test]
    fn govt_bar_selection() {
        let h = Span { n: 1, unit: SpanUnit::Hour };
        assert_eq!(govt_best_bar(h).0, "1H");
        let m15 = Span { n: 15, unit: SpanUnit::Minute };
        assert_eq!(govt_best_bar(m15).0, "5M");
        let d = Span { n: 1, unit: SpanUnit::Day };
        assert_eq!(govt_best_bar(d).0, "1D");
        let h4 = Span { n: 4, unit: SpanUnit::Hour };
        assert_eq!(govt_best_bar(h4).0, "1H");
    }
}
