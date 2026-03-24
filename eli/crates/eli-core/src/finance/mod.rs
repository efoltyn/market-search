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

/// Shared HTTP clients with connection pooling. Reused across tool calls
/// to avoid per-request TLS handshake overhead (~100ms saved per call).
pub(crate) mod shared_client {
    use std::sync::LazyLock;

    /// General-purpose client (rustls TLS, no proxy, tcp_nodelay).
    /// Used by auctions, options, news, search, COT, odds, filings, etc.
    pub(crate) static GENERAL: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .tcp_nodelay(true)
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(4)
            .no_proxy()
            .build()
            .expect("failed to build shared general HTTP client")
    });

    /// Native-TLS client for FRED/Akamai CDN (fingerprints rustls).
    /// Used only by schedule/FRED endpoints.
    pub(crate) static NATIVE_TLS: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .use_native_tls()
            .http1_only()
            .tcp_nodelay(true)
            .timeout(std::time::Duration::from_secs(20))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(2)
            .build()
            .expect("failed to build shared native-TLS HTTP client")
    });
}

const SEC_COMPANY_TICKERS_TTL_SECS: u64 = 60 * 60 * 24 * 7; // 7 days
const SEC_SUBMISSIONS_TTL_SECS: u64 = 60 * 60 * 24; // 1 day
const SEC_DEFAULT_TEXT_MAX_CHARS: usize = 10_000;
const SCHEDULE_HTTP_TIMEOUT_SECS: u64 = 6;
const SCHEDULE_PER_DAY_TIMEOUT_SECS: u64 = 5;
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
pub use timeseries::build_snapshot_analytics;
pub use timeseries::build_timeseries_analytics;
pub use timeseries::fetch_timeseries;
pub use timeseries::is_binance_ticker;
pub use timeseries::is_pyth_ticker;
pub use timeseries::is_stooq_pe_ticker;
pub use timeseries::is_stooq_ticker;
pub use timeseries::resample_candles;

mod options;
pub use options::fetch_options;

mod news;
pub use news::fetch_news;

mod schedule;
pub use schedule::fetch_schedule;

mod rate_path;
pub use rate_path::fetch_rate_path;

mod fundamentals;
pub use fundamentals::fetch_fundamentals;

mod search;
pub use search::fetch_search;

mod snapshot;
pub use snapshot::fetch_snapshot;

mod ibkr;
pub use ibkr::{
    fetch_ibkr_options, fetch_ibkr_search, fetch_ibkr_snapshot, fetch_ibkr_timeseries,
    invoke_ibkr_bridge, resolve_ibkr_connection,
};

mod sync;
pub use sync::sync_odds;

mod paper;
pub use paper::run_paper;

mod auctions;
pub use auctions::fetch_auctions;

mod cot;
pub use cot::fetch_cot;

mod volsurface;
pub use volsurface::fetch_volsurface;

mod nyfed;
pub use nyfed::fetch_nyfed;

mod stress;
pub use stress::fetch_stress;

mod fiscal;
pub use fiscal::fetch_fiscal;

pub mod ecb;
pub use ecb::{fetch_ecb, EcbPreset, EcbRequest, EcbResponse, EcbSeries};

pub mod eia;
pub use eia::{fetch_eia, EiaPreset, EiaRequest, EiaResponse, EiaSeries};

pub mod bis;
pub use bis::{fetch_bis, BisPreset, BisRequest, BisResponse};

pub mod boj;
pub use boj::{fetch_boj, BojPreset, BojRequest, BojResponse};

pub mod boe;
pub use boe::{fetch_boe, BoePreset, BoeRequest, BoeResponse};

pub(crate) mod credentials;

pub fn has_fred_api_attachment_hint() -> bool {
    credentials::has_fred_api_configuration_hint()
}
pub mod odds_db;
pub mod policy;

pub fn default_cache_dir() -> PathBuf {
    std::env::temp_dir().join("eli-finance-cache")
}

/// Well-known Yahoo Finance indices that require a `^` prefix.
/// Users commonly type "VIX" or "GSPC" without the caret — Yahoo returns
/// empty data for the bare ticker, so we auto-correct here.
const YAHOO_INDEX_BARE_NAMES: &[&str] = &[
    "VIX", "GSPC", "DJI", "IXIC", "RUT", "N225", "HSI", "AXJO",
    "STOXX50E", "FTSE", "GDAXI", "FCHI", "BVSP", "MERV", "KS11",
    "TWII", "JKSE", "KLSE", "STI", "NZ50", "OVX", "TNX", "TYX", "IRX",
];

pub fn normalize_tickers(tickers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = tickers
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| {
            let upper = t.to_ascii_uppercase();
            // Auto-add ^ prefix for known indices when user omits it.
            if !upper.starts_with('^')
                && YAHOO_INDEX_BARE_NAMES
                    .iter()
                    .any(|idx| upper == *idx)
            {
                format!("^{upper}")
            } else {
                upper
            }
        })
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
            ibkr: None,
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
