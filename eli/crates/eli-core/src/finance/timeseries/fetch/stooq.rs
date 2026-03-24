/// Stooq.com historical OHLCV provider.
///
/// Endpoint: https://stooq.com/q/d/l/?s={ticker}&d1={YYYYMMDD}&d2={YYYYMMDD}&i={d|w|m}
///
/// Coverage:
///   - US equities (aapl.us, spy.us) — back to 1990+
///   - US indices (^spx, ^dji) — back to 1950+
///   - Forex (eurusd, usdjpy) — decades
///   - Gold spot (xauusd, gc.c) — works
///   - PE ratios (aapl_pe.us) — unique: OHLC where the "price" is the PE ratio
///
/// Does NOT cover: commodity futures (cl.f returns empty), Treasury yields, DXY.
/// No auth required. Rate limit: be conservative (~1 req/sec).

const STOOQ_BASE: &str = "https://stooq.com/q/d/l/";

/// Translate a user-facing ticker into Stooq's format.
fn stooq_ticker(ticker: &str) -> String {
    let t = ticker.trim();

    // Already has Stooq suffix (.us, .uk, ^prefix, etc.)
    if t.contains('.') || t.starts_with('^') {
        return t.to_ascii_lowercase();
    }

    // STOOQ:AAPL_PE → aapl_pe.us, STOOQ:eurusd → eurusd (bare FX)
    if t.to_ascii_uppercase().starts_with("STOOQ:") {
        let raw = &t[6..];
        if raw.contains('.') || raw.starts_with('^') {
            return raw.to_ascii_lowercase();
        }
        let lower = raw.to_ascii_lowercase();
        // FX pairs are 6-letter bare tickers (eurusd, usdjpy, gbpusd, etc.)
        // Futures use .f suffix (cl.f, gc.f)
        // Treasury yields use .b suffix (10usy.b)
        // VIX is vi.f
        if is_stooq_fx_pair(&lower) {
            return lower;
        }
        return format!("{}.us", lower);
    }

    // Bare US ticker → append .us
    format!("{}.us", t.to_ascii_lowercase())
}

fn stooq_interval(granularity: Span) -> &'static str {
    let secs = granularity.approx_duration().num_seconds();
    if secs >= 2_592_000 {
        "m"
    } else if secs >= 604_800 {
        "w"
    } else {
        "d"
    }
}

pub(crate) async fn fetch_stooq_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let client = &*crate::finance::shared_client::GENERAL;
    let interval = stooq_interval(granularity);
    let d1 = start.format("%Y%m%d").to_string();
    let d2 = end.format("%Y%m%d").to_string();

    let mut out = Vec::new();
    let mut errors = Vec::new();

    for ticker in tickers {
        let stooq_sym = stooq_ticker(ticker);
        let url = format!(
            "{}?s={}&d1={}&d2={}&i={}",
            STOOQ_BASE, stooq_sym, d1, d2, interval
        );

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: format!("stooq request failed: {e}"),
                });
                continue;
            }
        };

        if !resp.status().is_success() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: format!("stooq returned http {}", resp.status()),
            });
            continue;
        }

        let body = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                errors.push(TimeseriesError {
                    ticker: ticker.clone(),
                    stage: Some("fetch".to_string()),
                    message: format!("stooq body read failed: {e}"),
                });
                continue;
            }
        };

        if body.trim() == "No data" || body.trim().is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: format!("stooq returned no data for '{stooq_sym}'"),
            });
            continue;
        }

        let mut candles = Vec::new();
        let mut lines = body.lines();
        // Skip header line: "Date,Open,High,Low,Close[,Volume]"
        let _header = lines.next();

        for line in lines {
            let fields: Vec<&str> = line.split(',').collect();
            if fields.len() < 5 {
                continue;
            }

            let date = match chrono::NaiveDate::parse_from_str(fields[0].trim(), "%Y-%m-%d") {
                Ok(d) => d,
                Err(_) => continue,
            };
            let t = match date.and_hms_opt(0, 0, 0) {
                Some(ndt) => DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc),
                None => continue,
            };
            if t < start || t > end {
                continue;
            }

            let o: f64 = fields[1].trim().parse().unwrap_or(0.0);
            let h: f64 = fields[2].trim().parse().unwrap_or(0.0);
            let l: f64 = fields[3].trim().parse().unwrap_or(0.0);
            let c: f64 = fields[4].trim().parse().unwrap_or(0.0);
            let v: Option<f64> = fields
                .get(5)
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|&v| v > 0.0);

            if o == 0.0 && h == 0.0 && l == 0.0 && c == 0.0 {
                continue;
            }

            candles.push(Candle { t, o, h, l, c, v });
        }

        if candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message: format!("stooq CSV parsed 0 valid candles for '{stooq_sym}'"),
            });
            continue;
        }

        candles.sort_by_key(|c| c.t);

        // Resample if needed (e.g. user requests 2d from daily data).
        let step = granularity.approx_duration();
        let base_step = Duration::days(if interval == "w" {
            7
        } else if interval == "m" {
            30
        } else {
            1
        });
        if step > base_step {
            candles = resample_candles(&candles, start, step);
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
        });

        // Conservative rate limit: 500ms between requests.
        if tickers.len() > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    Ok((out, errors))
}

/// Returns true if the bare ticker looks like a Stooq FX pair (6-char currency pair).
fn is_stooq_fx_pair(lower: &str) -> bool {
    const FX_PAIRS: &[&str] = &[
        "eurusd", "usdjpy", "gbpusd", "usdchf", "audusd", "usdcad",
        "nzdusd", "eurgbp", "eurjpy", "eurchf", "gbpjpy", "euraud",
        "eurcad", "audcad", "audjpy", "cadjpy", "chfjpy", "gbpaud",
        "gbpcad", "gbpchf", "nzdjpy", "audnzd",
    ];
    FX_PAIRS.contains(&lower)
}

/// Returns true if the ticker should route to Stooq.
pub fn is_stooq_ticker(ticker: &str) -> bool {
    let t = ticker.trim().to_ascii_uppercase();
    t.starts_with("STOOQ:")
}

/// Returns true if the ticker looks like a Stooq PE ratio ticker.
pub fn is_stooq_pe_ticker(ticker: &str) -> bool {
    let t = ticker.trim().to_ascii_lowercase();
    t.contains("_pe.") || (t.starts_with("stooq:") && t.contains("_pe"))
}
