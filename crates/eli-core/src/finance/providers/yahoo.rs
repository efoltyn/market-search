use crate::finance::{Candle, Span, SpanUnit, TickerSeries};
use crate::{Error, Result};
use chrono::{DateTime, TimeZone, Utc};
use yahoo_finance_api as yf;

pub async fn fetch(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
) -> Result<Vec<TickerSeries>> {
    let provider = yf::YahooConnector::new();
    let interval = map_granularity(granularity)?;
    let mut tasks = Vec::new();

    for ticker in tickers {
        let provider = provider.clone();
        let ticker = ticker.clone();
        let interval = interval.clone();
        
        tasks.push(tokio::spawn(async move {
            let resp = provider.get_quote_history_interval(&ticker, start, end, &interval).await;
            (ticker, resp)
        }));
    }

    let mut results = Vec::new();
    for task in futures::future::join_all(tasks).await {
        let (ticker, res) = task.map_err(|e| Error::System(format!("Join error: {}", e)))?;
        
        match res {
            Ok(response) => {
                let quotes = response.quotes().map_err(|e| Error::Provider(format!("Yahoo error for {}: {}", ticker, e)))?;
                let candles: Vec<Candle> = quotes.into_iter().map(|q| {
                    Candle {
                        t: Utc.timestamp_opt(q.timestamp as i64, 0).unwrap(),
                        o: q.open,
                        h: q.high,
                        l: q.low,
                        c: q.close,
                        v: Some(q.volume as f64),
                        kind: None,
                    }
                }).collect();

                let upstream = ticker.clone();
                results.push(TickerSeries {
                    ticker,
                    candles,
                    source: Some("yahoo".to_string()),
                    upstream_id: Some(upstream),
                });
            },
            Err(e) => {
                // If it's a "Not Found" we might want to skip, but for now strict error
                return Err(Error::Provider(format!("Failed to fetch {}: {}", ticker, e)));
            }
        }
    }

    Ok(results)
}

fn map_granularity(span: Span) -> Result<String> {
    // Yahoo supports: 1m, 2m, 5m, 15m, 30m, 60m, 90m, 1h, 1d, 5d, 1wk, 1mo, 3mo
    match (span.n, span.unit) {
        (1, SpanUnit::Minute) => Ok("1m".to_string()),
        (2, SpanUnit::Minute) => Ok("2m".to_string()),
        (5, SpanUnit::Minute) => Ok("5m".to_string()),
        (15, SpanUnit::Minute) => Ok("15m".to_string()),
        (30, SpanUnit::Minute) => Ok("30m".to_string()),
        (60, SpanUnit::Minute) | (1, SpanUnit::Hour) => Ok("1h".to_string()),
        (90, SpanUnit::Minute) => Ok("90m".to_string()),
        (1, SpanUnit::Day) => Ok("1d".to_string()),
        (5, SpanUnit::Day) => Ok("5d".to_string()),
        (1, SpanUnit::Week) => Ok("1wk".to_string()),
        (1, SpanUnit::Month) => Ok("1mo".to_string()),
        (3, SpanUnit::Month) => Ok("3mo".to_string()),
        _ => Err(Error::InvalidInput(format!(
            "Yahoo Finance does not support granularity: {}{}. Supported: 1m, 2m, 5m, 15m, 30m, 60m, 90m, 1h, 1d, 5d, 1wk, 1mo, 3mo",
            span.n,
            match span.unit {
                SpanUnit::Minute => "m",
                SpanUnit::Hour => "h",
                SpanUnit::Day => "d",
                SpanUnit::Week => "wk",
                SpanUnit::Month => "mo",
                SpanUnit::Year => "y",
            }
        ))),
    }
}
