/// Hyperliquid perpetuals provider (leverage layer: funding, premium, price, OI).
///
/// Endpoint: POST https://api.hyperliquid.xyz/info (JSON body, no key, US-accessible).
///
/// Ticker grammar: `HL:<COIN>[:<METRIC>]` (prefix case-insensitive, metric defaults to PX)
///   HL:BTC:FUNDING      hourly funding rate, decimal per hour (0.0000125 = 0.00125%/h)
///   HL:BTC:FUNDING_APR  same, annualized in percent (rate * 24 * 365 * 100)
///   HL:BTC:PREMIUM      hourly premium sample (perp mark vs oracle, decimal) from funding history
///   HL:BTC:PX           perp OHLCV candles (v = base-coin volume)
///   HL:BTC:OI           open interest in coins: CURRENT VALUE ONLY (one point candle at now)
///   HL:BTC:OI_USD       open interest * mark price in USD: CURRENT VALUE ONLY
/// Coins are upper-cased except a leading `k` before an upper-case letter (kPEPE, kBONK)
/// and HIP-3 builder dex coins (`HL:xyz:TSLA:PX` -> coin `xyz:TSLA`).
///
/// Timestamps: funding/premium rows carry the exact UTC settlement time (top of each
/// hour, a few ms late; floored to the second). Candles are stamped at bar OPEN.
///
/// Limits: fundingHistory returns <=500 rows per call (paginated forward here);
/// history goes back to the coin's listing (BTC: 2023). candleSnapshot serves only
/// the most recent ~5000 bars per interval (1h ~ 7 months, 1d ~ 13 years).
/// metaAndAssetCtxs is a live snapshot: Hyperliquid exposes NO open-interest history,
/// so OI must be accumulated by polling; asking for OI with an `end` more than an
/// hour in the past is an error rather than a look-ahead leak.
/// Rate limit: 1200 weight/min per IP (info calls are 2-20 weight); 429s get backoff.

const HL_INFO_URL: &str = "https://api.hyperliquid.xyz/info";
const HL_USER_AGENT: &str = "market-search/0.2 (+https://eliterminal.com)";
const HL_FUNDING_PAGE: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HlMetric {
    Funding,
    FundingApr,
    Premium,
    Px,
    Oi,
    OiUsd,
}

/// Returns true if the ticker should route to the Hyperliquid provider.
pub fn is_hyperliquid_ticker(ticker: &str) -> bool {
    let t = ticker.trim();
    ticker_has_prefix(t, "HL:")
}

fn hl_norm_coin(raw: &str) -> String {
    let raw = raw.trim();
    if let Some((dex, asset)) = raw.split_once(':') {
        return format!("{}:{}", dex.to_ascii_lowercase(), asset.to_ascii_uppercase());
    }
    let mut chars = raw.chars();
    if let (Some('k'), Some(next)) = (chars.next(), chars.next()) {
        if next.is_ascii_uppercase() {
            return format!("k{}", raw[1..].to_ascii_uppercase());
        }
    }
    raw.to_ascii_uppercase()
}

/// Parse `HL:<COIN>[:<METRIC>]` into (coin, metric).
fn hl_parse_ticker(ticker: &str) -> Option<(String, HlMetric)> {
    let t = ticker.trim();
    if !is_hyperliquid_ticker(t) {
        return None;
    }
    let body = &t[3..];
    let (coin_part, metric) = match body.rsplit_once(':') {
        Some((coin, m)) => {
            let metric = match m.trim().to_ascii_uppercase().as_str() {
                "FUNDING" | "FUND" => Some(HlMetric::Funding),
                "FUNDING_APR" | "APR" => Some(HlMetric::FundingApr),
                "PREMIUM" | "PREM" => Some(HlMetric::Premium),
                "PX" | "PRICE" | "OHLC" => Some(HlMetric::Px),
                "OI" => Some(HlMetric::Oi),
                "OI_USD" => Some(HlMetric::OiUsd),
                _ => None,
            };
            match metric {
                Some(m) => (coin, m),
                // `HL:xyz:TSLA` (dex coin, no metric) -> PX on the whole body.
                None => (body, HlMetric::Px),
            }
        }
        None => (body, HlMetric::Px),
    };
    if coin_part.trim().is_empty() {
        return None;
    }
    Some((hl_norm_coin(coin_part), metric))
}

/// Best native HL interval <= requested granularity that divides it evenly.
fn hl_best_interval(granularity: Span) -> (&'static str, i64) {
    let requested = granularity.approx_duration().num_seconds().max(1);
    let candidates: &[(&str, i64)] = &[
        ("1m", 60),
        ("3m", 180),
        ("5m", 300),
        ("15m", 900),
        ("30m", 1800),
        ("1h", 3600),
        ("2h", 7200),
        ("4h", 14400),
        ("8h", 28800),
        ("12h", 43200),
        ("1d", 86400),
        ("1w", 604800),
    ];
    let mut best = candidates[0];
    for &c in candidates {
        if c.1 <= requested && requested % c.1 == 0 {
            best = c;
        }
    }
    best
}

/// POST to /info with up to 3 attempts; backs off on 429/5xx and transport errors.
async fn hl_post(body: &serde_json::Value) -> std::result::Result<serde_json::Value, String> {
    let client = &*crate::finance::shared_client::GENERAL;
    let mut last_err = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(600 * attempt * attempt)).await;
        }
        let resp = match client
            .post(HL_INFO_URL)
            .header(reqwest::header::USER_AGENT, HL_USER_AGENT)
            .json(body)
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
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("http {status}: {}", text.chars().take(200).collect::<String>()));
        }
        return resp.json().await.map_err(|e| format!("json parse failed: {e}"));
    }
    Err(last_err)
}

/// Hyperliquid answers http 500 to a candle or funding request for a name it
/// does not list, the same as for a fault on its side. Say what that most
/// likely means instead of passing on the bare status.
fn hl_explain(coin: &str, e: String) -> String {
    if e.starts_with("http 500") {
        format!(
            "{e}. Hyperliquid answers this for a name it does not list: check '{coin}' (names are case-sensitive: HL:BTC, HL:kPEPE, HL:xyz:GOLD)."
        )
    } else {
        e
    }
}

fn hl_num(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|v| match v {
        serde_json::Value::String(s) => s.parse().ok(),
        other => other.as_f64(),
    })
}

/// One fundingHistory row: (settlement time floored to the second, rate/hour, premium).
fn hl_parse_funding_rows(body: &serde_json::Value) -> Vec<(DateTime<Utc>, f64, f64)> {
    let mut out = Vec::new();
    for row in body.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let Some(ms) = row.get("time").and_then(|v| v.as_i64()) else { continue };
        let (Some(rate), Some(prem)) = (hl_num(row.get("fundingRate")), hl_num(row.get("premium")))
        else {
            continue;
        };
        let Some(t) = DateTime::<Utc>::from_timestamp(ms.div_euclid(1000), 0) else { continue };
        out.push((t, rate, prem));
    }
    out
}

fn hl_parse_candles(body: &serde_json::Value) -> Vec<Candle> {
    let mut out = Vec::new();
    for b in body.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let Some(ms) = b.get("t").and_then(|v| v.as_i64()) else { continue };
        let (Some(o), Some(h), Some(l), Some(c)) =
            (hl_num(b.get("o")), hl_num(b.get("h")), hl_num(b.get("l")), hl_num(b.get("c")))
        else {
            continue;
        };
        let Some(t) = DateTime::<Utc>::from_timestamp_millis(ms) else { continue };
        out.push(Candle { t, o, h, l, c, v: hl_num(b.get("v")), kind: None });
    }
    out
}

/// From a metaAndAssetCtxs response, return (openInterest coins, markPx) for `coin`.
fn hl_parse_asset_ctx(body: &serde_json::Value, coin: &str) -> Option<(f64, f64)> {
    let universe = body.pointer("/0/universe")?.as_array()?;
    let ctxs = body.get(1)?.as_array()?;
    let idx = universe
        .iter()
        .position(|u| u.get("name").and_then(|n| n.as_str()) == Some(coin))?;
    let ctx = ctxs.get(idx)?;
    Some((hl_num(ctx.get("openInterest"))?, hl_num(ctx.get("markPx"))?))
}

fn hl_point(t: DateTime<Utc>, v: f64) -> Candle {
    Candle { t, o: v, h: v, l: v, c: v, v: None, kind: Some("point".to_string()) }
}

async fn hl_fetch_funding(
    coin: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> std::result::Result<Vec<(DateTime<Utc>, f64, f64)>, String> {
    let mut rows = Vec::new();
    let mut cursor = start.timestamp_millis();
    let end_ms = end.timestamp_millis();
    for _page in 0..400 {
        let body = serde_json::json!({
            "type": "fundingHistory", "coin": coin, "startTime": cursor, "endTime": end_ms
        });
        let resp = hl_post(&body).await?;
        let page = hl_parse_funding_rows(&resp);
        let raw_len = resp.as_array().map(|a| a.len()).unwrap_or(0);
        let last_ms = resp
            .as_array()
            .and_then(|a| a.last())
            .and_then(|r| r.get("time"))
            .and_then(|v| v.as_i64());
        rows.extend(page);
        match last_ms {
            Some(ms) if raw_len >= HL_FUNDING_PAGE && ms < end_ms => cursor = ms + 1,
            _ => break,
        }
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    }
    Ok(rows)
}

async fn hl_fetch_px(
    coin: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    interval: &str,
    interval_secs: i64,
) -> std::result::Result<Vec<Candle>, String> {
    let mut out: Vec<Candle> = Vec::new();
    let floor = start.timestamp().div_euclid(interval_secs) * interval_secs * 1000;
    let mut cursor = floor;
    let end_ms = end.timestamp_millis();
    for _page in 0..20 {
        let body = serde_json::json!({
            "type": "candleSnapshot",
            "req": {"coin": coin, "interval": interval, "startTime": cursor, "endTime": end_ms}
        });
        let resp = hl_post(&body).await?;
        let page = hl_parse_candles(&resp);
        let Some(last) = page.last().map(|c| c.t.timestamp_millis()) else { break };
        let n = page.len();
        out.extend(page);
        if n < 5000 || last >= end_ms {
            break;
        }
        cursor = last + 1;
    }
    Ok(out)
}

/// Fetch Hyperliquid series. Same signature/return conventions as `fetch_govt_series`.
pub(crate) async fn fetch_hyperliquid_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let step = granularity.approx_duration();
    let mut out = Vec::new();
    let mut errors = Vec::new();
    // One funding-history pull serves FUNDING / FUNDING_APR / PREMIUM for a coin.
    let mut funding_cache: HashMap<String, Vec<(DateTime<Utc>, f64, f64)>> = HashMap::new();
    let mut ctx_cache: Option<serde_json::Value> = None;

    let err = |ticker: &str, stage: &str, message: String| TimeseriesError {
        ticker: ticker.to_string(),
        stage: Some(stage.to_string()),
        message,
    };

    for ticker in tickers {
        let Some((coin, metric)) = hl_parse_ticker(ticker) else {
            errors.push(err(
                ticker,
                "parse",
                "hyperliquid ticker must look like HL:BTC:FUNDING, HL:ETH:PREMIUM, HL:SOL:PX, HL:BTC:OI".to_string(),
            ));
            continue;
        };

        let mut candles: Vec<Candle> = match metric {
            HlMetric::Funding | HlMetric::FundingApr | HlMetric::Premium => {
                if !funding_cache.contains_key(&coin) {
                    match hl_fetch_funding(&coin, start, end).await {
                        Ok(rows) => {
                            funding_cache.insert(coin.clone(), rows);
                        }
                        Err(e) => {
                            let e = hl_explain(&coin, e);
                            errors.push(err(ticker, "fetch", format!("hyperliquid fundingHistory '{coin}': {e}")));
                            continue;
                        }
                    }
                }
                let rows = funding_cache.get(&coin).map(|r| r.as_slice()).unwrap_or(&[]);
                rows.iter()
                    .map(|&(t, rate, prem)| {
                        let v = match metric {
                            HlMetric::Funding => rate,
                            HlMetric::FundingApr => rate * 24.0 * 365.0 * 100.0,
                            _ => prem,
                        };
                        Candle { t, o: v, h: v, l: v, c: v, v: None, kind: None }
                    })
                    .collect()
            }
            HlMetric::Px => {
                let (interval, secs) = hl_best_interval(granularity);
                match hl_fetch_px(&coin, start, end, interval, secs).await {
                    Ok(c) => c,
                    Err(e) => {
                        let e = hl_explain(&coin, e);
                        errors.push(err(ticker, "fetch", format!("hyperliquid candleSnapshot '{coin}': {e}")));
                        continue;
                    }
                }
            }
            HlMetric::Oi | HlMetric::OiUsd => {
                let now = Utc::now();
                if end < now - Duration::hours(1) {
                    errors.push(err(
                        ticker,
                        "fetch",
                        "hyperliquid exposes only CURRENT open interest (no history); OI cannot be served for a past as_of. Accumulate by polling.".to_string(),
                    ));
                    continue;
                }
                if ctx_cache.is_none() {
                    match hl_post(&serde_json::json!({"type": "metaAndAssetCtxs"})).await {
                        Ok(v) => ctx_cache = Some(v),
                        Err(e) => {
                            errors.push(err(ticker, "fetch", format!("hyperliquid metaAndAssetCtxs: {e}")));
                            continue;
                        }
                    }
                }
                let ctx = ctx_cache.as_ref().and_then(|b| hl_parse_asset_ctx(b, &coin));
                let Some((oi, mark)) = ctx else {
                    errors.push(err(ticker, "fetch", format!("hyperliquid: coin '{coin}' not in the main perp universe")));
                    continue;
                };
                let v = if metric == HlMetric::Oi { oi } else { oi * mark };
                errors.push(err(
                    ticker,
                    "data_quality",
                    "hyperliquid OI is a live snapshot only (single point at fetch time); no history exists upstream.".to_string(),
                ));
                vec![hl_point(now, v)]
            }
        };

        if !matches!(metric, HlMetric::Oi | HlMetric::OiUsd) {
            candles.retain(|c| c.t >= start - step && c.t <= end);
        }
        if candles.is_empty() {
            errors.push(err(ticker, "fetch", format!("hyperliquid returned no data for '{coin}' in this window")));
            continue;
        }
        candles.sort_by_key(|c| c.t);
        candles.dedup_by_key(|c| c.t);

        let native_secs = match metric {
            HlMetric::Px => hl_best_interval(granularity).1,
            HlMetric::Oi | HlMetric::OiUsd => step.num_seconds(),
            _ => 3600,
        };
        if native_secs < step.num_seconds() {
            candles = resample_candles(&candles, start, step);
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
            source: Some("hyperliquid".to_string()),
            upstream_id: Some(coin),
        });
    }

    Ok((out, errors))
}

#[cfg(test)]
mod hyperliquid_tests {
    use super::*;

    #[test]
    fn hl_ticker_grammar() {
        assert!(is_hyperliquid_ticker("HL:BTC:FUNDING"));
        assert!(is_hyperliquid_ticker("hl:btc"));
        assert!(!is_hyperliquid_ticker("HL:"));
        assert!(!is_hyperliquid_ticker("HLT"));
        assert_eq!(hl_parse_ticker("HL:btc:funding"), Some(("BTC".into(), HlMetric::Funding)));
        assert_eq!(hl_parse_ticker("HL:ETH:FUNDING_APR"), Some(("ETH".into(), HlMetric::FundingApr)));
        assert_eq!(hl_parse_ticker("HL:SOL"), Some(("SOL".into(), HlMetric::Px)));
        assert_eq!(hl_parse_ticker("HL:kPEPE:OI"), Some(("kPEPE".into(), HlMetric::Oi)));
        assert_eq!(hl_parse_ticker("HL:BTC:OI_USD"), Some(("BTC".into(), HlMetric::OiUsd)));
        assert_eq!(hl_parse_ticker("HL:xyz:TSLA:PX"), Some(("xyz:TSLA".into(), HlMetric::Px)));
        assert_eq!(hl_parse_ticker("HL:xyz:tsla"), Some(("xyz:TSLA".into(), HlMetric::Px)));
    }

    #[test]
    fn hl_interval_selection() {
        assert_eq!(hl_best_interval(Span { n: 1, unit: SpanUnit::Hour }).0, "1h");
        assert_eq!(hl_best_interval(Span { n: 6, unit: SpanUnit::Hour }).0, "2h");
        assert_eq!(hl_best_interval(Span { n: 1, unit: SpanUnit::Day }).0, "1d");
        assert_eq!(hl_best_interval(Span { n: 15, unit: SpanUnit::Minute }).0, "15m");
    }

    #[test]
    fn hl_parse_fixtures() {
        // Real responses captured 2026-10-30.
        let funding: serde_json::Value = serde_json::from_str(
            r#"[{"coin":"BTC","fundingRate":"0.0000125","premium":"-0.0002627411","time":1793365200024},
                {"coin":"BTC","fundingRate":"0.0000125","premium":"-0.000067574","time":1793372400111}]"#,
        )
        .unwrap();
        let rows = hl_parse_funding_rows(&funding);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.timestamp(), 1793365200);
        assert!((rows[0].1 - 0.0000125).abs() < 1e-12);
        assert!((rows[0].2 + 0.0002627411).abs() < 1e-12);

        let candles: serde_json::Value = serde_json::from_str(
            r#"[{"t":1793361600000,"T":1793365199999,"s":"BTC","i":"1h","o":"77338.0","c":"77438.0","h":"77570.0","l":"77233.0","v":"1580.64275","n":10654}]"#,
        )
        .unwrap();
        let c = hl_parse_candles(&candles);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].c, 77438.0);
        assert_eq!(c[0].h, 77570.0);
        assert_eq!(c[0].v, Some(1580.64275));

        let ctx: serde_json::Value = serde_json::from_str(
            r#"[{"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40},{"szDecimals":4,"name":"ETH","maxLeverage":25}]},
                [{"funding":"0.0000125","openInterest":"41119.4624","markPx":"77571.0"},
                 {"funding":"0.0000100","openInterest":"900000.5","markPx":"2700.5"}]]"#,
        )
        .unwrap();
        assert_eq!(hl_parse_asset_ctx(&ctx, "BTC"), Some((41119.4624, 77571.0)));
        assert_eq!(hl_parse_asset_ctx(&ctx, "ETH"), Some((900000.5, 2700.5)));
        assert_eq!(hl_parse_asset_ctx(&ctx, "DOGE"), None);
    }

    #[tokio::test]
    #[ignore]
    async fn hl_live_btc() {
        let end = Utc::now();
        let start = end - Duration::hours(48);
        let tickers: Vec<String> = ["HL:BTC:FUNDING", "HL:BTC:FUNDING_APR", "HL:BTC:PREMIUM", "HL:BTC:PX", "HL:BTC:OI", "HL:BTC:OI_USD"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (series, errors) = fetch_hyperliquid_series(&tickers, start, end, Span { n: 1, unit: SpanUnit::Hour })
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
        assert_eq!(series.len(), 6);
        assert!(series[0].candles.len() >= 40);
    }
}
