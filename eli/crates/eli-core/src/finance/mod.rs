use crate::{Error, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;
use tokio::time::Duration as TokioDuration;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::connect_async;
use tracing::{info, warn};

const SEC_COMPANY_TICKERS_TTL_SECS: u64 = 60 * 60 * 24 * 7; // 7 days
const SEC_SUBMISSIONS_TTL_SECS: u64 = 60 * 60 * 24; // 1 day
const SEC_DEFAULT_TEXT_MAX_CHARS: usize = 10_000;
const SCHEDULE_HTTP_TIMEOUT_SECS: u64 = 12;
const SCHEDULE_PER_DAY_TIMEOUT_SECS: u64 = 10;
const YAHOO_SEARCH_URL: &str = "https://query2.finance.yahoo.com/v1/finance/search";
const KALSHI_BASE_URL: &str = "https://api.elections.kalshi.com/trade-api/v2";
const POLYMARKET_GAMMA_URL: &str = "https://gamma-api.polymarket.com";

mod types;
pub use types::*;

impl From<eli_finance_types::FinanceTypesError> for Error {
    fn from(value: eli_finance_types::FinanceTypesError) -> Self {
        match value {
            eli_finance_types::FinanceTypesError::InvalidInput(msg) => Error::InvalidInput(msg),
        }
    }
}

mod providers;
pub use providers::odds::fetch_odds;

mod filings;
pub use filings::fetch_filings;
pub use filings::fetch_insider;

mod timeseries;
pub use timeseries::fetch_timeseries;

mod options;
pub use options::fetch_options;

mod prices;
pub use prices::fetch_prices;

mod news;
pub use news::fetch_news;

mod macro_data;
pub use macro_data::fetch_macro;

mod schedule;
pub use schedule::fetch_schedule;

mod rate_path;
pub use rate_path::fetch_rate_path;

mod yield_curve;
pub use yield_curve::fetch_yield_curve;

mod dashboard;
pub use dashboard::fetch_dashboard;

mod fundamentals;
pub use fundamentals::fetch_fundamentals;

mod search;
pub use search::fetch_search;

mod snapshot;
pub use snapshot::fetch_snapshot;

mod sync;
pub use sync::sync_odds;

pub fn normalize_tickers(tickers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = tickers
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_uppercase())
        .collect();

    // Keep stable order but drop exact duplicates.
    let mut seen = std::collections::HashSet::<String>::new();
    out.retain(|t| seen.insert(t.to_string()));
    out
}

const YAHOO_OPTIONS_URL: &str = "https://query2.finance.yahoo.com/v7/finance/options";
const YAHOO_CRUMB_URL: &str = "https://query2.finance.yahoo.com/v1/test/getcrumb";

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{}>", tag);
    let end_tag = format!("</{}>", tag);

    let start = xml.find(&start_tag)? + start_tag.len();
    let end = xml.find(&end_tag)?;
    if end <= start {
        return None;
    }

    Some(xml[start..end].to_string())
}

// Legacy compatibility wrapper for simplified API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinanceRequest {
    pub tickers: Vec<String>,
    pub range: String,
    pub granularity: String,
    pub as_of: Option<String>,
}

pub struct FinanceTool;

impl FinanceTool {
    pub async fn fetch_timeseries(req: FinanceRequest) -> anyhow::Result<String> {
        let range = Span::parse(&req.range).map_err(|e| anyhow::anyhow!("invalid range: {e}"))?;
        let granularity = Span::parse(&req.granularity)
            .map_err(|e| anyhow::anyhow!("invalid granularity: {e}"))?;

        let as_of = match &req.as_of {
            Some(s) => Some(parse_as_of(s).map_err(|e| anyhow::anyhow!("invalid as_of: {e}"))?),
            None => None,
        };

        let ts_req = TimeseriesRequest {
            tickers: req.tickers,
            range,
            granularity,
            as_of,
            provider: ProviderKind::Yahoo,
            max_points_per_ticker: None,
        };

        let cache_dir = std::env::temp_dir().join("eli-finance-cache");
        let resp = fetch_timeseries(ts_req, &cache_dir)
            .await
            .map_err(|e| anyhow::anyhow!("fetch failed: {e}"))?;

        let mut output = String::new();
        for series in &resp.series {
            output.push_str(&format!("Ticker: {}\n", series.ticker));
            for candle in series.candles.iter().rev().take(20).rev() {
                output.push_str(&format!(
                    "  {}: O={:.2} H={:.2} L={:.2} C={:.2} V={}\n",
                    candle.t.format("%Y-%m-%d %H:%M"),
                    candle.o,
                    candle.h,
                    candle.l,
                    candle.c,
                    candle
                        .v
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "N/A".to_string())
                ));
            }
            output.push('\n');
        }

        Ok(output)
    }
}
