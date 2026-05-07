// Extended-hours (pre-market / after-hours) quote fetcher for finance_movers.
//
// Yahoo's chart/v8 endpoint with `includePrePost=true` returns the full minute-bar
// series across the pre/regular/post sessions plus a `currentTradingPeriod` block
// that gives us the timestamp ranges for each session. No crumb required.
//
// We compute the latest extended-hours print by walking the timestamp[] array
// backwards and finding the most recent non-null close inside the post-market
// window (or pre-market if it's morning). Change-pct is computed against
// `regularMarketPrice` — the most recent regular-session close — NOT against
// previousClose (yesterday's close), because we want the AFTER-hours move, not
// the cumulative day move.

const YAHOO_CHART_URL: &str = "https://query2.finance.yahoo.com/v8/finance/chart";
const EXTENDED_HOURS_BATCH_CHUNK: usize = 8;

#[derive(Debug, Clone, Serialize)]
pub struct ExtendedHoursQuote {
    pub ticker: String,
    /// last regular-session close (Yahoo `meta.regularMarketPrice`)
    pub regular_price: Option<f64>,
    /// prior trading day close (Yahoo `meta.chartPreviousClose`)
    pub regular_previous_close: Option<f64>,
    /// "pre" | "regular" | "post" | "closed"
    pub session: Option<String>,
    /// most recent post-market price (or pre-market if it's morning) — None when
    /// no extended-hours print has landed yet for the current window
    pub extended_price: Option<f64>,
    /// (extended_price / regular_price - 1) * 100
    pub extended_change_pct: Option<f64>,
    /// extended_price - regular_price
    pub extended_change_abs: Option<f64>,
    /// when the extended-hours print landed
    pub timestamp_utc: Option<chrono::DateTime<chrono::Utc>>,
}

impl ExtendedHoursQuote {
    fn empty(ticker: &str) -> Self {
        Self {
            ticker: ticker.to_string(),
            regular_price: None,
            regular_previous_close: None,
            session: None,
            extended_price: None,
            extended_change_pct: None,
            extended_change_abs: None,
            timestamp_utc: None,
        }
    }
}

/// Fetch a single ticker's extended-hours quote via Yahoo chart/v8.
pub async fn fetch_extended_hours_quote(ticker: &str) -> Result<ExtendedHoursQuote> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .context("build extended-hours client")?;
    fetch_extended_hours_quote_with_client(&client, ticker).await
}

async fn fetch_extended_hours_quote_with_client(
    client: &reqwest::Client,
    ticker: &str,
) -> Result<ExtendedHoursQuote> {
    let trimmed = ticker.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty ticker for extended-hours fetch");
    }
    let url = format!("{}/{}", YAHOO_CHART_URL, trimmed);
    let resp = client
        .get(&url)
        .query(&[
            ("interval", "1m"),
            ("range", "1d"),
            ("includePrePost", "true"),
        ])
        .send()
        .await
        .with_context(|| format!("fetch extended-hours for {trimmed}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "yahoo chart/v8 returned HTTP {} for {}",
            resp.status(),
            trimmed
        );
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("parse extended-hours for {trimmed}"))?;
    Ok(parse_extended_hours_payload(trimmed, &body))
}

/// Fetch many tickers in parallel, chunked at EXTENDED_HOURS_BATCH_CHUNK to avoid
/// Yahoo throttling. On error, returns an empty quote for that ticker so the
/// merge in movers.rs is total.
pub async fn fetch_extended_hours_quotes_batch(tickers: &[String]) -> Vec<ExtendedHoursQuote> {
    if tickers.is_empty() {
        return Vec::new();
    }
    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return tickers
                .iter()
                .map(|t| ExtendedHoursQuote::empty(t))
                .collect();
        }
    };

    let mut out: Vec<ExtendedHoursQuote> = Vec::with_capacity(tickers.len());
    for chunk in tickers.chunks(EXTENDED_HOURS_BATCH_CHUNK) {
        let futures_iter = chunk.iter().map(|ticker| {
            let client = client.clone();
            let ticker = ticker.clone();
            async move {
                match fetch_extended_hours_quote_with_client(&client, &ticker).await {
                    Ok(quote) => quote,
                    Err(_) => ExtendedHoursQuote::empty(&ticker),
                }
            }
        });
        let chunk_results: Vec<ExtendedHoursQuote> =
            futures::future::join_all(futures_iter).await;
        out.extend(chunk_results);
    }
    out
}

fn parse_extended_hours_payload(ticker: &str, body: &serde_json::Value) -> ExtendedHoursQuote {
    let mut quote = ExtendedHoursQuote::empty(ticker);
    let result = body
        .get("chart")
        .and_then(|c| c.get("result"))
        .and_then(|r| r.as_array())
        .and_then(|a| a.first());
    let Some(result) = result else {
        return quote;
    };
    let meta = result.get("meta");

    quote.regular_price = meta
        .and_then(|m| m.get("regularMarketPrice"))
        .and_then(|v| v.as_f64());
    quote.regular_previous_close = meta
        .and_then(|m| m.get("chartPreviousClose"))
        .or_else(|| meta.and_then(|m| m.get("previousClose")))
        .and_then(|v| v.as_f64());

    // Yahoo gives us TWO window blocks:
    //   - currentTradingPeriod: today's upcoming/active sessions
    //   - tradingPeriods: the sessions the response data actually covers
    // Between yesterday's post-close and today's pre-open (overnight), the bar
    // series is yesterday's but currentTradingPeriod has rolled to today —
    // tradingPeriods is what aligns with the bars. Prefer tradingPeriods, fall
    // back to currentTradingPeriod when absent.
    let trading_periods = meta.and_then(|m| m.get("tradingPeriods"));
    let current_period = meta.and_then(|m| m.get("currentTradingPeriod"));
    let pre_window = trading_periods
        .and_then(|t| t.get("pre"))
        .and_then(parse_nested_window)
        .or_else(|| current_period.and_then(|t| t.get("pre")).and_then(parse_window));
    let regular_window = trading_periods
        .and_then(|t| t.get("regular"))
        .and_then(parse_nested_window)
        .or_else(|| {
            current_period
                .and_then(|t| t.get("regular"))
                .and_then(parse_window)
        });
    let post_window = trading_periods
        .and_then(|t| t.get("post"))
        .and_then(parse_nested_window)
        .or_else(|| {
            current_period
                .and_then(|t| t.get("post"))
                .and_then(parse_window)
        });

    // Determine current session from market state + windows.
    let market_state = meta
        .and_then(|m| m.get("marketState"))
        .and_then(|v| v.as_str());
    quote.session = match market_state {
        Some("PRE") => Some("pre".to_string()),
        Some("REGULAR") => Some("regular".to_string()),
        Some("POST" | "POSTPOST") => Some("post".to_string()),
        Some("CLOSED" | "PREPRE") => Some("closed".to_string()),
        _ => None,
    };

    // Walk the bar series backwards, find the most recent non-null close in
    // post (preferred) or pre (fallback).
    let timestamps = result
        .get("timestamp")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let closes = result
        .get("indicators")
        .and_then(|i| i.get("quote"))
        .and_then(|q| q.as_array())
        .and_then(|a| a.first())
        .and_then(|q0| q0.get("close"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Try post-market first, then pre-market. If we're inside post we want post;
    // if pre is active and post hasn't happened yet, fall back to pre.
    let mut found = None;
    if let Some(post) = post_window {
        found = find_latest_in_window(&timestamps, &closes, post);
    }
    if found.is_none() {
        if let Some(pre) = pre_window {
            // Only use pre if we don't have a post print AND there's no
            // regular_window between pre.end and now (i.e. it's still morning
            // or pre-market). If the regular session has ended, the pre is
            // yesterday's pre — skip.
            let pre_is_current = match (pre, regular_window) {
                (_, Some(reg)) => pre.1 >= reg.0.saturating_sub(1) && pre.1 <= reg.1,
                _ => true,
            };
            if pre_is_current {
                found = find_latest_in_window(&timestamps, &closes, pre);
            }
        }
    }

    if let Some((px, ts)) = found {
        quote.extended_price = Some(px);
        quote.timestamp_utc = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0);
        if let Some(regular) = quote.regular_price {
            if regular.is_finite() && regular != 0.0 {
                quote.extended_change_pct = Some((px / regular - 1.0) * 100.0);
                quote.extended_change_abs = Some(px - regular);
            }
        }
    }

    quote
}

fn parse_window(v: &serde_json::Value) -> Option<(i64, i64)> {
    let start = v.get("start").and_then(|v| v.as_i64())?;
    let end = v.get("end").and_then(|v| v.as_i64())?;
    Some((start, end))
}

/// `meta.tradingPeriods.pre` is shaped as `[[{start, end, ...}]]` (array of
/// arrays of session objects). For the most common case (one day, one session
/// per type), peel both wrappers and parse the inner object.
fn parse_nested_window(v: &serde_json::Value) -> Option<(i64, i64)> {
    let inner = v
        .as_array()
        .and_then(|outer| outer.first())
        .and_then(|inner_arr| inner_arr.as_array())
        .and_then(|sessions| sessions.first())?;
    parse_window(inner)
}

/// Walk the bar series backwards, return (close, timestamp) for the most recent
/// non-null close whose timestamp falls inside [window.0, window.1].
fn find_latest_in_window(
    timestamps: &[serde_json::Value],
    closes: &[serde_json::Value],
    window: (i64, i64),
) -> Option<(f64, i64)> {
    let n = timestamps.len().min(closes.len());
    for i in (0..n).rev() {
        let ts = timestamps[i].as_i64()?;
        if ts < window.0 || ts > window.1 {
            continue;
        }
        if let Some(px) = closes[i].as_f64() {
            if px.is_finite() {
                return Some((px, ts));
            }
        }
    }
    None
}

#[cfg(test)]
mod movers_extended_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "hits live Yahoo; run with --ignored"]
    async fn amd_extended_hours_smoke() {
        let q = fetch_extended_hours_quote("AMD").await.unwrap();
        eprintln!("AMD extended-hours quote: {:#?}", q);
        assert_eq!(q.ticker, "AMD");
        // regular_price should always come back from chart/v8 meta
        assert!(q.regular_price.is_some(), "expected regular_price, got {:?}", q);
    }

    #[tokio::test]
    #[ignore = "hits live Yahoo; run with --ignored"]
    async fn batch_smoke() {
        let tickers = vec!["AMD".to_string(), "NVDA".to_string(), "AAPL".to_string()];
        let quotes = fetch_extended_hours_quotes_batch(&tickers).await;
        eprintln!("batch quotes: {:#?}", quotes);
        assert_eq!(quotes.len(), 3);
    }
}
