use crate::{Error, Result};
use crate::web::WebHit;
use chrono::{DateTime, Utc};
use serde::Deserialize;

pub async fn search_finance_ext(query: &str) -> Result<Vec<WebHit>> {
    let mut hits = Vec::new();

    // 1. House Stock Watcher (Congressional Trades)
    if let Ok(mut trades) = search_congress_trades(query).await {
        hits.append(&mut trades);
    }

    Ok(hits)
}

async fn search_congress_trades(query: &str) -> Result<Vec<WebHit>> {
    // Note: HouseStockWatcher usually provides a full daily JSON.
    // For MVP, we'll try to fetch the most recent and filter by query.
    let url = "https://housestockwatcher.com/api/v1/recent";
    let resp = reqwest::get(url).await
        .map_err(|e| Error::Provider(format!("housestockwatcher fetch failed: {e}")))?;

    #[derive(Deserialize)]
    struct Trade {
        representative: Option<String>,
        ticker: Option<String>,
        amount: Option<String>,
        transaction_date: Option<String>,
        type_: Option<String>,
    }

    let trades: Vec<Trade> = resp.json().await
        .map_err(|e| Error::Provider(format!("housestockwatcher parse failed: {e}")))?;

    let hits = trades.into_iter()
        .filter(|t| {
            t.ticker.as_deref().unwrap_or("").contains(query) || 
            t.representative.as_deref().unwrap_or("").to_lowercase().contains(&query.to_lowercase())
        })
        .map(|t| WebHit {
            title: format!("{} trade: {} by {}", t.type_.as_deref().unwrap_or_default(), t.ticker.as_deref().unwrap_or_default(), t.representative.as_deref().unwrap_or_default()),
            url: "https://housestockwatcher.com/".to_string(),
            snippet: format!("Amount: {}, Date: {}", t.amount.as_deref().unwrap_or_default(), t.transaction_date.as_deref().unwrap_or_default()),
            source: "HouseStockWatcher".to_string(),
            score: 0.95,
            published: t.transaction_date.as_ref().and_then(|d| DateTime::parse_from_rfc3339(&format!("{}T00:00:00Z", d)).ok().map(|dt| dt.with_timezone(&Utc))),
            provenance: "government_disclosure".to_string(),
        })
        .collect();

    Ok(hits)
}
