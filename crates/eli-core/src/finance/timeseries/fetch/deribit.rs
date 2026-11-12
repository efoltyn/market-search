/// Deribit public market-data provider (crypto options vol + perp funding).
///
/// Endpoint: GET https://www.deribit.com/api/v2/public/... (JSON-RPC over GET, no key,
/// US-accessible for public market data).
///
/// Ticker grammar: `DERIBIT:<METRIC>:<ARG>[:<TENOR>]` (case-insensitive)
///   DERIBIT:DVOL:BTC                     DVOL implied-vol index OHLC (vol points); also ETH
///   DERIBIT:FUNDING:BTC-PERPETUAL        8h-equivalent funding rate (decimal; `interest_8h`)
///   DERIBIT:FUNDING:BTC                  shorthand for BTC-PERPETUAL
///   DERIBIT:FUNDING_1H:BTC-PERPETUAL     funding accrued over each hour (`interest_1h`)
///   DERIBIT:FUNDING_APR:BTC              interest_8h annualized in percent (* 3 * 365 * 100)
///   DERIBIT:SKEW25:BTC[:30D]             25-delta risk reversal (call IV - put IV, vol points)
///                                        at a constant tenor (default 30 days): LIVE SNAPSHOT
///
/// DVOL: get_volatility_index_data, resolutions 1s/60s/1h/12h/1D. Each call returns the
/// LATEST <=1000 points of the window plus a `continuation` (an earlier end timestamp);
/// we page backwards until it is null or before `start`. Stamped at bucket open.
/// History back to 2021-03 (BTC/ETH).
///
/// FUNDING: get_funding_rate_history, hourly rows, <=744 per call, paged forward in 30-day
/// windows. Rows are stamped at the END of the hour they cover (the time the rate was
/// realized), so there is no look-ahead.
///
/// SKEW25: computed from ONE get_book_summary_by_currency(kind=option) call. Deribit's
/// book summary carries mark_iv, strike and per-expiry underlying (future) price but no
/// greeks, so delta is recomputed with Black-76 (r=0) from mark_iv. For each expiry the
/// 25-delta call and put IVs are linearly interpolated in delta; the RR is then linearly
/// interpolated in time to the target tenor between the bracketing expiries (expiries
/// under 1 day are skipped). There is no historical book summary, so SKEW25 is a single
/// point at fetch time and errors when `end` is more than an hour in the past.
///
/// Rate limit: public non-matching-engine requests are credit-limited (~20 req/s
/// sustained); 429 / 10028 "too_many_requests" get backoff.

const DERIBIT_BASE: &str = "https://www.deribit.com/api/v2/public";
const DERIBIT_USER_AGENT: &str = "market-search/0.2 (+https://eliterminal.com)";

#[derive(Clone, Debug, PartialEq)]
enum DeribitQuery {
    Dvol { currency: String },
    Funding { instrument: String, field: DeribitFundingField },
    Skew25 { currency: String, tenor_days: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeribitFundingField {
    Interest8h,
    Interest1h,
    Apr,
}

/// Returns true if the ticker should route to the Deribit provider.
pub fn is_deribit_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    ticker_has_prefix(t, "DERIBIT:")
}

fn deribit_parse_ticker(ticker: &str) -> Option<DeribitQuery> {
    let t = ticker.trim();
    if !is_deribit_ticker(t) {
        return None;
    }
    let parts: Vec<String> = t[8..].split(':').map(|p| p.trim().to_ascii_uppercase()).collect();
    let metric = parts.first()?.as_str();
    let arg = parts.get(1).filter(|a| !a.is_empty())?.clone();
    let perp = |a: String| if a.contains('-') { a } else { format!("{a}-PERPETUAL") };
    match metric {
        "DVOL" if parts.len() == 2 => Some(DeribitQuery::Dvol { currency: arg }),
        "FUNDING" | "FUNDING_8H" if parts.len() == 2 => Some(DeribitQuery::Funding {
            instrument: perp(arg),
            field: DeribitFundingField::Interest8h,
        }),
        "FUNDING_1H" if parts.len() == 2 => Some(DeribitQuery::Funding {
            instrument: perp(arg),
            field: DeribitFundingField::Interest1h,
        }),
        "FUNDING_APR" if parts.len() == 2 => Some(DeribitQuery::Funding {
            instrument: perp(arg),
            field: DeribitFundingField::Apr,
        }),
        "SKEW25" | "RR25" => {
            let tenor_days = match parts.get(2) {
                None => 30.0,
                Some(s) => s.trim_end_matches('D').parse::<f64>().ok().filter(|d| *d > 0.0)?,
            };
            if parts.len() > 3 {
                return None;
            }
            Some(DeribitQuery::Skew25 { currency: arg, tenor_days })
        }
        _ => None,
    }
}

/// GET with up to 3 attempts; backs off on 429/5xx/too_many_requests. Returns `result`.
async fn deribit_get(path_and_query: &str) -> std::result::Result<serde_json::Value, String> {
    let client = &*crate::finance::shared_client::GENERAL;
    let url = format!("{DERIBIT_BASE}/{path_and_query}");
    let mut last_err = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(600 * attempt * attempt)).await;
        }
        let resp = match client
            .get(&url)
            .header(reqwest::header::USER_AGENT, DERIBIT_USER_AGENT)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("request failed: {e}");
                continue;
            }
        };
        let status = resp.status();
        if status.as_u16() == 429 || status.is_server_error() {
            last_err = format!("http {status}");
            continue;
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Err(format!("http {status}, json parse failed: {e}")),
        };
        if let Some(err) = body.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
            if code == 10028 {
                last_err = format!("rate limited ({msg})");
                continue;
            }
            return Err(format!("deribit error {code}: {msg}"));
        }
        return body.get("result").cloned().ok_or_else(|| "missing result".to_string());
    }
    Err(last_err)
}

/// Native DVOL resolution <= requested: (api value, seconds).
fn deribit_dvol_resolution(granularity: Span) -> (&'static str, i64) {
    let requested = granularity.approx_duration().num_seconds().max(1);
    let candidates: &[(&str, i64)] = &[("60", 60), ("3600", 3600), ("43200", 43200), ("1D", 86400)];
    let mut best = candidates[0];
    for &c in candidates {
        if c.1 <= requested && requested % c.1 == 0 {
            best = c;
        }
    }
    best
}

/// Parse a DVOL `result` into candles and the continuation (ms) if any.
fn deribit_parse_dvol(result: &serde_json::Value) -> (Vec<Candle>, Option<i64>) {
    let mut out = Vec::new();
    for row in result.get("data").and_then(|d| d.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
        let Some(r) = row.as_array() else { continue };
        let f = |i: usize| r.get(i).and_then(|v| v.as_f64());
        let (Some(ms), Some(o), Some(h), Some(l), Some(c)) =
            (r.first().and_then(|v| v.as_i64()), f(1), f(2), f(3), f(4))
        else {
            continue;
        };
        let Some(t) = DateTime::<Utc>::from_timestamp_millis(ms) else { continue };
        out.push(Candle { t, o, h, l, c, v: None, kind: None });
    }
    let cont = result.get("continuation").and_then(|v| v.as_i64());
    (out, cont)
}

fn deribit_parse_funding(result: &serde_json::Value, field: DeribitFundingField) -> Vec<Candle> {
    let mut out = Vec::new();
    for row in result.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let Some(ms) = row.get("timestamp").and_then(|v| v.as_i64()) else { continue };
        let v = match field {
            DeribitFundingField::Interest8h => row.get("interest_8h").and_then(|v| v.as_f64()),
            DeribitFundingField::Interest1h => row.get("interest_1h").and_then(|v| v.as_f64()),
            DeribitFundingField::Apr => row
                .get("interest_8h")
                .and_then(|v| v.as_f64())
                .map(|r| r * 3.0 * 365.0 * 100.0),
        };
        let Some(v) = v else { continue };
        let Some(t) = DateTime::<Utc>::from_timestamp_millis(ms) else { continue };
        out.push(Candle { t, o: v, h: v, l: v, c: v, v: None, kind: None });
    }
    out
}

/// Standard normal CDF (Abramowitz-Stegun 7.1.26 erf, |err| < 1.5e-7).
fn deribit_norm_cdf(x: f64) -> f64 {
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * z);
    let poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    let erf = 1.0 - poly * (-z * z).exp();
    if x >= 0.0 { 0.5 * (1.0 + erf) } else { 0.5 * (1.0 - erf) }
}

/// Parse `BTC-27NOV26-80000-C` -> (expiry 08:00 UTC, strike, is_call).
fn deribit_parse_option_name(name: &str) -> Option<(DateTime<Utc>, f64, bool)> {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    let date = NaiveDate::parse_from_str(parts[1], "%d%b%y").ok()?;
    let expiry = date.and_hms_opt(8, 0, 0)?.and_utc();
    let strike: f64 = parts[2].replace('d', ".").parse().ok()?;
    let is_call = match parts[3] {
        "C" => true,
        "P" => false,
        _ => return None,
    };
    Some((expiry, strike, is_call))
}

/// Interpolate IV at |delta| = 0.25 from (|delta|, iv) points (any order).
fn deribit_iv_at_25d(mut pts: Vec<(f64, f64)>) -> Option<f64> {
    pts.retain(|(d, iv)| d.is_finite() && iv.is_finite());
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for w in pts.windows(2) {
        let ((d0, v0), (d1, v1)) = (w[0], w[1]);
        if d0 <= 0.25 && d1 >= 0.25 {
            if (d1 - d0).abs() < 1e-12 {
                return Some(v0);
            }
            return Some(v0 + (v1 - v0) * (0.25 - d0) / (d1 - d0));
        }
    }
    None
}

/// 25-delta risk reversal at `tenor_days` from a book-summary `result`, as of `now`.
/// Returns (rr vol points, description of the expiries used).
fn deribit_skew25_from_summary(
    result: &serde_json::Value,
    now: DateTime<Utc>,
    tenor_days: f64,
) -> Option<(f64, String)> {
    // expiry -> (calls (|delta|, iv), puts (|delta|, iv))
    let mut by_expiry: BTreeMap<DateTime<Utc>, (Vec<(f64, f64)>, Vec<(f64, f64)>)> = BTreeMap::new();
    for row in result.as_array()? {
        let Some(name) = row.get("instrument_name").and_then(|v| v.as_str()) else { continue };
        let Some((expiry, k, is_call)) = deribit_parse_option_name(name) else { continue };
        let (Some(iv), Some(f)) = (
            row.get("mark_iv").and_then(|v| v.as_f64()),
            row.get("underlying_price").and_then(|v| v.as_f64()),
        ) else {
            continue;
        };
        let t_years = (expiry - now).num_seconds() as f64 / (365.0 * 86400.0);
        if t_years < 1.0 / 365.0 || iv <= 0.0 || f <= 0.0 || k <= 0.0 {
            continue;
        }
        let sigma = iv / 100.0;
        let d1 = ((f / k).ln() + 0.5 * sigma * sigma * t_years) / (sigma * t_years.sqrt());
        let entry = by_expiry.entry(expiry).or_default();
        if is_call {
            entry.0.push((deribit_norm_cdf(d1), iv));
        } else {
            entry.1.push((1.0 - deribit_norm_cdf(d1), iv));
        }
    }
    let curve: Vec<(f64, f64, DateTime<Utc>)> = by_expiry
        .into_iter()
        .filter_map(|(exp, (calls, puts))| {
            let rr = deribit_iv_at_25d(calls)? - deribit_iv_at_25d(puts)?;
            Some(((exp - now).num_seconds() as f64 / 86400.0, rr, exp))
        })
        .collect();
    if curve.is_empty() {
        return None;
    }
    let fmt = |e: &DateTime<Utc>| e.format("%Y-%m-%d").to_string();
    if let Some(i) = curve.windows(2).position(|w| w[0].0 <= tenor_days && w[1].0 >= tenor_days) {
        let (t0, r0, e0) = curve[i];
        let (t1, r1, e1) = curve[i + 1];
        let rr = r0 + (r1 - r0) * (tenor_days - t0) / (t1 - t0);
        return Some((rr, format!("interpolated {tenor_days}d between {} ({r0:.2}) and {} ({r1:.2})", fmt(&e0), fmt(&e1))));
    }
    // Outside the expiry range: use the nearest expiry.
    let nearest = curve
        .iter()
        .min_by(|a, b| (a.0 - tenor_days).abs().partial_cmp(&(b.0 - tenor_days).abs()).unwrap_or(std::cmp::Ordering::Equal))?;
    Some((nearest.1, format!("nearest expiry {} ({:.1}d), no bracketing expiry", fmt(&nearest.2), nearest.0)))
}

async fn deribit_fetch_dvol(
    currency: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    resolution: &str,
) -> std::result::Result<Vec<Candle>, String> {
    let mut out = Vec::new();
    let start_ms = start.timestamp_millis();
    let mut end_ms = end.timestamp_millis();
    for _page in 0..200 {
        let q = format!(
            "get_volatility_index_data?currency={currency}&start_timestamp={start_ms}&end_timestamp={end_ms}&resolution={resolution}"
        );
        let result = deribit_get(&q).await?;
        let (candles, cont) = deribit_parse_dvol(&result);
        out.extend(candles);
        match cont {
            Some(c) if c > start_ms && c < end_ms => end_ms = c,
            _ => break,
        }
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    }
    Ok(out)
}

async fn deribit_fetch_funding(
    instrument: &str,
    field: DeribitFundingField,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> std::result::Result<Vec<Candle>, String> {
    let mut out = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let next = (cursor + Duration::days(30)).min(end);
        let q = format!(
            "get_funding_rate_history?instrument_name={instrument}&start_timestamp={}&end_timestamp={}",
            cursor.timestamp_millis(),
            next.timestamp_millis()
        );
        let result = deribit_get(&q).await?;
        out.extend(deribit_parse_funding(&result, field));
        cursor = next;
        if cursor < end {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        }
    }
    Ok(out)
}

/// Fetch Deribit series. Same signature/return conventions as `fetch_govt_series`.
pub(crate) async fn fetch_deribit_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let step = granularity.approx_duration();
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let err = |ticker: &str, stage: &str, message: String| TimeseriesError {
        ticker: ticker.to_string(),
        stage: Some(stage.to_string()),
        message,
    };

    for ticker in tickers {
        let Some(query) = deribit_parse_ticker(ticker) else {
            errors.push(err(
                ticker,
                "parse",
                "deribit ticker must look like DERIBIT:DVOL:BTC, DERIBIT:FUNDING:BTC-PERPETUAL, DERIBIT:FUNDING_APR:ETH, DERIBIT:SKEW25:BTC[:30D]".to_string(),
            ));
            continue;
        };

        let (fetched, native_secs, upstream) = match &query {
            DeribitQuery::Dvol { currency } => {
                let (res, secs) = deribit_dvol_resolution(granularity);
                (deribit_fetch_dvol(currency, start, end, res).await, secs, format!("DVOL:{currency}"))
            }
            DeribitQuery::Funding { instrument, field } => (
                deribit_fetch_funding(instrument, *field, start, end).await,
                3600,
                instrument.clone(),
            ),
            DeribitQuery::Skew25 { currency, tenor_days } => {
                let now = Utc::now();
                if end < now - Duration::hours(1) {
                    errors.push(err(
                        ticker,
                        "fetch",
                        "deribit SKEW25 is computed from the live option book (no history upstream); it cannot be served for a past as_of.".to_string(),
                    ));
                    continue;
                }
                let r = deribit_get(&format!("get_book_summary_by_currency?currency={currency}&kind=option"))
                    .await
                    .and_then(|res| {
                        deribit_skew25_from_summary(&res, now, *tenor_days)
                            .ok_or_else(|| format!("could not build a 25-delta smile for {currency}"))
                    });
                match r {
                    Ok((rr, how)) => {
                        errors.push(err(
                            ticker,
                            "data_quality",
                            format!("deribit SKEW25 is a live snapshot (single point, Black-76 deltas from mark_iv): {how}"),
                        ));
                        let c = Candle { t: now, o: rr, h: rr, l: rr, c: rr, v: None, kind: Some("point".to_string()) };
                        (Ok(vec![c]), step.num_seconds(), format!("SKEW25:{currency}:{tenor_days}D"))
                    }
                    Err(e) => (Err(e), 0, String::new()),
                }
            }
        };

        let mut candles = match fetched {
            Ok(c) => c,
            Err(e) => {
                errors.push(err(ticker, "fetch", format!("deribit: {e}")));
                continue;
            }
        };
        if !matches!(query, DeribitQuery::Skew25 { .. }) {
            candles.retain(|c| c.t >= start - step && c.t <= end);
        }
        if candles.is_empty() {
            errors.push(err(ticker, "fetch", format!("deribit returned no data for '{upstream}' in this window")));
            continue;
        }
        candles.sort_by_key(|c| c.t);
        candles.dedup_by_key(|c| c.t);
        if native_secs < step.num_seconds() {
            candles = resample_candles(&candles, start, step);
        }
        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some("deribit".to_string()),
            upstream_id: Some(upstream),
        });
    }

    Ok((out, errors))
}

#[cfg(test)]
mod deribit_tests {
    use super::*;

    #[test]
    fn deribit_ticker_grammar() {
        assert!(is_deribit_ticker("deribit:dvol:btc"));
        assert!(!is_deribit_ticker("DERIBIT:"));
        assert_eq!(deribit_parse_ticker("DERIBIT:DVOL:btc"), Some(DeribitQuery::Dvol { currency: "BTC".into() }));
        assert_eq!(
            deribit_parse_ticker("DERIBIT:FUNDING:BTC"),
            Some(DeribitQuery::Funding { instrument: "BTC-PERPETUAL".into(), field: DeribitFundingField::Interest8h })
        );
        assert_eq!(
            deribit_parse_ticker("DERIBIT:FUNDING_1H:ETH-PERPETUAL"),
            Some(DeribitQuery::Funding { instrument: "ETH-PERPETUAL".into(), field: DeribitFundingField::Interest1h })
        );
        assert_eq!(
            deribit_parse_ticker("DERIBIT:SKEW25:BTC"),
            Some(DeribitQuery::Skew25 { currency: "BTC".into(), tenor_days: 30.0 })
        );
        assert_eq!(
            deribit_parse_ticker("DERIBIT:SKEW25:ETH:7D"),
            Some(DeribitQuery::Skew25 { currency: "ETH".into(), tenor_days: 7.0 })
        );
        assert_eq!(deribit_parse_ticker("DERIBIT:FOO:BTC"), None);
        assert_eq!(deribit_parse_ticker("DERIBIT:DVOL"), None);
    }

    #[test]
    fn deribit_parse_fixtures() {
        // Real responses captured 2026-10-30.
        let dvol: serde_json::Value = serde_json::from_str(
            r#"{"data":[[1793361600000,39.03,39.38,38.54,39.38],[1793365200000,39.38,41.35,39.38,41.2]],"continuation":1789772400000}"#,
        )
        .unwrap();
        let (c, cont) = deribit_parse_dvol(&dvol);
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].h, 41.35);
        assert_eq!(c[1].c, 41.2);
        assert_eq!(cont, Some(1789772400000));

        let funding: serde_json::Value = serde_json::from_str(
            r#"[{"timestamp":1793365200000,"index_price":77336.21,"interest_8h":5.4382246375860864e-5,"interest_1h":0.0,"prev_index_price":77258.8},
                {"timestamp":1793368800000,"index_price":77021.69,"interest_8h":3.816222781657576e-5,"interest_1h":9.858779668614241e-7,"prev_index_price":77336.21}]"#,
        )
        .unwrap();
        let f8 = deribit_parse_funding(&funding, DeribitFundingField::Interest8h);
        assert_eq!(f8.len(), 2);
        assert!((f8[0].c - 5.4382246375860864e-5).abs() < 1e-15);
        let f1 = deribit_parse_funding(&funding, DeribitFundingField::Interest1h);
        assert!((f1[1].c - 9.858779668614241e-7).abs() < 1e-15);
        let apr = deribit_parse_funding(&funding, DeribitFundingField::Apr);
        assert!((apr[0].c - 5.4382246375860864e-5 * 109500.0).abs() < 1e-9);
    }

    #[test]
    fn deribit_option_name_and_cdf() {
        let (exp, k, call) = deribit_parse_option_name("BTC-1NOV26-76500-C").unwrap();
        assert_eq!(exp.to_rfc3339(), "2026-11-01T08:00:00+00:00");
        assert_eq!(k, 76500.0);
        assert!(call);
        let (_, k2, call2) = deribit_parse_option_name("XRP_USDC-27NOV26-0d5-P").unwrap();
        assert_eq!(k2, 0.5);
        assert!(!call2);
        assert!(deribit_parse_option_name("BTC-PERPETUAL").is_none());
        assert!((deribit_norm_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((deribit_norm_cdf(1.0) - 0.841344746).abs() < 1e-6);
        assert!((deribit_norm_cdf(-1.0) - 0.158655254).abs() < 1e-6);
    }

    #[test]
    fn deribit_skew_synthetic_smile() {
        // Two expiries, put wing richer than call wing by a fixed amount.
        let now = DateTime::parse_from_rfc3339("2026-10-30T08:00:00Z").unwrap().with_timezone(&Utc);
        let mut rows = Vec::new();
        for (exp, put_rich) in [("20NOV26", 4.0), ("25DEC26", 2.0)] {
            for k in (50_000..=110_000).step_by(2_500) {
                let base = 40.0;
                let skew = if (k as f64) < 77_000.0 { put_rich } else { 0.0 };
                for cp in ["C", "P"] {
                    rows.push(serde_json::json!({
                        "instrument_name": format!("BTC-{exp}-{k}-{cp}"),
                        "mark_iv": base + skew,
                        "underlying_price": 77_000.0,
                    }));
                }
            }
        }
        let (rr, how) = deribit_skew25_from_summary(&serde_json::Value::Array(rows), now, 30.0).unwrap();
        // 21d expiry RR = -4, 56d expiry RR = -2 -> 30d interpolates to about -3.5.
        assert!(rr < -3.0 && rr > -4.0, "rr={rr} {how}");
    }

    #[tokio::test]
    #[ignore]
    async fn deribit_live_btc() {
        let end = Utc::now();
        let start = end - Duration::hours(48);
        let tickers: Vec<String> = ["DERIBIT:DVOL:BTC", "DERIBIT:FUNDING:BTC-PERPETUAL", "DERIBIT:FUNDING_APR:BTC", "DERIBIT:SKEW25:BTC", "DERIBIT:SKEW25:BTC:7D"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (series, errors) = fetch_deribit_series(&tickers, start, end, Span { n: 1, unit: SpanUnit::Hour })
            .await
            .unwrap();
        for e in &errors {
            println!("ERR {} [{:?}] {}", e.ticker, e.stage, e.message);
        }
        for s in &series {
            println!("== {} ({} candles)", s.ticker, s.candles.len());
            for c in &s.candles {
                println!("{} o={} h={} l={} c={}", c.t.format("%Y-%m-%dT%H:%MZ"), c.o, c.h, c.l, c.c);
            }
        }
        assert_eq!(series.len(), 5);
        assert!(series[0].candles.len() >= 40);
    }
}
