use crate::{Error, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;
use tokio::time::Duration as TokioDuration;
use tokio_tungstenite::connect_async;

const MAX_POINTS_PER_TICKER_DEFAULT: usize = 5_000;
const SEC_COMPANY_TICKERS_TTL_SECS: u64 = 60 * 60 * 24 * 7; // 7 days
const SEC_SUBMISSIONS_TTL_SECS: u64 = 60 * 60 * 24; // 1 day
const SEC_DEFAULT_TEXT_MAX_CHARS: usize = 10_000;
const YAHOO_SEARCH_URL: &str = "https://query2.finance.yahoo.com/v1/finance/search";
const YAHOO_QUOTE_SUMMARY_URL: &str = "https://query2.finance.yahoo.com/v7/finance/quoteSummary";
const KALSHI_BASE_URL: &str = "https://api.elections.kalshi.com/trade-api/v2";
const POLYMARKET_GAMMA_URL: &str = "https://gamma-api.polymarket.com";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Deterministic synthetic data for offline testing.
    Mock,
    /// Yahoo Finance chart API (free; best-effort, rate-limited).
    Yahoo,
    /// FRED CSV export (Federal Reserve Economic Data).
    Fred,
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Mock
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpanUnit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Span {
    pub n: i64,
    pub unit: SpanUnit,
}

impl Span {
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim().to_ascii_lowercase();
        if s.is_empty() {
            return Err(Error::InvalidInput("empty span".to_string()));
        }

        let mut split_at = 0usize;
        for (idx, ch) in s.char_indices() {
            if ch.is_ascii_digit() {
                split_at = idx + ch.len_utf8();
            } else {
                break;
            }
        }

        if split_at == 0 {
            return Err(Error::InvalidInput(format!(
                "invalid span '{raw}' (expected like 10min, 1h, 30d, 12mo, 5y)"
            )));
        }

        let n: i64 = s[..split_at]
            .parse()
            .map_err(|_| Error::InvalidInput(format!("invalid span number: '{raw}'")))?;
        if n <= 0 {
            return Err(Error::InvalidInput(format!("span must be > 0: '{raw}'")));
        }

        let unit_raw = s[split_at..].trim();
        let unit = match unit_raw {
            "m" => {
                return Err(Error::InvalidInput(format!(
                    "ambiguous span unit '{unit_raw}' (use min for minutes or mo for months)"
                )));
            }
            "min" | "mins" | "minute" | "minutes" => SpanUnit::Minute,
            "h" | "hr" | "hrs" | "hour" | "hours" => SpanUnit::Hour,
            "d" | "day" | "days" => SpanUnit::Day,
            "w" | "wk" | "wks" | "week" | "weeks" => SpanUnit::Week,
            "mo" | "mon" | "month" | "months" => SpanUnit::Month,
            "y" | "yr" | "yrs" | "year" | "years" => SpanUnit::Year,
            _ => {
                return Err(Error::InvalidInput(format!(
                    "invalid span unit '{unit_raw}' (expected min,h,d,w,mo,y)"
                )));
            }
        };

        Ok(Self { n, unit })
    }

    /// Approximate duration for sizing/limits.
    pub fn approx_duration(&self) -> Duration {
        match self.unit {
            SpanUnit::Minute => Duration::minutes(self.n),
            SpanUnit::Hour => Duration::hours(self.n),
            SpanUnit::Day => Duration::days(self.n),
            SpanUnit::Week => Duration::weeks(self.n),
            SpanUnit::Month => Duration::days(self.n * 30),
            SpanUnit::Year => Duration::days(self.n * 365),
        }
    }

    pub fn to_string_compact(&self) -> String {
        let suffix = match self.unit {
            SpanUnit::Minute => "min",
            SpanUnit::Hour => "h",
            SpanUnit::Day => "d",
            SpanUnit::Week => "w",
            SpanUnit::Month => "mo",
            SpanUnit::Year => "y",
        };
        format!("{}{}", self.n, suffix)
    }
}

pub fn parse_as_of(raw: &str) -> Result<DateTime<Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(Error::InvalidInput("empty as-of".to_string()));
    }

    // Prefer explicit RFC3339 when provided.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Accept YYYY-MM-DD as shorthand (assume end-of-day UTC).
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| Error::InvalidInput(format!("invalid as-of '{raw}' (use YYYY-MM-DD or RFC3339)")))?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(
        date.and_hms_opt(23, 59, 59)
            .ok_or_else(|| Error::InvalidInput(format!("invalid as-of date: '{raw}'")))?,
        Utc,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeseriesRequest {
    pub tickers: Vec<String>,
    pub range: Span,
    pub granularity: Span,

    #[serde(default)]
    pub as_of: Option<DateTime<Utc>>,

    #[serde(default)]
    pub provider: ProviderKind,

    #[serde(default)]
    pub max_points_per_ticker: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candle {
    /// Timestamp (UTC).
    pub t: DateTime<Utc>,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TickerSeries {
    pub ticker: String,
    pub candles: Vec<Candle>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheInfo {
    pub hit: bool,
    pub path: String,
    pub key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolErrorInfo {
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeseriesError {
    pub ticker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeseriesResponse {
    pub provider: ProviderKind,
    pub tickers: Vec<String>,
    pub granularity: Span,
    pub range: Span,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub generated_at: DateTime<Utc>,
    pub series: Vec<TickerSeries>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<TimeseriesError>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_tickers: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub analytics: Option<TimeseriesAnalytics>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRequest {
    pub tickers: Vec<String>,

    #[serde(default)]
    pub provider: ProviderKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TickerSnapshot {
    pub ticker: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_price: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_close: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_low: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_high: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_cap: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise_value: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub shares_outstanding: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub float_shares: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_split_factor: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_split_date: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotResponse {
    pub provider: ProviderKind,
    pub tickers: Vec<String>,
    pub generated_at: DateTime<Utc>,
    pub snapshots: Vec<TickerSnapshot>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub analytics: Option<SnapshotAnalytics>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeseriesAnalytics {
    pub stats: BTreeMap<String, TimeseriesStats>,
    pub correlation_matrix: BTreeMap<String, BTreeMap<String, Option<f64>>>,
    pub periods_per_year: f64,

    #[serde(default = "default_risk_free_rate_annual")]
    pub risk_free_rate_annual: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeseriesStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_return: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annualized_vol: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpe_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_strength: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotAnalytics {
    pub market_caps: BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_market_cap: Option<u64>,
    pub market_cap_weights: BTreeMap<String, f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_returns: Option<BTreeMap<String, f64>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_strength: Option<BTreeMap<String, f64>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilingsRequest {
    pub ticker: String,

    /// Form types to include (e.g. ["8-K","10-K","10-Q"]). If empty, defaults to 8-K/10-K/10-Q.
    #[serde(default)]
    pub forms: Vec<String>,

    /// Max number of filings to return (most recent first).
    #[serde(default)]
    pub limit: Option<usize>,

    /// If true, download the primary document and save as text under cache_dir.
    #[serde(default)]
    pub include_text: bool,

    /// Max chars for the inline excerpt (full text is still written to disk when include_text=true).
    #[serde(default)]
    pub max_chars: Option<usize>,

    /// Optional SEC User-Agent override (e.g. "eli-cli (mailto:...)").
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilingDoc {
    pub form: String,
    pub filing_date: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_date: Option<String>,

    pub accession_number: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance_datetime: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_document: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_doc_description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub filing_index_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_path: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_excerpt: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilingsResponse {
    pub ticker: String,
    pub cik: String,
    pub company_name: String,
    pub generated_at: DateTime<Utc>,
    pub filings: Vec<FilingDoc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundamentalsRequest {
    pub ticker: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinancialStatement {
    pub date: String,
    pub period: String, // e.g. "quarterly" or "annual"
    
    // Income Statement items
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_revenue: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_of_revenue: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gross_profit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_income: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_income: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ebitda: Option<i64>,

    // Balance Sheet items
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_assets: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_liabilities: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_equity: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cash_and_equivalents: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_debt: Option<i64>,

    // Cash Flow items
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_cash_flow: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub investing_cash_flow: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub financing_cash_flow: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capital_expenditure: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_cash_flow: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundamentalsResponse {
    pub ticker: String,
    pub company_name: Option<String>,
    pub currency: Option<String>,
    pub generated_at: DateTime<Utc>,
    pub statements: Vec<FinancialStatement>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchItem {
    pub symbol: String,
    pub name: Option<String>,
    pub exchange: Option<String>,
    pub asset_type: Option<String>, // e.g. "EQUITY", "INDEX", "ETF"
    pub score: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub results: Vec<SearchItem>,
    pub macro_suggestions: Vec<SearchItem>, // Curated FRED-like IDs
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewsItem {
    pub title: String,
    pub link: String,
    pub date: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewsRequest {
    pub ticker: String,
    pub date: String, // YYYY-MM-DD
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewsResponse {
    pub ticker: String,
    pub date: String,
    pub news: Vec<NewsItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroRequest {
    pub range: Option<Span>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroIndicator {
    pub symbol: String,
    pub name: String,
    pub current_value: f64,
    pub change_1y: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroResponse {
    pub generated_at: DateTime<Utc>,
    pub indicators: Vec<MacroIndicator>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PricesRequest {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub asset_type: Option<String>,
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PricePoint {
    pub source: String,
    pub symbol: String,
    pub value: f64,
    pub timestamp: u64,
    pub received_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PricesResponse {
    pub source: String,
    pub generated_at: DateTime<Utc>,
    pub prices: Vec<PricePoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disambiguation: Option<PriceDisambiguation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PriceDisambiguation {
    pub query: String,
    pub candidates: Vec<PriceCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PriceCandidate {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsRequest {
    /// Data source: kalshi (default), polymarket, or auto (kalshi then polymarket).
    #[serde(default)]
    pub provider: Option<String>,
    /// If true, skip Kalshi and force Polymarket (useful for testing).
    #[serde(default)]
    pub disable_kalshi: bool,
    #[serde(default)]
    pub series_ticker: Option<String>,
    #[serde(default)]
    pub event_ticker: Option<String>,
    #[serde(default)]
    pub market_ticker: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub max_pages: Option<usize>,
    #[serde(default)]
    pub include_orderbook: bool,
    #[serde(default)]
    pub orderbook_depth: Option<usize>,
    /// If true, list available series (optionally filtered by category or search term).
    #[serde(default)]
    pub list_series: bool,
    /// If true, list open events (optionally filtered by category or search term).
    #[serde(default)]
    pub list_events: bool,
    /// If true, list open markets (optionally filtered by series_ticker).
    #[serde(default)]
    pub list_markets: bool,
    /// If true, list Polymarket tags.
    #[serde(default)]
    pub list_tags: bool,
    /// Filter by category (e.g., "Financials", "Politics", "Science and Technology").
    #[serde(default)]
    pub category: Option<String>,
    /// Search term to filter titles.
    #[serde(default)]
    pub search: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSeries {
    pub ticker: String,
    pub title: String,
    pub category: Option<String>,
    pub frequency: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsEvent {
    pub ticker: String,
    pub title: String,
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<OddsTag>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsMarket {
    pub ticker: String,
    pub title: String,
    pub event_ticker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_bid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_ask: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcomes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_prices: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clob_token_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability_yes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_best_bids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_best_asks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orderbook_timestamp: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsOrderLevel {
    pub price: i64,
    pub quantity: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsOrderbook {
    pub market_ticker: String,
    pub yes: Vec<OddsOrderLevel>,
    pub no: Vec<OddsOrderLevel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsListedEvent {
    pub ticker: String,
    pub title: String,
    pub category: Option<String>,
    pub series_ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<OddsTag>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsListedMarket {
    pub ticker: String,
    pub title: String,
    pub event_ticker: String,
    pub yes_price: Option<i64>,
    pub volume: Option<i64>,
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcomes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_prices: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clob_token_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability_yes: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsResponse {
    pub base_url: String,
    pub generated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<OddsSeries>,
    pub events: Vec<OddsEvent>,
    pub markets: Vec<OddsMarket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orderbook: Option<OddsOrderbook>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Available series when list_series is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_series: Option<Vec<OddsSeries>>,
    /// Available events when list_events is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_events: Option<Vec<OddsListedEvent>>,
    /// Available markets when list_markets is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_markets: Option<Vec<OddsListedMarket>>,
    /// Available tags when list_tags is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_tags: Option<Vec<OddsTag>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<OddsSourceInfo>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsTag {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSourceInfo {
    pub source: String,
    pub base_url: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Options Chain (Yahoo Finance)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptionsRequest {
    pub ticker: String,
    /// Specific expiration date (YYYY-MM-DD). If None, returns first available expiry.
    #[serde(default)]
    pub expiry: Option<String>,
    /// Filter: "calls", "puts", or None for both.
    #[serde(default)]
    pub option_type: Option<String>,
    /// Only return strikes within this percentage of current price (e.g., 10 = ±10%).
    #[serde(default)]
    pub near_money_pct: Option<f64>,
    /// If true, only return summary metrics without full chain.
    #[serde(default)]
    pub summary_only: bool,
    /// If true, list available expirations without fetching chain.
    #[serde(default)]
    pub list_expirations: bool,
    /// If true, fetch summary metrics across multiple expirations (fast signal mode).
    #[serde(default)]
    pub multi_expiry: bool,
    /// Number of expirations to fetch in multi_expiry mode (default 3).
    #[serde(default)]
    pub num_expiries: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptionContract {
    pub contract_symbol: String,
    pub strike: f64,
    pub expiry: String,
    pub option_type: String, // "call" or "put"
    pub bid: f64,
    pub ask: f64,
    pub last: f64,
    pub change: f64,
    pub pct_change: f64,
    pub volume: u64,
    pub open_interest: u64,
    pub implied_volatility: f64,
    pub in_the_money: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptionsMetrics {
    pub underlying_price: f64,
    pub put_call_ratio_volume: f64,
    pub put_call_ratio_oi: f64,
    pub total_call_volume: u64,
    pub total_put_volume: u64,
    pub total_call_oi: u64,
    pub total_put_oi: u64,
    pub atm_iv_call: Option<f64>,
    pub atm_iv_put: Option<f64>,
    pub max_pain: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptionsResponse {
    pub ticker: String,
    pub underlying_price: f64,
    pub generated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorInfo>,
    pub expirations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_expiry: Option<String>,
    pub calls: Vec<OptionContract>,
    pub puts: Vec<OptionContract>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<OptionsMetrics>,
    /// Human-readable hint when options are unavailable or filtered out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Multi-expiry summary (only present when multi_expiry=true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_expiry_summary: Option<MultiExpirySummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExpirySnapshot {
    pub expiry: String,
    pub days_to_expiry: i64,
    pub total_volume: u64,
    pub total_oi: u64,
    pub put_call_ratio_volume: f64,
    pub put_call_ratio_oi: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_pain: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atm_iv: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MultiExpirySummary {
    pub snapshots: Vec<ExpirySnapshot>,
    pub aggregate_volume: u64,
    pub weighted_put_call_ratio: f64,
    /// "bullish" if near-term P/C < 0.7, "bearish" if > 1.3, else "neutral"
    pub near_term_bias: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Insider Trading (SEC Form 4)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InsiderRequest {
    pub ticker: String,
    /// Number of days to look back (default 90).
    #[serde(default)]
    pub days: Option<u32>,
    /// Max number of transactions to return.
    #[serde(default)]
    pub limit: Option<usize>,
    /// If true, only return summary metrics without transaction list.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional SEC User-Agent override.
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InsiderTransaction {
    pub filing_date: String,
    pub transaction_date: String,
    pub insider_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insider_title: Option<String>,
    pub is_director: bool,
    pub is_officer: bool,
    pub is_ten_percent_owner: bool,
    /// P = Purchase, S = Sale, A = Award, M = Exercise, etc.
    pub transaction_code: String,
    pub shares: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_per_share: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// A = Acquired, D = Disposed
    pub acquired_disposed: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shares_owned_after: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InsiderSummary {
    pub buy_count: u32,
    pub sell_count: u32,
    pub buy_shares: f64,
    pub sell_shares: f64,
    pub buy_value: f64,
    pub sell_value: f64,
    pub net_shares: f64,
    pub net_value: f64,
    pub unique_insiders: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InsiderResponse {
    pub ticker: String,
    pub company_name: String,
    pub cik: String,
    pub generated_at: DateTime<Utc>,
    pub days_lookback: u32,
    pub summary: InsiderSummary,
    pub transactions: Vec<InsiderTransaction>,
}

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

fn periods_per_year(granularity: Span) -> f64 {
    let n = granularity.n.max(1) as f64;
    match granularity.unit {
        SpanUnit::Minute => (252.0 * 24.0 * 60.0) / n,
        SpanUnit::Hour => (252.0 * 24.0) / n,
        SpanUnit::Day => 252.0 / n,
        SpanUnit::Week => 52.0 / n,
        SpanUnit::Month => 12.0 / n,
        SpanUnit::Year => 1.0 / n,
    }
}

fn default_risk_free_rate_annual() -> f64 {
    0.04
}

fn build_snapshot_analytics(snapshots: &[TickerSnapshot]) -> SnapshotAnalytics {
    let mut market_caps: BTreeMap<String, u64> = BTreeMap::new();
    for snap in snapshots {
        if let Some(cap) = snap.market_cap {
            market_caps.insert(snap.ticker.clone(), cap);
        }
    }
    let total_market_cap = if market_caps.is_empty() {
        None
    } else {
        Some(market_caps.values().sum())
    };
    let mut market_cap_weights: BTreeMap<String, f64> = BTreeMap::new();
    if let Some(total) = total_market_cap {
        if total > 0 {
            let denom = total as f64;
            for (ticker, cap) in &market_caps {
                market_cap_weights.insert(ticker.clone(), (*cap as f64) / denom);
            }
        }
    }

    let mut daily_returns: BTreeMap<String, f64> = BTreeMap::new();
    for snap in snapshots {
        if let (Some(curr), Some(prev)) = (snap.current_price, snap.previous_close) {
            if prev != 0.0 {
                daily_returns.insert(snap.ticker.clone(), (curr / prev) - 1.0);
            }
        }
    }
    let daily_returns = if daily_returns.is_empty() {
        None
    } else {
        Some(daily_returns)
    };

    let relative_strength = daily_returns.as_ref().and_then(|dr| {
        if dr.is_empty() {
            return None;
        }
        let mean = dr.values().sum::<f64>() / dr.len() as f64;
        let mut rs: BTreeMap<String, f64> = BTreeMap::new();
        for (ticker, r) in dr {
            rs.insert(ticker.clone(), *r - mean);
        }
        Some(rs)
    });

    SnapshotAnalytics {
        market_caps,
        total_market_cap,
        market_cap_weights,
        daily_returns,
        relative_strength,
    }
}

fn build_timeseries_analytics(series: &[TickerSeries], granularity: Span) -> TimeseriesAnalytics {
    let mut dates: BTreeSet<DateTime<Utc>> = BTreeSet::new();
    for s in series {
        for candle in &s.candles {
            dates.insert(candle.t);
        }
    }
    let aligned_dates: Vec<DateTime<Utc>> = dates.into_iter().collect();

    let mut aligned_returns: BTreeMap<String, Vec<Option<f64>>> = BTreeMap::new();
    let mut stats: BTreeMap<String, TimeseriesStats> = BTreeMap::new();

    let per_year = periods_per_year(granularity);
    let rf_annual = default_risk_free_rate_annual();
    let rf_per_period = if per_year > 0.0 {
        rf_annual / per_year
    } else {
        0.0
    };

    for s in series {
        let mut price_map: HashMap<DateTime<Utc>, f64> = HashMap::new();
        for candle in &s.candles {
            price_map.insert(candle.t, candle.c);
        }
        let mut prices: Vec<Option<f64>> = Vec::with_capacity(aligned_dates.len());
        for d in &aligned_dates {
            prices.push(price_map.get(d).copied());
        }

        // Align by union of timestamps; forward-fill gaps once the series has started.
        let mut last: Option<f64> = None;
        for p in &mut prices {
            if p.is_some() {
                last = *p;
            } else if let Some(v) = last {
                *p = Some(v);
            }
        }

        let mut returns: Vec<Option<f64>> = Vec::with_capacity(prices.len());
        for i in 0..prices.len() {
            if i == 0 {
                returns.push(None);
                continue;
            }
            match (prices[i], prices[i - 1]) {
                (Some(curr), Some(prev)) if prev != 0.0 => returns.push(Some((curr / prev) - 1.0)),
                _ => returns.push(None),
            }
        }

        let first = prices.iter().find_map(|v| *v);
        let last = prices.iter().rev().find_map(|v| *v);
        let total_return = match (first, last) {
            (Some(f), Some(l)) if f != 0.0 => Some((l / f) - 1.0),
            _ => None,
        };

        let valid_returns: Vec<f64> = returns.iter().filter_map(|v| *v).collect();
        let (annualized_vol, sharpe_ratio) = if valid_returns.len() >= 2 {
            let mean = valid_returns.iter().sum::<f64>() / valid_returns.len() as f64;
            let mut var = 0.0;
            for r in &valid_returns {
                var += (*r - mean) * (*r - mean);
            }
            let denom = (valid_returns.len() as f64 - 1.0).max(1.0);
            let std = (var / denom).sqrt();
            if std > 0.0 {
                let ann_vol = std * per_year.sqrt();
                let sharpe = (mean - rf_per_period) * per_year.sqrt() / std;
                (Some(ann_vol), Some(sharpe))
            } else {
                (Some(0.0), None)
            }
        } else {
            (None, None)
        };

        aligned_returns.insert(s.ticker.clone(), returns);
        stats.insert(
            s.ticker.clone(),
            TimeseriesStats {
                total_return,
                annualized_vol,
                sharpe_ratio,
                relative_strength: None,
            },
        );
    }

    // Relative strength: outperformance vs the mean total return of the request.
    let mean_total_return = {
        let vals: Vec<f64> = stats.values().filter_map(|s| s.total_return).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<f64>() / vals.len() as f64)
        }
    };
    if let Some(mean) = mean_total_return {
        for s in stats.values_mut() {
            if let Some(tr) = s.total_return {
                s.relative_strength = Some(tr - mean);
            }
        }
    }

    let mut correlation_matrix: BTreeMap<String, BTreeMap<String, Option<f64>>> = BTreeMap::new();
    let tickers: Vec<String> = aligned_returns.keys().cloned().collect();
    for t1 in &tickers {
        let mut row: BTreeMap<String, Option<f64>> = BTreeMap::new();
        for t2 in &tickers {
            let r1 = aligned_returns.get(t1).cloned().unwrap_or_default();
            let r2 = aligned_returns.get(t2).cloned().unwrap_or_default();
            let mut xs: Vec<f64> = Vec::new();
            let mut ys: Vec<f64> = Vec::new();
            let n = r1.len().min(r2.len());
            for i in 0..n {
                if let (Some(a), Some(b)) = (r1[i], r2[i]) {
                    xs.push(a);
                    ys.push(b);
                }
            }
            row.insert(t2.clone(), correlation(&xs, &ys));
        }
        correlation_matrix.insert(t1.clone(), row);
    }

    TimeseriesAnalytics {
        stats,
        correlation_matrix,
        periods_per_year: per_year,
        risk_free_rate_annual: rf_annual,
    }
}

fn correlation(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < 2 || ys.len() < 2 || xs.len() != ys.len() {
        return None;
    }
    let mean_x = xs.iter().sum::<f64>() / xs.len() as f64;
    let mean_y = ys.iter().sum::<f64>() / ys.len() as f64;
    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    for i in 0..xs.len() {
        let dx = xs[i] - mean_x;
        let dy = ys[i] - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }
    if var_x == 0.0 || var_y == 0.0 {
        return None;
    }
    Some(cov / (var_x.sqrt() * var_y.sqrt()))
}

pub async fn fetch_timeseries(req: TimeseriesRequest, cache_dir: &Path) -> Result<TimeseriesResponse> {
    let tickers = normalize_tickers(&req.tickers);
    if tickers.is_empty() {
        return Err(Error::InvalidInput("at least one ticker is required".to_string()));
    }

    let max_points = req
        .max_points_per_ticker
        .unwrap_or(MAX_POINTS_PER_TICKER_DEFAULT)
        .max(2);

    let now = Utc::now();
    let mut end = req.as_of.unwrap_or(now);
    if end > now {
        end = now;
    }
    let start = end
        .checked_sub_signed(req.range.approx_duration())
        .ok_or_else(|| Error::InvalidInput("range underflow".to_string()))?;

    let step = req.granularity.approx_duration();
    if step.num_seconds() <= 0 {
        return Err(Error::InvalidInput("granularity must be > 0".to_string()));
    }

    let approx_points = ((end - start).num_seconds() / step.num_seconds()).max(1) as usize + 1;
    if approx_points > max_points {
        return Err(Error::InvalidInput(format!(
            "requested ~{approx_points} points per ticker exceeds limit {max_points}; increase granularity or shrink range"
        )));
    }

    let cache_key = cache_key(&req, &tickers, start, end)?;
    let cache_path = cache_path(cache_dir, &cache_key);

    if cache_path.exists() {
        let raw = std::fs::read_to_string(&cache_path)?;
        let mut resp: TimeseriesResponse = serde_json::from_str(&raw)?;
        if resp.analytics.is_none() {
            resp.analytics = Some(build_timeseries_analytics(&resp.series, resp.granularity));
            if let Ok(json) = serde_json::to_string_pretty(&resp) {
                let _ = std::fs::write(&cache_path, json);
            }
        }
        resp.cache = Some(CacheInfo {
            hit: true,
            path: cache_path.display().to_string(),
            key: cache_key,
        });
        return Ok(resp);
    }

    std::fs::create_dir_all(cache_path.parent().unwrap_or(cache_dir))?;

    let generated_at = Utc::now();
    let (series, errors) = match req.provider {
        ProviderKind::Mock => (generate_mock_series(&tickers, start, end, step), Vec::new()),
        ProviderKind::Yahoo => {
            fetch_yahoo_series(&tickers, start, end, req.granularity, max_points).await?
        }
        ProviderKind::Fred => fetch_fred_series(&tickers, start, end, req.granularity).await?,
    };

    if !errors.is_empty() {
        let valid_tickers: Vec<String> = series.iter().map(|s| s.ticker.clone()).collect();
        let error = ToolErrorInfo {
            error: "TickerFetchFailed".to_string(),
            message: "One or more tickers failed to fetch timeseries data; no series returned."
                .to_string(),
            hint: Some("All requested tickers must be valid for this provider.".to_string()),
        };
        return Ok(TimeseriesResponse {
            provider: req.provider,
            tickers: tickers.clone(),
            granularity: req.granularity,
            range: req.range,
            start,
            end,
            generated_at,
            series: Vec::new(),
            status: Some("error".to_string()),
            error: Some(error),
            errors: Some(errors),
            valid_tickers: if valid_tickers.is_empty() {
                None
            } else {
                Some(valid_tickers)
            },
            analytics: None,
            cache: None,
        });
    }

    let resp = TimeseriesResponse {
        provider: req.provider,
        tickers: tickers.clone(),
        granularity: req.granularity,
        range: req.range,
        start,
        end,
        generated_at,
        series,
        status: None,
        error: None,
        errors: None,
        valid_tickers: None,
        analytics: None,
        cache: Some(CacheInfo {
            hit: false,
            path: cache_path.display().to_string(),
            key: cache_key.clone(),
        }),
    };

    let mut resp = resp;
    resp.analytics = Some(build_timeseries_analytics(&resp.series, resp.granularity));

    let json = serde_json::to_string_pretty(&resp)?;
    std::fs::write(&cache_path, json)?;

    Ok(resp)
}

pub async fn fetch_fundamentals(req: FundamentalsRequest) -> Result<FundamentalsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    let mut connector = yahoo_finance_api::YahooConnector::new()
        .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

    let info = connector
        .get_ticker_info(&ticker)
        .await
        .map_err(|e| Error::Provider(format!("yahoo fundamentals failed for '{ticker}': {e}")))?;

    let qs = info.quote_summary.ok_or_else(|| {
        Error::Provider(format!("yahoo quote summary missing for '{ticker}'"))
    })?;
    let result = qs.result.ok_or_else(|| {
        Error::Provider(format!("yahoo quote summary result missing for '{ticker}'"))
    })?;
    let first = result.get(0).ok_or_else(|| {
        Error::Provider(format!("yahoo quote summary result empty for '{ticker}'"))
    })?;

    let quote_type = first.quote_type.as_ref();
    let stats = first.default_key_statistics.as_ref();
    let fin = first.financial_data.as_ref();

    let company_name = quote_type.and_then(|q| q.long_name.clone().or_else(|| q.short_name.clone()));
    let currency = fin.and_then(|f| f.financial_currency.clone());

    let mut statement = FinancialStatement {
        date: Utc::now().date_naive().format("%Y-%m-%d").to_string(),
        period: "current".to_string(),
        total_revenue: fin.and_then(|f| f.total_revenue).map(|v| v as i64),
        cost_of_revenue: None,
        gross_profit: fin.and_then(|f| f.gross_profits).map(|v| v as i64),
        operating_income: fin.and_then(|f| f.operating_cashflow),
        net_income: None, // not directly available in spot financialData
        ebitda: fin.and_then(|f| f.ebitda).map(|v| v as i64),
        total_assets: None,
        total_liabilities: None,
        total_equity: None,
        cash_and_equivalents: fin.and_then(|f| f.total_cash).map(|v| v as i64),
        total_debt: fin.and_then(|f| f.total_debt).map(|v| v as i64),
        operating_cash_flow: fin.and_then(|f| f.operating_cashflow),
        investing_cash_flow: None,
        financing_cash_flow: None,
        capital_expenditure: None,
        free_cash_flow: fin.and_then(|f| f.free_cashflow),
    };

    Ok(FundamentalsResponse {
        ticker,
        company_name,
        currency,
        generated_at: Utc::now(),
        statements: vec![statement],
    })
}

pub async fn fetch_search(req: SearchRequest) -> Result<SearchResponse> {
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Err(Error::InvalidInput("search query is required".to_string()));
    }

    let client = reqwest::Client::builder().no_proxy().build().map_err(|e| {
        Error::Provider(format!("search client init failed: {e}"))
    })?;

    let resp = client
        .get(YAHOO_SEARCH_URL)
        .query(&[("q", query.as_str()), ("quotesCount", "10"), ("newsCount", "0")])
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("yahoo search fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "yahoo search fetch failed: http {}",
            resp.status()
        )));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| {
        Error::Provider(format!("yahoo search parse failed: {e}"))
    })?;

    let mut results = Vec::new();
    if let Some(quotes) = json["quotes"].as_array() {
        for q in quotes {
            let symbol = q["symbol"].as_str().unwrap_or_default();
            if symbol.is_empty() {
                continue;
            }
            let mut score = q["score"].as_f64().unwrap_or(0.0);
            let exchange = q["exchange"].as_str().unwrap_or_default();
            
            // Boost major US exchanges to surface primary assets (AAPL, GC=F, etc) over obscure ETFs
            if matches!(exchange, "NYQ" | "NMS" | "CMX" | "NYM" | "CBT" | "PNK" | "BATS") {
                score *= 10.0;
            }

            results.push(SearchItem {
                symbol: symbol.to_string(),
                name: q["shortname"].as_str().or(q["longname"].as_str()).map(|s| s.to_string()),
                exchange: Some(exchange.to_string()),
                asset_type: q["quoteType"].as_str().map(|s| s.to_string()),
                score: Some(score),
            });
        }
    }

    // Sort by boosted score
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Curated Macro Suggestions (FRED IDs)
    let macro_items = vec![
        SearchItem { symbol: "CPIAUCSL".into(), name: Some("CPI (Headline Inflation)".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
        SearchItem { symbol: "UNRATE".into(), name: Some("Unemployment Rate".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
        SearchItem { symbol: "PAYEMS".into(), name: Some("Non-farm Payrolls".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
        SearchItem { symbol: "FEDFUNDS".into(), name: Some("Fed Funds Rate".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
        SearchItem { symbol: "GDPC1".into(), name: Some("Real GDP".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
        SearchItem { symbol: "T10Y2Y".into(), name: Some("10Y-2Y Yield Spread".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
        SearchItem { symbol: "M2SL".into(), name: Some("M2 Money Supply".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
        SearchItem { symbol: "INDPRO".into(), name: Some("Industrial Production".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
        SearchItem { symbol: "DCOILWTICO".into(), name: Some("WTI Oil Price".into()), exchange: Some("FRED".into()), asset_type: Some("MACRO".into()), score: None },
    ];

    let query_lower = query.to_lowercase();
    let suggestions = if query_lower.len() > 2 {
        macro_items.into_iter()
            .filter(|item| {
                item.symbol.to_lowercase().contains(&query_lower) ||
                item.name.as_ref().map(|n| n.to_lowercase().contains(&query_lower)).unwrap_or(false)
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(SearchResponse {
        query,
        results,
        macro_suggestions: suggestions,
    })
}

pub async fn fetch_snapshot(req: SnapshotRequest) -> Result<SnapshotResponse> {
    let tickers = normalize_tickers(&req.tickers);
    if tickers.is_empty() {
        return Err(Error::InvalidInput("at least one ticker is required".to_string()));
    }

    let generated_at = Utc::now();
    let snapshots = match req.provider {
        ProviderKind::Mock => generate_mock_snapshots(&tickers),
        ProviderKind::Yahoo => fetch_yahoo_snapshots(&tickers).await?,
        ProviderKind::Fred => {
            return Err(Error::InvalidInput(
                "fred provider does not support snapshot (use timeseries for macro series IDs)"
                    .to_string(),
            ))
        }
    };
    let analytics = build_snapshot_analytics(&snapshots);

    Ok(SnapshotResponse {
        provider: req.provider,
        tickers,
        generated_at,
        snapshots,
        analytics: Some(analytics),
    })
}

pub async fn fetch_filings(req: FilingsRequest, cache_dir: &Path) -> Result<FilingsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    let mut forms = req
        .forms
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase())
        .collect::<Vec<_>>();
    if forms.is_empty() {
        forms = vec!["8-K".to_string(), "10-K".to_string(), "10-Q".to_string()];
    }
    let forms_set: std::collections::HashSet<String> = forms.into_iter().collect();

    let limit = req.limit.unwrap_or(5).clamp(1, 25);
    let excerpt_max = req.max_chars.unwrap_or(SEC_DEFAULT_TEXT_MAX_CHARS).max(256);

    let sec_dir = cache_dir.join("finance").join("sec");
    std::fs::create_dir_all(&sec_dir)?;

    let (cik_str, company_name) = sec_lookup_cik(&ticker, &sec_dir, req.user_agent.as_deref()).await?;
    let submissions = sec_fetch_submissions(&cik_str, &company_name, &sec_dir, req.user_agent.as_deref()).await?;

    let recent = submissions
        .filings
        .as_ref()
        .and_then(|f| f.recent.as_ref())
        .ok_or_else(|| Error::Provider(format!("sec submissions missing recent filings for '{ticker}'")))?;

    let n = recent.form.len();
    let cik_num = submissions
        .cik
        .trim_start_matches('0')
        .parse::<u64>()
        .unwrap_or_else(|_| submissions.cik.parse::<u64>().unwrap_or(0));

    let mut out: Vec<FilingDoc> = Vec::new();
    let client = sec_client(req.user_agent.as_deref())?;

    for i in 0..n {
        let form = recent.form.get(i).cloned().unwrap_or_default();
        if !forms_set.contains(&form.to_ascii_uppercase()) {
            continue;
        }
        let accession = recent.accession_number.get(i).cloned().unwrap_or_default();
        let filing_date = recent.filing_date.get(i).cloned().unwrap_or_default();
        if accession.trim().is_empty() || filing_date.trim().is_empty() {
            continue;
        }

        let report_date = recent.report_date.as_ref().and_then(|v| v.get(i).cloned());
        let acceptance_datetime = recent
            .acceptance_date_time
            .as_ref()
            .and_then(|v| v.get(i).cloned());
        let items = recent.items.as_ref().and_then(|v| v.get(i).cloned());
        let size = recent.size.as_ref().and_then(|v| v.get(i).cloned());
        let primary_document = recent
            .primary_document
            .as_ref()
            .and_then(|v| v.get(i).cloned());
        let primary_doc_description = recent
            .primary_doc_description
            .as_ref()
            .and_then(|v| v.get(i).cloned());

        let accession_nodash = accession.replace('-', "");
        let base = format!(
            "https://www.sec.gov/Archives/edgar/data/{}/{}/",
            cik_num, accession_nodash
        );
        let filing_index_url = format!("{base}index.json");
        let url = primary_document
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .map(|doc| format!("{base}{}", doc.trim_start_matches('/')));

        let mut doc = FilingDoc {
            form,
            filing_date,
            report_date,
            accession_number: accession,
            acceptance_datetime,
            items,
            primary_document,
            primary_doc_description,
            size,
            url: url.clone(),
            filing_index_url: Some(filing_index_url),
            text_path: None,
            text_excerpt: None,
        };

        if req.include_text {
            if let Some(url) = url {
                // Download and convert to text; store on disk and return an excerpt inline.
                let raw = sec_get_text(&client, &url).await?;
                let text = html_to_text(&raw);

                let filings_dir = cache_dir
                    .join("finance")
                    .join("filings")
                    .join(&ticker);
                std::fs::create_dir_all(&filings_dir)?;
                let safe_form = sanitize_for_filename(&doc.form);
                let path = filings_dir.join(format!("{accession_nodash}_{safe_form}.txt"));
                std::fs::write(&path, text.as_bytes())?;

                doc.text_path = Some(path.display().to_string());
                doc.text_excerpt = Some(best_effort_sec_filing_excerpt(
                    &text,
                    &doc.form,
                    doc.items.as_deref(),
                    excerpt_max,
                ));
            }
        }

        out.push(doc);
        if out.len() >= limit {
            break;
        }

        // Be nice to SEC: small delay between doc fetches.
        if req.include_text {
            tokio::time::sleep(StdDuration::from_millis(125)).await;
        }
    }

    Ok(FilingsResponse {
        ticker,
        cik: cik_str,
        company_name,
        generated_at: Utc::now(),
        filings: out,
    })
}

pub async fn fetch_news(req: NewsRequest) -> Result<NewsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    let date = req.date.trim();

    // Calculate window around the date
    let target_date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| Error::InvalidInput(format!("invalid date '{date}' (use YYYY-MM-DD)")))?;

    // Tighten window: after (date - 1), before (date + 1)
    let after = target_date.pred_opt().unwrap_or(target_date);
    let before = target_date.succ_opt().unwrap_or(target_date);

    let after_str = after.format("%Y-%m-%d").to_string();
    let before_str = before.format("%Y-%m-%d").to_string();

    let url = format!(
        "https://news.google.com/rss/search?q={}+after:{}+before:{}&hl=en-US&gl=US&ceid=US:en",
        ticker, after_str, before_str
    );

    let client = reqwest::Client::builder().no_proxy().build().map_err(|e| {
        Error::Provider(format!("news client init failed: {e}"))
    })?;
    let resp = client.get(url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("news fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!("news fetch failed: http {}", resp.status())));
    }

    let xml = resp.text().await
        .map_err(|e| Error::Provider(format!("news read failed: {e}")))?;

    // Simple manual XML parsing (extracting <item> tags)
    let mut news = Vec::new();
    let mut cursor = 0;
    while let Some(start) = xml[cursor..].find("<item>") {
        let abs_start = cursor + start;
        let end = match xml[abs_start..].find("</item>") {
            Some(e) => abs_start + e + 7,
            None => break,
        };
        let item_xml = &xml[abs_start..end];

        let title = extract_xml_tag(item_xml, "title").unwrap_or_default();
        let link = extract_xml_tag(item_xml, "link").unwrap_or_default();
        let pub_date = extract_xml_tag(item_xml, "pubDate").unwrap_or_default();

        news.push(NewsItem {
            title: html_escape::decode_html_entities(&title).to_string(),
            link,
            date: pub_date,
        });

        cursor = end;
        if news.len() >= 10 { break; }
    }

    Ok(NewsResponse {
        ticker,
        date: date.to_string(),
        news,
    })
}

pub async fn fetch_macro(req: MacroRequest) -> Result<MacroResponse> {
    let indicators = vec![
        ("CPIAUCSL", "CPI (Headline Inflation)"),
        ("UNRATE", "Unemployment Rate"),
        ("PAYEMS", "Non-farm Payrolls"),
        ("FEDFUNDS", "Fed Funds Rate"),
        ("GDPC1", "Real GDP"),
        ("T10Y2Y", "10Y-2Y Yield Spread"),
        ("M2SL", "M2 Money Supply"),
        ("DCOILWTICO", "WTI Oil Price"),
    ];

    let range = req.range.unwrap_or(Span { n: 1, unit: SpanUnit::Year });
    let end = Utc::now();
    let start = end - range.approx_duration() - Duration::days(400); // extra for 1y change

    let mut out = Vec::new();
    for (id, name) in indicators {
        let series = fetch_fred_series(&[id.to_string()], start, end, Span { n: 1, unit: SpanUnit::Month }).await;
        if let Ok((mut svec, _errors)) = series {
            if let Some(s) = svec.pop() {
                if let Some(latest) = s.candles.last() {
                    let mut change_1y = None;
                    if s.candles.len() > 12 {
                        let year_ago = &s.candles[s.candles.len().saturating_sub(13)];
                        if year_ago.c != 0.0 {
                            change_1y = Some((latest.c - year_ago.c) / year_ago.c * 100.0);
                        }
                    }
                    out.push(MacroIndicator {
                        symbol: id.to_string(),
                        name: name.to_string(),
                        current_value: latest.c,
                        change_1y,
                    });
                }
            }
        }
    }

    Ok(MacroResponse {
        generated_at: Utc::now(),
        indicators: out,
    })
}

pub async fn fetch_prices(req: PricesRequest) -> Result<PricesResponse> {
    #[derive(Deserialize)]
    struct HermesFeed {
        id: String,
        #[serde(default)]
        attributes: std::collections::HashMap<String, String>,
    }
    #[derive(Deserialize)]
    struct HermesPrice {
        conf: String,
        expo: i32,
        price: String,
        publish_time: i64,
    }
    #[derive(Deserialize)]
    struct HermesParsedUpdate {
        id: String,
        price: HermesPrice,
    }
    #[derive(Deserialize)]
    struct HermesLatest {
        parsed: Vec<HermesParsedUpdate>,
    }

    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(10))
        .build()
        .map_err(|e| Error::Provider(format!("prices client init failed: {e}")))?;

    let asset_type = req
        .asset_type
        .as_deref()
        .unwrap_or("crypto")
        .to_ascii_lowercase();
    let mut ids: Vec<String> = req
        .ids
        .iter()
        .map(|s| s.trim().trim_start_matches("0x").to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut id_to_symbol: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let query = req
        .query
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let feed_id = |feed: &HermesFeed| feed.id.trim_start_matches("0x").to_string();
    let feed_symbol = |feed: &HermesFeed| feed.attributes.get("symbol").cloned();
    let feed_description = |feed: &HermesFeed| {
        feed.attributes
            .get("description")
            .cloned()
            .or_else(|| feed.attributes.get("name").cloned())
    };

    let make_candidate = |feed: &HermesFeed| PriceCandidate {
        id: feed_id(feed),
        symbol: feed.attributes.get("symbol").cloned(),
        description: feed.attributes.get("description").cloned().or_else(|| feed.attributes.get("name").cloned()),
        asset_type: feed.attributes.get("asset_type").cloned(),
    };

    let score_candidate = |query: &str, symbol: Option<&str>, description: Option<&str>| -> i32 {
        let q = query.to_ascii_lowercase();
        let mut score = 0;
        if let Some(sym) = symbol {
            let sym_l = sym.to_ascii_lowercase();
            if sym_l == q {
                score += 100;
            }
            if sym_l.starts_with(&q) {
                score += 40;
            }
            if sym_l.contains(&q) {
                score += 20;
            }
            let len_diff = (sym_l.len() as i32 - q.len() as i32).abs().min(20);
            score -= len_diff;
        }
        if let Some(desc) = description {
            let desc_l = desc.to_ascii_lowercase();
            if desc_l.contains(&q) {
                score += 10;
            }
        }
        score
    };

    if ids.is_empty() {
        let mut url = reqwest::Url::parse("https://hermes.pyth.network/v2/price_feeds")
            .map_err(|e| Error::Provider(format!("prices feeds url failed: {e}")))?;
        if let Some(query) = query.as_deref() {
            url.query_pairs_mut().append_pair("query", query);
        }
        if !asset_type.is_empty() {
            url.query_pairs_mut().append_pair("asset_type", &asset_type);
        }
        let mut feeds: Vec<HermesFeed> = client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("prices feeds fetch failed: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Provider(format!("prices feeds parse failed: {e}")))?;

        if let Some(query) = query.as_deref() {
            if feeds.is_empty() {
                let mut url = reqwest::Url::parse("https://hermes.pyth.network/v2/price_feeds")
                    .map_err(|e| Error::Provider(format!("prices feeds url failed: {e}")))?;
                if !asset_type.is_empty() {
                    url.query_pairs_mut().append_pair("asset_type", &asset_type);
                }
                feeds = client
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| Error::Provider(format!("prices feeds fetch failed: {e}")))?
                    .json()
                    .await
                    .map_err(|e| Error::Provider(format!("prices feeds parse failed: {e}")))?;
            }

            if feeds.is_empty() {
                let error = ToolErrorInfo {
                    error: "NoMatches".to_string(),
                    message: "No price feeds matched the query.".to_string(),
                    hint: Some("Provide a more specific query or explicit feed IDs.".to_string()),
                };
                return Ok(PricesResponse {
                    source: "pyth".to_string(),
                    generated_at: Utc::now(),
                    prices: vec![],
                    status: Some("error".to_string()),
                    error: Some(error),
                    disambiguation: None,
                });
            }

            let exact: Vec<&HermesFeed> = feeds
                .iter()
                .filter(|feed| {
                    feed_symbol(feed)
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case(query))
                        .unwrap_or(false)
                        || feed_id(feed).eq_ignore_ascii_case(query)
                })
                .collect();

            if exact.len() == 1 {
                let feed = exact[0];
                let id = feed_id(feed);
                let symbol = feed
                    .attributes
                    .get("symbol")
                    .cloned()
                    .unwrap_or_else(|| id.clone());
                id_to_symbol.insert(id.clone(), symbol);
                ids.push(id);
            } else {
                let mut candidates: Vec<PriceCandidate> = if exact.is_empty() {
                    let mut scored: Vec<(i32, PriceCandidate)> = feeds
                        .iter()
                        .map(|feed| {
                            (
                                score_candidate(
                                    query,
                                    feed_symbol(feed).as_deref(),
                                    feed_description(feed).as_deref(),
                                ),
                                make_candidate(feed),
                            )
                        })
                        .collect();
                    scored.sort_by(|a, b| b.0.cmp(&a.0));
                    scored.into_iter().take(5).map(|(_, c)| c).collect()
                } else {
                    exact.into_iter().map(make_candidate).collect()
                };

                if candidates.len() > 5 {
                    candidates.truncate(5);
                }

                let disambiguation = PriceDisambiguation {
                    query: query.to_string(),
                    candidates,
                    message: Some("Ambiguous query; choose a specific feed id.".to_string()),
                };

                return Ok(PricesResponse {
                    source: "pyth".to_string(),
                    generated_at: Utc::now(),
                    prices: vec![],
                    status: Some("disambiguation".to_string()),
                    error: None,
                    disambiguation: Some(disambiguation),
                });
            }
        } else {
            if feeds.is_empty() {
                let error = ToolErrorInfo {
                    error: "NoMatches".to_string(),
                    message: "No price feeds available for the requested asset type.".to_string(),
                    hint: Some("Specify a query or explicit feed IDs.".to_string()),
                };
                return Ok(PricesResponse {
                    source: "pyth".to_string(),
                    generated_at: Utc::now(),
                    prices: vec![],
                    status: Some("error".to_string()),
                    error: Some(error),
                    disambiguation: None,
                });
            }

            for feed in feeds {
                let id = feed_id(&feed);
                let symbol = feed
                    .attributes
                    .get("symbol")
                    .cloned()
                    .unwrap_or_else(|| id.clone());
                id_to_symbol.insert(id.clone(), symbol);
                ids.push(id);
            }
        }
    } else {
        for id in &ids {
            id_to_symbol.insert(id.clone(), id.clone());
        }
    }

    let mut url = reqwest::Url::parse("https://hermes.pyth.network/v2/updates/price/latest")
        .map_err(|e| Error::Provider(format!("prices latest url failed: {e}")))?;
    for id in &ids {
        url.query_pairs_mut().append_pair("ids[]", id);
    }
    url.query_pairs_mut().append_pair("parsed", "true");

    let latest: HermesLatest = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("prices latest fetch failed: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Provider(format!("prices latest parse failed: {e}")))?;

    if latest.parsed.is_empty() {
        let error = ToolErrorInfo {
            error: "NoPrices".to_string(),
            message: "No prices returned for the requested feed IDs.".to_string(),
            hint: Some("Verify feed IDs or run a query to discover valid IDs.".to_string()),
        };
        return Ok(PricesResponse {
            source: "pyth".to_string(),
            generated_at: Utc::now(),
            prices: vec![],
            status: Some("error".to_string()),
            error: Some(error),
            disambiguation: None,
        });
    }

    let mut prices = Vec::new();
    for item in latest.parsed {
        let expo = item.price.expo;
        let price_raw: f64 = item.price.price.parse().unwrap_or(0.0);
        let value = price_raw * 10f64.powi(expo);
        let symbol = id_to_symbol
            .get(&item.id.trim_start_matches("0x").to_string())
            .cloned()
            .unwrap_or_else(|| item.id.clone());
        prices.push(PricePoint {
            source: "pyth".to_string(),
            symbol,
            value,
            timestamp: item.price.publish_time as u64,
            received_at: Utc::now(),
        });
    }

    prices.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    Ok(PricesResponse {
        source: "pyth".to_string(),
        generated_at: Utc::now(),
        prices,
        status: None,
        error: None,
        disambiguation: None,
    })
}

fn json_value_to_string(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_json_array_strings(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<serde_json::Value>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(json_value_to_string)
        .collect()
}

fn parse_json_value_strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Array(arr) => arr.iter().cloned().map(json_value_to_string).collect(),
        serde_json::Value::String(s) => parse_json_array_strings(s),
        serde_json::Value::Null => Vec::new(),
        other => vec![json_value_to_string(other.clone())],
    }
}

fn parse_probability(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok()
}

fn probability_yes_from_outcomes(outcomes: &[String], prices: &[String]) -> Option<f64> {
    let mut idx = None;
    for (i, o) in outcomes.iter().enumerate() {
        if o.trim().eq_ignore_ascii_case("yes") {
            idx = Some(i);
            break;
        }
    }
    let i = idx?;
    prices.get(i).and_then(|p| parse_probability(p))
}

#[derive(Deserialize)]
struct PolyBookLevel {
    price: String,
    size: String,
}

#[derive(Deserialize)]
struct PolyBookMessage {
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    asset_id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    bids: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    asks: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    buys: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    sells: Option<Vec<PolyBookLevel>>,
}

struct PolyBookSnapshot {
    best_bid: Option<String>,
    best_ask: Option<String>,
    timestamp: Option<String>,
}

async fn fetch_polymarket_books_ws(
    token_ids: &[String],
    timeout_ms: u64,
) -> Result<std::collections::HashMap<String, PolyBookSnapshot>> {
    use tokio_tungstenite::tungstenite::Message;
    let mut out: std::collections::HashMap<String, PolyBookSnapshot> = std::collections::HashMap::new();
    if token_ids.is_empty() {
        return Ok(out);
    }

    let (mut ws, _) = connect_async("wss://ws-subscriptions-clob.polymarket.com/ws/market")
        .await
        .map_err(|e| Error::Provider(format!("polymarket ws connect failed: {e}")))?;

    let subscribe = serde_json::json!({
        "type": "market",
        "assets_ids": token_ids,
    });
    ws.send(Message::Text(subscribe.to_string()))
        .await
        .map_err(|e| Error::Provider(format!("polymarket ws subscribe failed: {e}")))?;

    let deadline = tokio::time::Instant::now() + TokioDuration::from_millis(timeout_ms.max(1));
    while out.len() < token_ids.len() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        let next = tokio::time::timeout(remaining, ws.next()).await;
        let Some(msg) = next.ok().and_then(|v| v.transpose().ok()).flatten() else {
            break;
        };
        if let Message::Text(text) = msg {
            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg: PolyBookMessage = match serde_json::from_value(parsed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg.event_type != "book" {
                continue;
            }
            let Some(asset_id) = msg.asset_id.clone() else {
                continue;
            };
            let bids = msg.bids.or(msg.buys).unwrap_or_default();
            let asks = msg.asks.or(msg.sells).unwrap_or_default();
            let best_bid = bids.first().map(|b| b.price.clone());
            let best_ask = asks.first().map(|a| a.price.clone());
            out.insert(
                asset_id,
                PolyBookSnapshot {
                    best_bid,
                    best_ask,
                    timestamp: msg.timestamp.clone(),
                },
            );
        }
    }

    let _ = ws.close(None).await;
    Ok(out)
}

async fn fetch_odds_kalshi(req: OddsRequest) -> Result<OddsResponse> {
    let client = reqwest::Client::builder().no_proxy().build().map_err(|e| {
        Error::Provider(format!("odds client init failed: {e}"))
    })?;

    // Handle list_series mode: return available series (optionally filtered)
    if req.list_series {
        #[derive(Deserialize)]
        struct SeriesListResp {
            series: Vec<RawSeriesEntry>,
        }
        #[derive(Deserialize)]
        struct RawSeriesEntry {
            ticker: String,
            title: String,
            category: Option<String>,
            frequency: Option<String>,
        }

        let url = format!("{}/series", KALSHI_BASE_URL);
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("kalshi series list failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "kalshi series list failed: http {}",
                resp.status()
            )));
        }

        let body: SeriesListResp = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("kalshi series list parse failed: {e}")))?;

        // Filter by category and/or search term
        let category_filter = req.category.as_deref().map(|s| s.trim().to_lowercase());
        let search_filter = req
            .search
            .as_deref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        let limit = req.limit.unwrap_or(50);

        let filtered: Vec<OddsSeries> = body
            .series
            .into_iter()
            .filter(|s| {
                if let Some(ref cat) = category_filter {
                    if let Some(ref sc) = s.category {
                        if !sc.to_lowercase().contains(cat) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                if let Some(ref search) = search_filter {
                    let title_match = s.title.to_lowercase().contains(search);
                    let ticker_match = s.ticker.to_lowercase().contains(search);
                    if !title_match && !ticker_match {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .map(|s| OddsSeries {
                ticker: s.ticker,
                title: s.title,
                category: s.category,
                frequency: s.frequency,
            })
            .collect();

        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at: Utc::now(),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor: None,
            available_series: Some(filtered),
            available_events: None,
            available_markets: None,
            available_tags: None,
            sources: None,
        });
    }

    // Handle list_events mode: return open events
    if req.list_events {
        #[derive(Deserialize)]
        struct EventsListResp {
            events: Vec<RawEventEntry>,
            #[serde(default)]
            cursor: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawEventEntry {
            event_ticker: String,
            title: String,
            category: Option<String>,
            series_ticker: Option<String>,
        }

        let search_filter = req
            .search
            .as_deref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        let mut limit = req.limit.unwrap_or(100);
        if search_filter.is_some() && req.limit.is_none() {
            limit = 200;
        }
        if limit < 1 {
            limit = 1;
        } else if limit > 200 {
            limit = 200;
        }
        let max_pages = match req.max_pages {
            Some(n) => n.max(1),
            None => {
                if search_filter.is_some() {
                    let target = 500usize;
                    (target + limit - 1) / limit
                } else {
                    1
                }
            }
        };
        let mut page = 0usize;
        let mut page_cursor = req.cursor.clone();
        let mut cursor: Option<String> = None;
        let status = req
            .status
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "open".to_string());

        let mut filtered: Vec<OddsListedEvent> = Vec::new();
        while page < max_pages {
            let mut query: Vec<(&str, String)> = vec![("status", status.clone()), ("limit", limit.to_string())];
            if let Some(ref cat) = req.category {
                let cat = cat.trim();
                if !cat.is_empty() {
                    query.push(("category", cat.to_string()));
                }
            }
            if let Some(ref c) = page_cursor {
                if !c.trim().is_empty() {
                    query.push(("cursor", c.trim().to_string()));
                }
            }

            let url = format!("{}/events", KALSHI_BASE_URL);
            let resp = client
                .get(&url)
                .query(&query)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi events list failed: {e}")))?;

            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "kalshi events list failed: http {}",
                    resp.status()
                )));
            }

            let body: EventsListResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi events list parse failed: {e}")))?;

            for e in body.events {
                if let Some(ref search) = search_filter {
                    let title_match = e.title.to_lowercase().contains(search);
                    let ticker_match = e.event_ticker.to_lowercase().contains(search);
                    if !title_match && !ticker_match {
                        continue;
                    }
                }
                filtered.push(OddsListedEvent {
                    ticker: e.event_ticker,
                    title: e.title,
                    category: e.category,
                    series_ticker: e.series_ticker,
                    source: Some("kalshi".to_string()),
                    event_id: None,
                    slug: None,
                    tags: None,
                });
            }

            page += 1;
            page_cursor = body.cursor.clone();
            cursor = body.cursor;
            if page_cursor.as_deref().unwrap_or("").is_empty() {
                break;
            }
        }

        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at: Utc::now(),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor,
            available_series: None,
            available_events: Some(filtered),
            available_markets: None,
            available_tags: None,
            sources: None,
        });
    }

    // Handle list_markets mode: return open markets
    if req.list_markets {
        #[derive(Deserialize)]
        struct MarketsListResp {
            markets: Vec<RawMarketEntry>,
            #[serde(default)]
            cursor: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawMarketEntry {
            ticker: String,
            title: String,
            event_ticker: String,
            #[serde(default, rename = "last_price", alias = "yes_price")]
            yes_price: Option<i64>,
            #[serde(default)]
            volume: Option<i64>,
            #[serde(default)]
            status: Option<String>,
        }

        let search_filter = req.search.as_deref().map(|s| s.trim().to_lowercase());
        let limit = req.limit.unwrap_or(100);
        let max_pages = req.max_pages.unwrap_or(1).max(1);
        let mut page = 0usize;
        let mut page_cursor = req.cursor.clone();
        let mut cursor: Option<String> = None;
        let status = req
            .status
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "open".to_string());

        let mut filtered: Vec<OddsListedMarket> = Vec::new();
        while page < max_pages {
            let mut query: Vec<(&str, String)> = vec![("status", status.clone()), ("limit", limit.to_string())];
            if let Some(ref series) = req.series_ticker {
                let series = series.trim();
                if !series.is_empty() {
                    query.push(("series_ticker", series.to_string()));
                }
            }
            if let Some(ref event) = req.event_ticker {
                let event = event.trim();
                if !event.is_empty() {
                    query.push(("event_ticker", event.to_string()));
                }
            }
            if let Some(ref c) = page_cursor {
                if !c.trim().is_empty() {
                    query.push(("cursor", c.trim().to_string()));
                }
            }

            let url = format!("{}/markets", KALSHI_BASE_URL);
            let resp = client
                .get(&url)
                .query(&query)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi markets list failed: {e}")))?;

            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "kalshi markets list failed: http {}",
                    resp.status()
                )));
            }

            let body: MarketsListResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi markets list parse failed: {e}")))?;

            for m in body.markets {
                if let Some(ref search) = search_filter {
                    let title_match = m.title.to_lowercase().contains(search);
                    let ticker_match = m.ticker.to_lowercase().contains(search);
                    if !title_match && !ticker_match {
                        continue;
                    }
                }
                filtered.push(OddsListedMarket {
                    ticker: m.ticker,
                    title: m.title,
                    event_ticker: m.event_ticker,
                    yes_price: m.yes_price,
                    volume: m.volume,
                    status: m.status,
                    source: Some("kalshi".to_string()),
                    market_id: None,
                    event_id: None,
                    slug: None,
                    outcomes: None,
                    outcome_prices: None,
                    clob_token_ids: None,
                    probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
                });
            }

            page += 1;
            page_cursor = body.cursor.clone();
            cursor = body.cursor;
            if page_cursor.as_deref().unwrap_or("").is_empty() {
                break;
            }
        }

        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at: Utc::now(),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor,
            available_series: None,
            available_events: None,
            available_markets: Some(filtered),
            available_tags: None,
            sources: None,
        });
    }

    if req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(Error::InvalidInput(
            "use --list-series, --list-events, --list-markets, or provide series/event/market ticker".to_string(),
        ));
    }

    if req.include_orderbook && req.market_ticker.as_deref().unwrap_or("").trim().is_empty() {
        return Err(Error::InvalidInput(
            "market_ticker is required when include_orderbook is true".to_string(),
        ));
    }

    let mut series: Option<OddsSeries> = None;
    let mut events: Vec<OddsEvent> = Vec::new();
    let mut markets: Vec<OddsMarket> = Vec::new();
    let mut cursor: Option<String> = None;

    if let Some(raw) = req.series_ticker.as_deref() {
        let ticker = raw.trim();
        if !ticker.is_empty() {
            #[derive(Deserialize)]
            struct SeriesResp {
                series: RawSeries,
            }
            #[derive(Deserialize)]
            struct RawSeries {
                ticker: String,
                title: String,
                category: Option<String>,
                frequency: Option<String>,
            }

            let url = format!("{}/series/{}", KALSHI_BASE_URL, ticker);
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi series fetch failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "kalshi series fetch failed: http {}",
                    resp.status()
                )));
            }
            let body: SeriesResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi series parse failed: {e}")))?;

            series = Some(OddsSeries {
                ticker: body.series.ticker,
                title: body.series.title,
                category: body.series.category,
                frequency: body.series.frequency,
            });

            #[derive(Deserialize)]
            struct MarketsResp {
                markets: Vec<RawMarket>,
                #[serde(default)]
                cursor: Option<String>,
            }
            #[derive(Deserialize)]
            struct RawMarket {
                ticker: String,
                title: String,
                event_ticker: String,
                #[serde(default)]
                status: Option<String>,
                #[serde(default, rename = "last_price", alias = "yes_price")]
                yes_price: Option<i64>,
                #[serde(default)]
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
            }

            // If an event_ticker is also provided, list markets by event instead of series.
            if req.event_ticker.as_deref().unwrap_or("").trim().is_empty() {
                let mut page = 0usize;
                let max_pages = req.max_pages.unwrap_or(1).max(1);
                let mut page_cursor = req.cursor.clone();

                while page < max_pages {
                    let mut query: Vec<(&str, String)> = vec![("series_ticker", ticker.to_string())];
                    if let Some(status) = req.status.as_ref().map(|s| s.trim().to_string()) {
                        if !status.is_empty() {
                            query.push(("status", status));
                        }
                    }
                    if let Some(limit) = req.limit {
                        query.push(("limit", limit.to_string()));
                    }
                    if let Some(c) = page_cursor.as_ref() {
                        if !c.trim().is_empty() {
                            query.push(("cursor", c.to_string()));
                        }
                    }

                    let url = format!("{}/markets", KALSHI_BASE_URL);
                    let resp = client
                        .get(url)
                        .query(&query)
                        .send()
                        .await
                        .map_err(|e| Error::Provider(format!("kalshi markets fetch failed: {e}")))?;
                    if !resp.status().is_success() {
                        return Err(Error::Provider(format!(
                            "kalshi markets fetch failed: http {}",
                            resp.status()
                        )));
                    }
                    let body: MarketsResp = resp
                        .json()
                        .await
                        .map_err(|e| Error::Provider(format!("kalshi markets parse failed: {e}")))?;

                    for m in body.markets {
                        markets.push(OddsMarket {
                            ticker: m.ticker,
                            title: m.title,
                            event_ticker: m.event_ticker,
                            status: m.status,
                            yes_price: m.yes_price,
                            yes_bid: m.yes_bid,
                            yes_ask: m.yes_ask,
                            volume: m.volume,
                            source: Some("kalshi".to_string()),
                            market_id: None,
                            event_id: None,
                            slug: None,
                            outcomes: None,
                            outcome_prices: None,
                            clob_token_ids: None,
                            probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
                            outcome_best_bids: None,
                            outcome_best_asks: None,
                            orderbook_timestamp: None,
                        });
                    }

                    page += 1;
                    page_cursor = body.cursor.clone();
                    cursor = body.cursor;
                    if page_cursor.as_deref().unwrap_or("").is_empty() {
                        break;
                    }
                }
            }
        }
    }

    if let Some(raw) = req.event_ticker.as_deref() {
        let ticker = raw.trim();
        if !ticker.is_empty() {
            #[derive(Deserialize)]
            struct EventResp {
                event: RawEvent,
            }
            #[derive(Deserialize)]
            struct RawEvent {
                #[serde(rename = "event_ticker")]
                ticker: String,
                title: String,
                category: Option<String>,
            }

            let url = format!("{}/events/{}", KALSHI_BASE_URL, ticker);
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi event fetch failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "kalshi event fetch failed: http {}",
                    resp.status()
                )));
            }
            let body: EventResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi event parse failed: {e}")))?;

            events.push(OddsEvent {
                ticker: body.event.ticker,
                title: body.event.title,
                category: body.event.category,
                source: Some("kalshi".to_string()),
                event_id: None,
                slug: None,
                tags: None,
            });

            // List markets for this event (unless series listing already covered, or user didn't ask for any listing).
            #[derive(Deserialize)]
            struct MarketsResp {
                markets: Vec<RawMarket>,
                #[serde(default)]
                cursor: Option<String>,
            }
            #[derive(Deserialize)]
            struct RawMarket {
                ticker: String,
                title: String,
                event_ticker: String,
                #[serde(default)]
                status: Option<String>,
                #[serde(default, rename = "last_price", alias = "yes_price")]
                yes_price: Option<i64>,
                #[serde(default)]
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
            }

            let mut page = 0usize;
            let max_pages = req.max_pages.unwrap_or(1).max(1);
            let mut page_cursor = req.cursor.clone();

            while page < max_pages {
                let mut query: Vec<(&str, String)> = vec![("event_ticker", ticker.to_string())];
                if let Some(status) = req.status.as_ref().map(|s| s.trim().to_string()) {
                    if !status.is_empty() {
                        query.push(("status", status));
                    }
                }
                if let Some(limit) = req.limit {
                    query.push(("limit", limit.to_string()));
                }
                if let Some(c) = page_cursor.as_ref() {
                    if !c.trim().is_empty() {
                        query.push(("cursor", c.to_string()));
                    }
                }

                let url = format!("{}/markets", KALSHI_BASE_URL);
                let resp = client
                    .get(url)
                    .query(&query)
                    .send()
                    .await
                    .map_err(|e| Error::Provider(format!("kalshi markets fetch failed: {e}")))?;
                if !resp.status().is_success() {
                    return Err(Error::Provider(format!(
                        "kalshi markets fetch failed: http {}",
                        resp.status()
                    )));
                }
                let body: MarketsResp = resp
                    .json()
                    .await
                    .map_err(|e| Error::Provider(format!("kalshi markets parse failed: {e}")))?;

                for m in body.markets {
                    markets.push(OddsMarket {
                        ticker: m.ticker,
                        title: m.title,
                        event_ticker: m.event_ticker,
                        status: m.status,
                        yes_price: m.yes_price,
                        yes_bid: m.yes_bid,
                        yes_ask: m.yes_ask,
                        volume: m.volume,
                        source: Some("kalshi".to_string()),
                        market_id: None,
                        event_id: None,
                        slug: None,
                        outcomes: None,
                        outcome_prices: None,
                        clob_token_ids: None,
                        probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
                        outcome_best_bids: None,
                        outcome_best_asks: None,
                        orderbook_timestamp: None,
                    });
                }

                page += 1;
                page_cursor = body.cursor.clone();
                cursor = body.cursor;
                if page_cursor.as_deref().unwrap_or("").is_empty() {
                    break;
                }
            }
        }
    }

    if let Some(raw) = req.market_ticker.as_deref() {
        let ticker = raw.trim();
        if !ticker.is_empty() {
            #[derive(Deserialize)]
            struct MarketResp {
                market: RawMarket,
            }
            #[derive(Deserialize)]
            struct RawMarket {
                ticker: String,
                title: String,
                event_ticker: String,
                #[serde(default)]
                status: Option<String>,
                #[serde(default, rename = "last_price", alias = "yes_price")]
                yes_price: Option<i64>,
                #[serde(default)]
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
            }

            let url = format!("{}/markets/{}", KALSHI_BASE_URL, ticker);
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi market fetch failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "kalshi market fetch failed: http {}",
                    resp.status()
                )));
            }
            let body: MarketResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi market parse failed: {e}")))?;

            let m = body.market;
            if !markets.iter().any(|existing| existing.ticker == m.ticker) {
                markets.push(OddsMarket {
                    ticker: m.ticker,
                    title: m.title,
                    event_ticker: m.event_ticker,
                    status: m.status,
                    yes_price: m.yes_price,
                    yes_bid: m.yes_bid,
                    yes_ask: m.yes_ask,
                    volume: m.volume,
                    source: Some("kalshi".to_string()),
                    market_id: None,
                    event_id: None,
                    slug: None,
                    outcomes: None,
                    outcome_prices: None,
                    clob_token_ids: None,
                    probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
                    outcome_best_bids: None,
                    outcome_best_asks: None,
                    orderbook_timestamp: None,
                });
            }
        }
    }

    let mut orderbook: Option<OddsOrderbook> = None;
    if req.include_orderbook {
        if let Some(raw) = req.market_ticker.as_deref() {
            let ticker = raw.trim();
            if !ticker.is_empty() {
                #[derive(Deserialize)]
                struct OrderbookResp {
                    orderbook: RawOrderbook,
                }
                #[derive(Deserialize)]
                struct RawOrderbook {
                    #[serde(default)]
                    yes: Option<Vec<[i64; 2]>>,
                    #[serde(default)]
                    no: Option<Vec<[i64; 2]>>,
                }

                let url = format!("{}/markets/{}/orderbook", KALSHI_BASE_URL, ticker);
                let resp = client
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| Error::Provider(format!("kalshi orderbook fetch failed: {e}")))?;
                if !resp.status().is_success() {
                    return Err(Error::Provider(format!(
                        "kalshi orderbook fetch failed: http {}",
                        resp.status()
                    )));
                }
                let body: OrderbookResp = resp
                    .json()
                    .await
                    .map_err(|e| Error::Provider(format!("kalshi orderbook parse failed: {e}")))?;

                let depth = req.orderbook_depth.unwrap_or(5).max(1);
                let yes = body
                    .orderbook
                    .yes
                    .unwrap_or_default()
                    .into_iter()
                    .take(depth)
                    .map(|pair| OddsOrderLevel {
                        price: pair[0],
                        quantity: pair[1],
                    })
                    .collect::<Vec<_>>();
                let no = body
                    .orderbook
                    .no
                    .unwrap_or_default()
                    .into_iter()
                    .take(depth)
                    .map(|pair| OddsOrderLevel {
                        price: pair[0],
                        quantity: pair[1],
                    })
                    .collect::<Vec<_>>();

                orderbook = Some(OddsOrderbook {
                    market_ticker: ticker.to_string(),
                    yes,
                    no,
                });
            }
        }
    }

    Ok(OddsResponse {
        base_url: KALSHI_BASE_URL.to_string(),
        generated_at: Utc::now(),
        series,
        events,
        markets,
        orderbook,
        cursor,
        available_series: None,
        available_events: None,
        available_markets: None,
        available_tags: None,
        sources: None,
    })
}

async fn fetch_odds_polymarket(req: &OddsRequest) -> Result<OddsResponse> {
    let client = reqwest::Client::builder().no_proxy().build().map_err(|e| {
        Error::Provider(format!("odds client init failed: {e}"))
    })?;

    #[derive(Deserialize)]
    #[derive(Clone)]
    struct PolyTag {
        id: serde_json::Value,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        slug: Option<String>,
    }

    #[derive(Deserialize)]
    struct PolyMarket {
        id: serde_json::Value,
        #[serde(default)]
        question: Option<String>,
        #[serde(default, rename = "clobTokenIds")]
        clob_token_ids: serde_json::Value,
        #[serde(default)]
        outcomes: serde_json::Value,
        #[serde(default, rename = "outcomePrices")]
        outcome_prices: serde_json::Value,
    }

    #[derive(Deserialize)]
    struct PolyEvent {
        id: serde_json::Value,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        active: Option<bool>,
        #[serde(default)]
        closed: Option<bool>,
        #[serde(default)]
        tags: Option<Vec<PolyTag>>,
        #[serde(default)]
        markets: Option<Vec<PolyMarket>>,
    }

    #[derive(Deserialize)]
    struct PolySearchMarket {
        id: serde_json::Value,
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        outcomes: serde_json::Value,
        #[serde(default, rename = "outcomePrices")]
        outcome_prices: serde_json::Value,
        #[serde(default, rename = "clobTokenIds")]
        clob_token_ids: serde_json::Value,
    }

    #[derive(Deserialize)]
    struct PolySearchEvent {
        id: serde_json::Value,
        #[serde(default)]
        ticker: Option<String>,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        category: Option<String>,
        #[serde(default)]
        active: Option<bool>,
        #[serde(default)]
        closed: Option<bool>,
        #[serde(default)]
        tags: Option<Vec<PolyTag>>,
        #[serde(default)]
        markets: Option<Vec<PolySearchMarket>>,
    }

    #[derive(Deserialize)]
    struct PolySearchPagination {
        #[serde(default, rename = "hasMore")]
        has_more: Option<bool>,
    }

    #[derive(Deserialize)]
    struct PolySearchResp {
        #[serde(default)]
        events: Option<Vec<PolySearchEvent>>,
        #[serde(default)]
        pagination: Option<PolySearchPagination>,
    }

    let search_filter = req
        .search
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

    if req.list_tags {
        let limit = req.limit.unwrap_or(100).max(1);
        let max_pages = match req.max_pages {
            Some(n) => n.max(1),
            None => {
                let target = 500usize;
                (target + limit - 1) / limit
            }
        };
        let mut offset = req
            .cursor
            .as_deref()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut page = 0usize;
        let mut has_more = false;
        let mut tags_out: Vec<OddsTag> = Vec::new();
        while page < max_pages {
            let url = format!(
                "{}/tags?limit={}&offset={}",
                POLYMARKET_GAMMA_URL, limit, offset
            );
            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("polymarket tags list failed: {e}")))?;

            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "polymarket tags list failed: http {}",
                    resp.status()
                )));
            }

            let raw: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("polymarket tags list parse failed: {e}")))?;

            let tags_value = if raw.is_array() {
                raw
            } else if let Some(data) = raw.get("data") {
                data.clone()
            } else if let Some(tags) = raw.get("tags") {
                tags.clone()
            } else if let Some(error) = raw.get("error").or_else(|| raw.get("message")) {
                return Err(Error::Provider(format!(
                    "polymarket tags list error: {}",
                    error
                )));
            } else {
                return Err(Error::Provider(
                    "polymarket tags list unexpected response".to_string(),
                ));
            };

            let page_tags: Vec<PolyTag> = serde_json::from_value(tags_value)
                .map_err(|e| Error::Provider(format!("polymarket tags list parse failed: {e}")))?;
            let raw_len = page_tags.len();
            tags_out.extend(page_tags.into_iter().map(|t| OddsTag {
                id: json_value_to_string(t.id),
                label: t.label,
                slug: t.slug,
            }));

            page += 1;
            if raw_len < limit {
                has_more = false;
                break;
            }
            offset = offset.saturating_add(limit);
            has_more = true;
        }

        return Ok(OddsResponse {
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            generated_at: Utc::now(),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor: if has_more { Some(offset.to_string()) } else { None },
            available_series: None,
            available_events: None,
            available_markets: None,
            available_tags: Some(tags_out),
            sources: None,
        });
    }

    if search_filter.is_some()
        && (req.list_events || req.list_markets)
        && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
    {
        let search_raw = req.search.as_deref().unwrap_or("").trim();
        let limit = req.limit.unwrap_or(100).max(1);
        let max_pages = match req.max_pages {
            Some(n) => n.max(1),
            None => {
                let target = 500usize;
                (target + limit - 1) / limit
            }
        };
        let mut page = req
            .cursor
            .as_deref()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1);
        if page < 1 {
            page = 1;
        }
        let status = req
            .status
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "all".to_string());

        let mut listed_events: Vec<OddsListedEvent> = Vec::new();
        let mut listed_markets: Vec<OddsListedMarket> = Vec::new();
        let mut pages_done = 0usize;
        let mut has_more = true;
        let mut next_cursor: Option<String> = None;

        while pages_done < max_pages && has_more {
            let query: Vec<(&str, String)> = vec![
                ("q", search_raw.to_string()),
                ("limit_per_type", limit.to_string()),
                ("page", page.to_string()),
                ("events_status", status.clone()),
                ("search_tags", "false".to_string()),
                ("search_profiles", "false".to_string()),
                ("optimized", "true".to_string()),
            ];

            let url = format!("{}/public-search", POLYMARKET_GAMMA_URL);
            let resp = client
                .get(&url)
                .query(&query)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("polymarket public-search failed: {e}")))?;

            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "polymarket public-search failed: http {}",
                    resp.status()
                )));
            }

            let body: PolySearchResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("polymarket public-search parse failed: {e}")))?;

            let events = body.events.unwrap_or_default();
            for event in events {
                let event_id = event
                    .ticker
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| json_value_to_string(event.id.clone()));
                let title = event.title.clone().unwrap_or_else(|| event_id.clone());
                let tags = event.tags.clone().map(|t| {
                    t.into_iter()
                        .map(|tag| OddsTag {
                            id: json_value_to_string(tag.id),
                            label: tag.label,
                            slug: tag.slug,
                        })
                        .collect::<Vec<_>>()
                });

                if req.list_events {
                    listed_events.push(OddsListedEvent {
                        ticker: event_id.clone(),
                        title: title.clone(),
                        category: event.category.clone(),
                        series_ticker: None,
                        source: Some("polymarket".to_string()),
                        event_id: Some(event_id.clone()),
                        slug: event.slug.clone(),
                        tags: tags.clone(),
                    });
                }

                let status = match (event.active, event.closed) {
                    (Some(true), Some(false)) => Some("open".to_string()),
                    (Some(false), Some(true)) => Some("closed".to_string()),
                    _ => None,
                };

                if req.list_markets {
                    if let Some(markets) = event.markets {
                        for m in markets {
                            let market_id = json_value_to_string(m.id);
                            let title = m.question.unwrap_or_else(|| market_id.clone());
                            let outcomes_vec = parse_json_value_strings(&m.outcomes);
                            let outcome_prices_vec = parse_json_value_strings(&m.outcome_prices);
                            let outcomes = if outcomes_vec.is_empty() { None } else { Some(outcomes_vec) };
                            let outcome_prices = if outcome_prices_vec.is_empty() {
                                None
                            } else {
                                Some(outcome_prices_vec)
                            };
                            let clob_token_ids_vec = parse_json_value_strings(&m.clob_token_ids);
                            let clob_token_ids = if clob_token_ids_vec.is_empty() {
                                None
                            } else {
                                Some(clob_token_ids_vec)
                            };
                            let probability_yes = match (outcomes.as_ref(), outcome_prices.as_ref()) {
                                (Some(o), Some(p)) => probability_yes_from_outcomes(o, p),
                                _ => None,
                            };

                            listed_markets.push(OddsListedMarket {
                                ticker: market_id.clone(),
                                title: title.clone(),
                                event_ticker: event_id.clone(),
                                yes_price: None,
                                volume: None,
                                status: status.clone(),
                                source: Some("polymarket".to_string()),
                                market_id: Some(market_id.clone()),
                                event_id: Some(event_id.clone()),
                                slug: m.slug.clone(),
                                outcomes: outcomes.clone(),
                                outcome_prices: outcome_prices.clone(),
                                clob_token_ids: clob_token_ids.clone(),
                                probability_yes,
                            });
                        }
                    }
                }
            }

            has_more = body
                .pagination
                .and_then(|p| p.has_more)
                .unwrap_or(false);
            pages_done += 1;
            if has_more {
                page += 1;
                next_cursor = Some(page.to_string());
            } else {
                next_cursor = None;
            }
        }

        return Ok(OddsResponse {
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            generated_at: Utc::now(),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor: next_cursor,
            available_series: None,
            available_events: if req.list_events { Some(listed_events) } else { None },
            available_markets: if req.list_markets { Some(listed_markets) } else { None },
            available_tags: None,
            sources: None,
        });
    }

    let limit = req.limit.unwrap_or(100).max(1);
    let max_pages = match req.max_pages {
        Some(n) => n.max(1),
        None => {
            let has_search = search_filter.is_some();
            if has_search {
                let target = 500usize;
                (target + limit - 1) / limit
            } else {
                1
            }
        }
    };
    let mut offset = req
        .cursor
        .as_deref()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let event_filter = req
        .event_ticker
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

    let mut events: Vec<PolyEvent> = Vec::new();
    let mut page = 0usize;
    while page < max_pages {
        let url = format!(
            "{}/events?active=true&closed=false&limit={}&offset={}",
            POLYMARKET_GAMMA_URL, limit, offset
        );
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("polymarket events list failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "polymarket events list failed: http {}",
                resp.status()
            )));
        }

        let raw: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("polymarket events list parse failed: {e}")))?;

        let events_value = if raw.is_array() {
            raw
        } else if let Some(data) = raw.get("data") {
            data.clone()
        } else if let Some(events_json) = raw.get("events") {
            events_json.clone()
        } else if let Some(error) = raw.get("error").or_else(|| raw.get("message")) {
            return Err(Error::Provider(format!(
                "polymarket events list error: {}",
                error
            )));
        } else {
            return Err(Error::Provider(
                "polymarket events list unexpected response".to_string(),
            ));
        };

        let mut page_events: Vec<PolyEvent> = serde_json::from_value(events_value)
            .map_err(|e| Error::Provider(format!("polymarket events list parse failed: {e}")))?;
        let raw_len = page_events.len();

        if let Some(ref search) = search_filter {
            page_events = page_events
                .into_iter()
                .filter(|e| {
                    let title = e.title.as_deref().unwrap_or("").to_lowercase();
                    let slug = e.slug.as_deref().unwrap_or("").to_lowercase();
                    title.contains(search) || slug.contains(search)
                })
                .collect();
        }

        if let Some(ref filter) = event_filter {
            page_events = page_events
                .into_iter()
                .filter(|e| {
                    let id = json_value_to_string(e.id.clone()).to_lowercase();
                    let slug = e.slug.as_deref().unwrap_or("").to_lowercase();
                    id == *filter || slug == *filter
                })
                .collect();
        }

        events.extend(page_events);

        page += 1;
        if raw_len < limit {
            break;
        }
        offset = offset.saturating_add(limit);
    }

    let mut odds_events: Vec<OddsEvent> = Vec::new();
    let mut odds_markets: Vec<OddsMarket> = Vec::new();
    let mut listed_events: Vec<OddsListedEvent> = Vec::new();
    let mut listed_markets: Vec<OddsListedMarket> = Vec::new();

    let list_events_only = req.list_events;
    let list_markets_only = req.list_markets;

    for event in events {
        let event_id = json_value_to_string(event.id);
        let title = event.title.unwrap_or_else(|| event_id.clone());
        let slug = event.slug.clone();
        let tags = event.tags.map(|t| {
            t.into_iter()
                .map(|tag| OddsTag {
                    id: json_value_to_string(tag.id),
                    label: tag.label,
                    slug: tag.slug,
                })
                .collect::<Vec<_>>()
        });

        if list_events_only {
            listed_events.push(OddsListedEvent {
                ticker: event_id.clone(),
                title: title.clone(),
                category: None,
                series_ticker: None,
                source: Some("polymarket".to_string()),
                event_id: Some(event_id.clone()),
                slug: slug.clone(),
                tags: tags.clone(),
            });
        } else {
            odds_events.push(OddsEvent {
                ticker: event_id.clone(),
                title: title.clone(),
                category: None,
                source: Some("polymarket".to_string()),
                event_id: Some(event_id.clone()),
                slug: slug.clone(),
                tags: tags.clone(),
            });
        }

        let status = match (event.active, event.closed) {
            (Some(true), Some(false)) => Some("open".to_string()),
            (Some(false), Some(true)) => Some("closed".to_string()),
            _ => None,
        };

        if let Some(markets) = event.markets {
            for m in markets {
                let market_id = json_value_to_string(m.id);
                let title = m.question.unwrap_or_else(|| market_id.clone());
                let outcomes_vec = parse_json_value_strings(&m.outcomes);
                let outcome_prices_vec = parse_json_value_strings(&m.outcome_prices);
                let outcomes = if outcomes_vec.is_empty() { None } else { Some(outcomes_vec) };
                let outcome_prices = if outcome_prices_vec.is_empty() { None } else { Some(outcome_prices_vec) };
                let clob_token_ids_vec = parse_json_value_strings(&m.clob_token_ids);
                let clob_token_ids = if clob_token_ids_vec.is_empty() { None } else { Some(clob_token_ids_vec) };
                let probability_yes = match (outcomes.as_ref(), outcome_prices.as_ref()) {
                    (Some(o), Some(p)) => probability_yes_from_outcomes(o, p),
                    _ => None,
                };

                if list_markets_only {
                    listed_markets.push(OddsListedMarket {
                        ticker: market_id.clone(),
                        title: title.clone(),
                        event_ticker: event_id.clone(),
                        yes_price: None,
                        volume: None,
                        status: status.clone(),
                        source: Some("polymarket".to_string()),
                        market_id: Some(market_id.clone()),
                        event_id: Some(event_id.clone()),
                        slug: None,
                        outcomes: outcomes.clone(),
                        outcome_prices: outcome_prices.clone(),
                        clob_token_ids: clob_token_ids.clone(),
                        probability_yes,
                    });
                } else {
                    odds_markets.push(OddsMarket {
                        ticker: market_id.clone(),
                        title: title.clone(),
                        event_ticker: event_id.clone(),
                        status: status.clone(),
                        yes_price: None,
                        yes_bid: None,
                        yes_ask: None,
                        volume: None,
                        source: Some("polymarket".to_string()),
                        market_id: Some(market_id.clone()),
                        event_id: Some(event_id.clone()),
                        slug: None,
                        outcomes: outcomes.clone(),
                        outcome_prices: outcome_prices.clone(),
                        clob_token_ids: clob_token_ids.clone(),
                        probability_yes,
                        outcome_best_bids: None,
                        outcome_best_asks: None,
                        orderbook_timestamp: None,
                    });
                }
            }
        }
    }

    if let Some(ref market_filter) = req.market_ticker {
        let filter = market_filter.trim().to_lowercase();
        if !filter.is_empty() {
            odds_markets.retain(|m| {
                let id = m.market_id.as_deref().unwrap_or(&m.ticker).to_lowercase();
                id == filter
            });
            listed_markets.retain(|m| {
                let id = m.market_id.as_deref().unwrap_or(&m.ticker).to_lowercase();
                id == filter
            });
        }
    }

    if req.include_orderbook {
        let mut token_ids = Vec::new();
        for market in &odds_markets {
            if let Some(tokens) = market.clob_token_ids.as_ref() {
                for token in tokens {
                    if !token_ids.contains(token) {
                        token_ids.push(token.clone());
                    }
                }
            }
        }

        if !token_ids.is_empty() {
            let books = fetch_polymarket_books_ws(&token_ids, 3000).await?;
            for market in &mut odds_markets {
                if let Some(tokens) = market.clob_token_ids.as_ref() {
                    let mut bids = Vec::new();
                    let mut asks = Vec::new();
                    let mut timestamp = None;
                    for token in tokens {
                        if let Some(book) = books.get(token) {
                            bids.push(book.best_bid.clone().unwrap_or_default());
                            asks.push(book.best_ask.clone().unwrap_or_default());
                            if timestamp.is_none() {
                                timestamp = book.timestamp.clone();
                            }
                        } else {
                            bids.push(String::new());
                            asks.push(String::new());
                        }
                    }
                    market.outcome_best_bids = Some(bids.clone());
                    market.outcome_best_asks = Some(asks.clone());
                    market.orderbook_timestamp = timestamp;

                    if let (Some(outcomes), Some(bids), Some(asks)) = (
                        market.outcomes.as_ref(),
                        market.outcome_best_bids.as_ref(),
                        market.outcome_best_asks.as_ref(),
                    ) {
                        if let Some(idx) = outcomes
                            .iter()
                            .position(|o| o.trim().eq_ignore_ascii_case("yes"))
                        {
                            let bid = bids.get(idx).and_then(|v| parse_probability(v));
                            let ask = asks.get(idx).and_then(|v| parse_probability(v));
                            if let (Some(b), Some(a)) = (bid, ask) {
                                market.probability_yes = Some((b + a) / 2.0);
                            } else if let Some(b) = bid {
                                market.probability_yes = Some(b);
                            } else if let Some(a) = ask {
                                market.probability_yes = Some(a);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(OddsResponse {
        base_url: POLYMARKET_GAMMA_URL.to_string(),
        generated_at: Utc::now(),
        series: None,
        events: odds_events,
        markets: odds_markets,
        orderbook: None,
        cursor: None,
        available_series: None,
        available_events: if list_events_only { Some(listed_events) } else { None },
        available_markets: if list_markets_only { Some(listed_markets) } else { None },
        available_tags: None,
        sources: None,
    })
}

pub async fn fetch_odds(req: OddsRequest) -> Result<OddsResponse> {
    let mut provider = req
        .provider
        .as_deref()
        .unwrap_or("kalshi")
        .trim()
        .to_ascii_lowercase();

    if req.list_tags {
        if provider == "kalshi" {
            return Err(Error::InvalidInput(
                "list_tags is only supported for polymarket (use --provider polymarket or auto)"
                    .to_string(),
            ));
        }
        if provider == "auto" {
            provider = "polymarket".to_string();
        }
    }

    if req.disable_kalshi {
        if req.list_series {
            return Err(Error::InvalidInput(
                "list_series requires kalshi, but kalshi is disabled".to_string(),
            ));
        }
        if provider == "kalshi" || provider == "auto" {
            provider = "polymarket".to_string();
        }
    }

    if req.list_series {
        if provider == "polymarket" {
            return Err(Error::InvalidInput(
                "list_series is only supported for kalshi (use --provider kalshi or omit --provider)".to_string(),
            ));
        }
        let mut resp = fetch_odds_kalshi(req).await?;
        resp.sources = Some(vec![OddsSourceInfo {
            source: "kalshi".to_string(),
            base_url: KALSHI_BASE_URL.to_string(),
            ok: true,
            error: None,
        }]);
        return Ok(resp);
    }

    if !req.list_events
        && !req.list_markets
        && !req.list_tags
        && req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(Error::InvalidInput(
            "use --list-events, --list-markets, or provide series/event/market ticker".to_string(),
        ));
    }

    if req.include_orderbook && req.market_ticker.as_deref().unwrap_or("").trim().is_empty() {
        return Err(Error::InvalidInput(
            "market_ticker is required when include_orderbook is true".to_string(),
        ));
    }

    if provider == "polymarket" {
        let mut poly = fetch_odds_polymarket(&req).await?;
        poly.sources = Some(vec![OddsSourceInfo {
            source: "polymarket".to_string(),
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            ok: true,
            error: None,
        }]);
        return Ok(poly);
    }

    if provider == "auto" {
        let mut sources = Vec::new();
        let kalshi_result = fetch_odds_kalshi(req.clone()).await;
        match kalshi_result {
            Ok(mut kalshi) => {
                sources.push(OddsSourceInfo {
                    source: "kalshi".to_string(),
                    base_url: KALSHI_BASE_URL.to_string(),
                    ok: true,
                    error: None,
                });
                let has_events = kalshi.available_events.as_ref().is_some_and(|v| !v.is_empty())
                    || !kalshi.events.is_empty();
                let has_markets = kalshi.available_markets.as_ref().is_some_and(|v| !v.is_empty())
                    || !kalshi.markets.is_empty();
                let has_series = kalshi.series.is_some();

                let found = if req.list_events {
                    has_events
                } else if req.list_markets {
                    has_markets
                } else if req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
                    && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
                    && req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
                {
                    has_events || has_markets
                } else if !req.market_ticker.as_deref().unwrap_or("").trim().is_empty() {
                    has_markets || kalshi.orderbook.is_some()
                } else if !req.event_ticker.as_deref().unwrap_or("").trim().is_empty() {
                    has_events || has_markets
                } else {
                    has_series || has_markets
                };

                if found {
                    kalshi.sources = Some(sources);
                    return Ok(kalshi);
                }

                let mut poly = fetch_odds_polymarket(&req).await?;
                sources.push(OddsSourceInfo {
                    source: "polymarket".to_string(),
                    base_url: POLYMARKET_GAMMA_URL.to_string(),
                    ok: true,
                    error: None,
                });
                poly.sources = Some(sources);
                return Ok(poly);
            }
            Err(e) => {
                let msg = e.to_string();
                sources.push(OddsSourceInfo {
                    source: "kalshi".to_string(),
                    base_url: KALSHI_BASE_URL.to_string(),
                    ok: false,
                    error: Some(msg),
                });
            }
        }

        let mut poly = fetch_odds_polymarket(&req).await?;
        sources.push(OddsSourceInfo {
            source: "polymarket".to_string(),
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            ok: true,
            error: None,
        });
        poly.sources = Some(sources);
        return Ok(poly);
    }

    let mut kalshi = fetch_odds_kalshi(req).await?;
    kalshi.sources = Some(vec![OddsSourceInfo {
        source: "kalshi".to_string(),
        base_url: KALSHI_BASE_URL.to_string(),
        ok: true,
        error: None,
    }]);
    Ok(kalshi)
}

const YAHOO_OPTIONS_URL: &str = "https://query2.finance.yahoo.com/v7/finance/options";
const YAHOO_CRUMB_URL: &str = "https://query2.finance.yahoo.com/v1/test/getcrumb";

async fn yahoo_lookup_quote_type(client: &reqwest::Client, ticker: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(YAHOO_SEARCH_URL).ok()?;
    url.query_pairs_mut()
        .append_pair("q", ticker)
        .append_pair("quotesCount", "8")
        .append_pair("newsCount", "0");

    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let json: serde_json::Value = resp.json().await.ok()?;
    let quotes = json["quotes"].as_array()?;

    let mut fallback: Option<&serde_json::Value> = None;
    for q in quotes {
        if fallback.is_none() {
            fallback = Some(q);
        }
        if q["symbol"]
            .as_str()
            .map(|s| s.eq_ignore_ascii_case(ticker))
            .unwrap_or(false)
        {
            return q["quoteType"].as_str().map(|s| s.to_string());
        }
    }

    fallback.and_then(|q| q["quoteType"].as_str().map(|s| s.to_string()))
}

pub async fn fetch_options(req: OptionsRequest) -> Result<OptionsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    // Build client with cookie store for Yahoo auth
    let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .cookie_provider(jar.clone())
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko)")
        .build()
        .map_err(|e| Error::Provider(format!("http client init failed: {e}")))?;

    if let Some(quote_type) = yahoo_lookup_quote_type(&client, &ticker).await {
        let quote_type_norm = quote_type.trim().to_ascii_uppercase();
        if matches!(quote_type_norm.as_str(), "INDEX" | "MUTUALFUND") {
            let error = ToolErrorInfo {
                error: "AssetTypeMismatch".to_string(),
                message: format!(
                    "Ticker '{ticker}' is type '{quote_type_norm}'. This provider does not support options chains for this asset class."
                ),
                hint: Some(
                    "Use a tradable instrument that lists options for this asset class."
                        .to_string(),
                ),
            };
            return Ok(OptionsResponse {
                ticker,
                underlying_price: 0.0,
                generated_at: Utc::now(),
                status: Some("error".to_string()),
                error: Some(error),
                expirations: vec![],
                selected_expiry: None,
                calls: vec![],
                puts: vec![],
                metrics: None,
                note: None,
                multi_expiry_summary: None,
            });
        }
    }

    // First hit fc.yahoo.com to initialize cookies (required for crumb auth)
    let _ = client.get("https://fc.yahoo.com").send().await;

    // Get crumb for Yahoo API auth
    let crumb_resp = client
        .get(YAHOO_CRUMB_URL)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("yahoo crumb fetch failed: {e}")))?;

    if !crumb_resp.status().is_success() {
        return Err(Error::Provider(format!(
            "yahoo crumb fetch failed: http {}",
            crumb_resp.status()
        )));
    }

    let crumb = crumb_resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("yahoo crumb read failed: {e}")))?;

    // Fetch options data with crumb
    let base_url = format!("{}/{}?crumb={}", YAHOO_OPTIONS_URL, ticker, crumb);
    let resp = client
        .get(&base_url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("yahoo options fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "yahoo options fetch failed: http {} for {}",
            resp.status(),
            ticker
        )));
    }

    #[derive(Deserialize)]
    struct YahooOptionsResp {
        #[serde(rename = "optionChain")]
        option_chain: YahooOptionChain,
    }

    #[derive(Deserialize)]
    struct YahooOptionChain {
        result: Vec<YahooChainResult>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct YahooChainResult {
        underlying_symbol: Option<String>,
        expiration_dates: Option<Vec<i64>>,
        quote: Option<YahooQuote>,
        options: Option<Vec<YahooOptions>>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct YahooQuote {
        regular_market_price: Option<f64>,
    }

    #[derive(Deserialize)]
    struct YahooOptions {
        #[serde(rename = "expirationDate")]
        expiration_date: i64,
        calls: Option<Vec<YahooContract>>,
        puts: Option<Vec<YahooContract>>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct YahooContract {
        contract_symbol: String,
        strike: f64,
        expiration: i64,
        bid: Option<f64>,
        ask: Option<f64>,
        last_price: Option<f64>,
        change: Option<f64>,
        percent_change: Option<f64>,
        volume: Option<i64>,
        open_interest: Option<i64>,
        implied_volatility: Option<f64>,
        in_the_money: Option<bool>,
    }

    let body: YahooOptionsResp = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("yahoo options parse failed: {e}")))?;

    let chain = body
        .option_chain
        .result
        .into_iter()
        .next()
        .ok_or_else(|| Error::Provider(format!("no options data for {}", ticker)))?;

    let underlying_price = chain
        .quote
        .and_then(|q| q.regular_market_price)
        .unwrap_or(0.0);

    // Convert expiration timestamps to dates
    let expiration_timestamps_raw = chain.expiration_dates.clone().unwrap_or_default();
    let expirations: Vec<String> = expiration_timestamps_raw
        .iter()
        .filter_map(|&ts| {
            Utc.timestamp_opt(ts, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d").to_string())
        })
        .collect();

    let note = if expirations.is_empty() {
        Some(
            "No listed options expirations returned for this symbol. Some symbols (e.g., futures/indices) may not have options here; try an equity/ETF proxy or use `--expirations` on a different ticker."
                .to_string(),
        )
    } else {
        None
    };

    // If only listing expirations, return early
    if req.list_expirations {
        return Ok(OptionsResponse {
            ticker,
            underlying_price,
            generated_at: Utc::now(),
            status: None,
            error: None,
            expirations,
            selected_expiry: None,
            calls: vec![],
            puts: vec![],
            metrics: None,
            note,
            multi_expiry_summary: None,
        });
    }

    // Multi-expiry mode: fetch summary for multiple expirations
    if req.multi_expiry {
        let num_expiries = req.num_expiries.unwrap_or(3).min(5);
        let expiration_timestamps: Vec<i64> = chain
            .expiration_dates
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .take(num_expiries)
            .copied()
            .collect();

        let mut snapshots: Vec<ExpirySnapshot> = Vec::new();
        let mut aggregate_volume: u64 = 0;
        let mut weighted_pc_sum: f64 = 0.0;
        let mut first_pc_ratio: Option<f64> = None;

        for exp_ts in expiration_timestamps {
            let url = format!("{}/{}?crumb={}&date={}", YAHOO_OPTIONS_URL, ticker, crumb, exp_ts);
            let resp = client.get(&url).send().await;

            if let Ok(resp) = resp {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<YahooOptionsResp>().await {
                        if let Some(chain_result) = body.option_chain.result.into_iter().next() {
                            if let Some(opts) = chain_result.options.and_then(|o| o.into_iter().next()) {
                                let calls = opts.calls.unwrap_or_default();
                                let puts = opts.puts.unwrap_or_default();

                                let call_vol: u64 = calls.iter().filter_map(|c| c.volume).map(|v| v as u64).sum();
                                let put_vol: u64 = puts.iter().filter_map(|p| p.volume).map(|v| v as u64).sum();
                                let call_oi: u64 = calls.iter().filter_map(|c| c.open_interest).map(|v| v as u64).sum();
                                let put_oi: u64 = puts.iter().filter_map(|p| p.open_interest).map(|v| v as u64).sum();

                                let pc_vol = if call_vol > 0 { put_vol as f64 / call_vol as f64 } else { 0.0 };
                                let pc_oi = if call_oi > 0 { put_oi as f64 / call_oi as f64 } else { 0.0 };

                                let total_vol = call_vol + put_vol;

                                // Max pain calculation
                                let mut strike_oi: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
                                for c in &calls {
                                    let strike_cents = (c.strike * 100.0).round() as i64;
                                    *strike_oi.entry(strike_cents).or_insert(0) += c.open_interest.unwrap_or(0) as u64;
                                }
                                for p in &puts {
                                    let strike_cents = (p.strike * 100.0).round() as i64;
                                    *strike_oi.entry(strike_cents).or_insert(0) += p.open_interest.unwrap_or(0) as u64;
                                }
                                let max_pain = strike_oi
                                    .into_iter()
                                    .max_by_key(|(_, oi)| *oi)
                                    .map(|(strike_cents, _)| strike_cents as f64 / 100.0);

                                // ATM IV
                                let atm_iv = calls
                                    .iter()
                                    .min_by(|a, b| {
                                        (a.strike - underlying_price).abs()
                                            .partial_cmp(&(b.strike - underlying_price).abs())
                                            .unwrap_or(std::cmp::Ordering::Equal)
                                    })
                                    .and_then(|c| c.implied_volatility);

                                let expiry_date = Utc.timestamp_opt(exp_ts, 0)
                                    .single()
                                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                                    .unwrap_or_default();

                                let days_to_expiry = Utc.timestamp_opt(exp_ts, 0)
                                    .single()
                                    .map(|dt| (dt - Utc::now()).num_days())
                                    .unwrap_or(0);

                                if first_pc_ratio.is_none() && total_vol > 0 {
                                    first_pc_ratio = Some(pc_vol);
                                }

                                aggregate_volume += total_vol;
                                weighted_pc_sum += pc_vol * total_vol as f64;

                                snapshots.push(ExpirySnapshot {
                                    expiry: expiry_date,
                                    days_to_expiry,
                                    total_volume: total_vol,
                                    total_oi: call_oi + put_oi,
                                    put_call_ratio_volume: pc_vol,
                                    put_call_ratio_oi: pc_oi,
                                    max_pain,
                                    atm_iv,
                                });
                            }
                        }
                    }
                }
            }

            // Rate limit
            tokio::time::sleep(StdDuration::from_millis(100)).await;
        }

        let weighted_put_call_ratio = if aggregate_volume > 0 {
            weighted_pc_sum / aggregate_volume as f64
        } else {
            0.0
        };

        let near_term_bias = match first_pc_ratio {
            Some(pc) if pc < 0.7 => "bullish".to_string(),
            Some(pc) if pc > 1.3 => "bearish".to_string(),
            _ => "neutral".to_string(),
        };

        let multi_summary = MultiExpirySummary {
            snapshots,
            aggregate_volume,
            weighted_put_call_ratio,
            near_term_bias,
        };

        return Ok(OptionsResponse {
            ticker,
            underlying_price,
            generated_at: Utc::now(),
            status: None,
            error: None,
            expirations,
            selected_expiry: None,
            calls: vec![],
            puts: vec![],
            metrics: None,
            note,
            multi_expiry_summary: Some(multi_summary),
        });
    }

    // Determine which expiry to fetch
    let target_expiry_ts: Option<i64> = if let Some(exp_str) = req.expiry.as_deref() {
        // Parse user-provided expiry date
        let date = chrono::NaiveDate::parse_from_str(exp_str.trim(), "%Y-%m-%d")
            .map_err(|_| Error::InvalidInput(format!("invalid expiry date: {exp_str}")))?;
        let dt = DateTime::<Utc>::from_naive_utc_and_offset(
            date.and_hms_opt(0, 0, 0).unwrap(),
            Utc,
        );
        Some(dt.timestamp())
    } else {
        None
    };

    // Fetch specific expiry if requested (different from first fetch)
    let options_data = if let Some(ts) = target_expiry_ts {
        let url = format!("{}/{}?crumb={}&date={}", YAHOO_OPTIONS_URL, ticker, crumb, ts);
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("yahoo options expiry fetch failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "yahoo options expiry fetch failed: http {}",
                resp.status()
            )));
        }

        let body: YahooOptionsResp = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("yahoo options expiry parse failed: {e}")))?;

        body.option_chain
            .result
            .into_iter()
            .next()
            .and_then(|r| r.options)
            .and_then(|o| o.into_iter().next())
    } else {
        chain.options.and_then(|o| o.into_iter().next())
    };

    let (raw_calls, raw_puts, selected_expiry) = match options_data {
        Some(opts) => {
            let exp_date = Utc
                .timestamp_opt(opts.expiration_date, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d").to_string());
            (
                opts.calls.unwrap_or_default(),
                opts.puts.unwrap_or_default(),
                exp_date,
            )
        }
        None => (vec![], vec![], None),
    };

    // Convert Yahoo contracts to our format
    let convert_contract = |c: YahooContract, opt_type: &str| -> OptionContract {
        let expiry = Utc
            .timestamp_opt(c.expiration, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default();

        OptionContract {
            contract_symbol: c.contract_symbol,
            strike: c.strike,
            expiry,
            option_type: opt_type.to_string(),
            bid: c.bid.unwrap_or(0.0),
            ask: c.ask.unwrap_or(0.0),
            last: c.last_price.unwrap_or(0.0),
            change: c.change.unwrap_or(0.0),
            pct_change: c.percent_change.unwrap_or(0.0),
            volume: c.volume.unwrap_or(0) as u64,
            open_interest: c.open_interest.unwrap_or(0) as u64,
            implied_volatility: c.implied_volatility.unwrap_or(0.0),
            in_the_money: c.in_the_money.unwrap_or(false),
        }
    };

    let mut calls: Vec<OptionContract> = raw_calls
        .into_iter()
        .map(|c| convert_contract(c, "call"))
        .collect();

    let mut puts: Vec<OptionContract> = raw_puts
        .into_iter()
        .map(|c| convert_contract(c, "put"))
        .collect();

    // Filter by option type if specified
    if let Some(ref opt_type) = req.option_type {
        let t = opt_type.trim().to_lowercase();
        if t == "calls" || t == "call" {
            puts.clear();
        } else if t == "puts" || t == "put" {
            calls.clear();
        }
    }

    // Filter by near-money percentage if specified
    if let Some(pct) = req.near_money_pct {
        if underlying_price > 0.0 && pct > 0.0 {
            let low = underlying_price * (1.0 - pct / 100.0);
            let high = underlying_price * (1.0 + pct / 100.0);
            calls.retain(|c| c.strike >= low && c.strike <= high);
            puts.retain(|p| p.strike >= low && p.strike <= high);
        }
    }

    // Calculate metrics
    let total_call_volume: u64 = calls.iter().map(|c| c.volume).sum();
    let total_put_volume: u64 = puts.iter().map(|p| p.volume).sum();
    let total_call_oi: u64 = calls.iter().map(|c| c.open_interest).sum();
    let total_put_oi: u64 = puts.iter().map(|p| p.open_interest).sum();

    let put_call_ratio_volume = if total_call_volume > 0 {
        total_put_volume as f64 / total_call_volume as f64
    } else {
        0.0
    };

    let put_call_ratio_oi = if total_call_oi > 0 {
        total_put_oi as f64 / total_call_oi as f64
    } else {
        0.0
    };

    // Find ATM options (closest to underlying price)
    let atm_iv_call = calls
        .iter()
        .min_by(|a, b| {
            (a.strike - underlying_price)
                .abs()
                .partial_cmp(&(b.strike - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|c| c.implied_volatility);

    let atm_iv_put = puts
        .iter()
        .min_by(|a, b| {
            (a.strike - underlying_price)
                .abs()
                .partial_cmp(&(b.strike - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|p| p.implied_volatility);

    // Calculate max pain (strike with highest total OI)
    let mut strike_oi: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
    for c in &calls {
        let strike_cents = (c.strike * 100.0).round() as i64;
        *strike_oi.entry(strike_cents).or_insert(0) += c.open_interest;
    }
    for p in &puts {
        let strike_cents = (p.strike * 100.0).round() as i64;
        *strike_oi.entry(strike_cents).or_insert(0) += p.open_interest;
    }
    let max_pain = strike_oi
        .into_iter()
        .max_by_key(|(_, oi)| *oi)
        .map(|(strike_cents, _)| strike_cents as f64 / 100.0);

    let metrics = Some(OptionsMetrics {
        underlying_price,
        put_call_ratio_volume,
        put_call_ratio_oi,
        total_call_volume,
        total_put_volume,
        total_call_oi,
        total_put_oi,
        atm_iv_call,
        atm_iv_put,
        max_pain,
    });

    // If summary only, clear the chains
    let (final_calls, final_puts) = if req.summary_only {
        (vec![], vec![])
    } else {
        (calls, puts)
    };

    let note = if note.is_some() {
        note
    } else if selected_expiry.is_none() {
        Some("No options chain returned for the requested expiry. Use `--expirations` to see valid dates.".to_string())
    } else {
        None
    };

    Ok(OptionsResponse {
        ticker,
        underlying_price,
        generated_at: Utc::now(),
        status: None,
        error: None,
        expirations,
        selected_expiry,
        calls: final_calls,
        puts: final_puts,
        metrics,
        note,
        multi_expiry_summary: None,
    })
}

pub async fn fetch_insider(req: InsiderRequest, cache_dir: &Path) -> Result<InsiderResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    let days = req.days.unwrap_or(90);
    let limit = req.limit.unwrap_or(50).clamp(1, 200);
    let cutoff_date = Utc::now() - Duration::days(days as i64);

    // Reuse existing SEC infrastructure
    let sec_dir = cache_dir.join("finance").join("sec");
    std::fs::create_dir_all(&sec_dir)?;

    let (cik_str, company_name) = sec_lookup_cik(&ticker, &sec_dir, req.user_agent.as_deref()).await?;
    let submissions = sec_fetch_submissions(&cik_str, &company_name, &sec_dir, req.user_agent.as_deref()).await?;

    let recent = submissions
        .filings
        .as_ref()
        .and_then(|f| f.recent.as_ref())
        .ok_or_else(|| Error::Provider(format!("sec submissions missing recent filings for '{ticker}'")))?;

    let n = recent.form.len();
    let cik_num = submissions
        .cik
        .trim_start_matches('0')
        .parse::<u64>()
        .unwrap_or_else(|_| submissions.cik.parse::<u64>().unwrap_or(0));

    let client = sec_client(req.user_agent.as_deref())?;
    let mut transactions: Vec<InsiderTransaction> = Vec::new();
    let mut insiders_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for i in 0..n {
        if transactions.len() >= limit {
            break;
        }

        let form = recent.form.get(i).cloned().unwrap_or_default();
        if form != "4" {
            continue;
        }

        let filing_date_str = recent.filing_date.get(i).cloned().unwrap_or_default();
        if filing_date_str.is_empty() {
            continue;
        }

        // Parse filing date and check against cutoff
        if let Ok(filing_date) = chrono::NaiveDate::parse_from_str(&filing_date_str, "%Y-%m-%d") {
            let filing_dt = DateTime::<Utc>::from_naive_utc_and_offset(
                filing_date.and_hms_opt(0, 0, 0).unwrap_or_default(),
                Utc,
            );
            if filing_dt < cutoff_date {
                continue;
            }
        }

        let accession = recent.accession_number.get(i).cloned().unwrap_or_default();
        if accession.is_empty() {
            continue;
        }

        let primary_doc = recent
            .primary_document
            .as_ref()
            .and_then(|v| v.get(i).cloned())
            .unwrap_or_default();

        // Find the XML file (primary doc often points to xslF345X05/something.xml)
        let accession_nodash = accession.replace('-', "");
        let xml_filename = if primary_doc.contains(".xml") {
            // Extract just the XML filename from paths like "xslF345X05/file.xml"
            primary_doc
                .split('/')
                .last()
                .unwrap_or(&primary_doc)
                .to_string()
        } else {
            // Try to find an XML file in the filing index
            format!("primary_doc.xml")
        };

        let xml_url = format!(
            "https://www.sec.gov/Archives/edgar/data/{}/{}/{}",
            cik_num, accession_nodash, xml_filename
        );

        // Fetch and parse Form 4 XML
        match fetch_form4_xml(&client, &xml_url).await {
            Ok(form4_txns) => {
                for txn in form4_txns {
                    insiders_seen.insert(txn.insider_name.clone());
                    let mut txn_with_filing_date = txn;
                    txn_with_filing_date.filing_date = filing_date_str.clone();
                    transactions.push(txn_with_filing_date);
                    if transactions.len() >= limit {
                        break;
                    }
                }
            }
            Err(_) => {
                // Try alternate XML path from filing index
                let index_url = format!(
                    "https://www.sec.gov/Archives/edgar/data/{}/{}/index.json",
                    cik_num, accession_nodash
                );
                if let Ok(xml_path) = find_form4_xml_from_index(&client, &index_url).await {
                    let alt_url = format!(
                        "https://www.sec.gov/Archives/edgar/data/{}/{}/{}",
                        cik_num, accession_nodash, xml_path
                    );
                    if let Ok(form4_txns) = fetch_form4_xml(&client, &alt_url).await {
                        for txn in form4_txns {
                            insiders_seen.insert(txn.insider_name.clone());
                            let mut txn_with_filing_date = txn;
                            txn_with_filing_date.filing_date = filing_date_str.clone();
                            transactions.push(txn_with_filing_date);
                            if transactions.len() >= limit {
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Rate limit: be nice to SEC
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }

    // Compute summary
    let mut buy_count = 0u32;
    let mut sell_count = 0u32;
    let mut buy_shares = 0.0f64;
    let mut sell_shares = 0.0f64;
    let mut buy_value = 0.0f64;
    let mut sell_value = 0.0f64;

    for txn in &transactions {
        let val = txn.value.unwrap_or(0.0);
        match txn.transaction_code.as_str() {
            "P" => {
                buy_count += 1;
                buy_shares += txn.shares;
                buy_value += val;
            }
            "S" => {
                sell_count += 1;
                sell_shares += txn.shares;
                sell_value += val;
            }
            _ => {}
        }
    }

    let summary = InsiderSummary {
        buy_count,
        sell_count,
        buy_shares,
        sell_shares,
        buy_value,
        sell_value,
        net_shares: buy_shares - sell_shares,
        net_value: buy_value - sell_value,
        unique_insiders: insiders_seen.len(),
    };

    let final_transactions = if req.summary_only {
        vec![]
    } else {
        transactions
    };

    Ok(InsiderResponse {
        ticker,
        company_name,
        cik: cik_str,
        generated_at: Utc::now(),
        days_lookback: days,
        summary,
        transactions: final_transactions,
    })
}

async fn fetch_form4_xml(client: &reqwest::Client, url: &str) -> Result<Vec<InsiderTransaction>> {
    let resp = client
        .get(url)
        .header("accept", "application/xml, text/xml")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("form4 fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "form4 fetch failed: http {} ({})",
            resp.status(),
            url
        )));
    }

    let xml = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("form4 read failed: {e}")))?;

    parse_form4_xml(&xml)
}

fn parse_form4_xml(xml: &str) -> Result<Vec<InsiderTransaction>> {
    let mut transactions = Vec::new();

    // Extract reporting owner info
    let owner_name = extract_xml_tag(xml, "rptOwnerName").unwrap_or_default();
    let officer_title = extract_xml_tag(xml, "officerTitle");
    let is_director = extract_xml_tag(xml, "isDirector")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);
    let is_officer = extract_xml_tag(xml, "isOfficer")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);
    let is_ten_percent_owner = extract_xml_tag(xml, "isTenPercentOwner")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    // Parse non-derivative transactions
    let mut cursor = 0usize;
    while let Some(start) = xml[cursor..].find("<nonDerivativeTransaction>") {
        let abs_start = cursor + start;
        let end = match xml[abs_start..].find("</nonDerivativeTransaction>") {
            Some(e) => abs_start + e + 27,
            None => break,
        };
        let txn_xml = &xml[abs_start..end];

        if let Some(txn) = parse_single_transaction(
            txn_xml,
            &owner_name,
            &officer_title,
            is_director,
            is_officer,
            is_ten_percent_owner,
        ) {
            transactions.push(txn);
        }

        cursor = end;
    }

    Ok(transactions)
}

fn parse_single_transaction(
    txn_xml: &str,
    owner_name: &str,
    officer_title: &Option<String>,
    is_director: bool,
    is_officer: bool,
    is_ten_percent_owner: bool,
) -> Option<InsiderTransaction> {
    let transaction_date = extract_xml_tag(txn_xml, "transactionDate")
        .and_then(|d| extract_xml_tag(&d, "value"))
        .unwrap_or_default();

    let transaction_code = extract_xml_tag(txn_xml, "transactionCode").unwrap_or_default();

    let shares_str = extract_xml_tag(txn_xml, "transactionShares")
        .and_then(|s| extract_xml_tag(&s, "value"))
        .unwrap_or_default();
    let shares: f64 = shares_str.parse().unwrap_or(0.0);

    let price_str = extract_xml_tag(txn_xml, "transactionPricePerShare")
        .and_then(|p| extract_xml_tag(&p, "value"))
        .unwrap_or_default();
    let price: Option<f64> = price_str.parse().ok();

    let acquired_disposed = extract_xml_tag(txn_xml, "transactionAcquiredDisposedCode")
        .and_then(|a| extract_xml_tag(&a, "value"))
        .unwrap_or_else(|| "D".to_string());

    let shares_owned_after_str = extract_xml_tag(txn_xml, "sharesOwnedFollowingTransaction")
        .and_then(|s| extract_xml_tag(&s, "value"))
        .unwrap_or_default();
    let shares_owned_after: Option<f64> = shares_owned_after_str.parse().ok();

    let value = price.map(|p| p * shares);

    if transaction_date.is_empty() || shares == 0.0 {
        return None;
    }

    Some(InsiderTransaction {
        filing_date: String::new(), // Will be filled in by caller
        transaction_date,
        insider_name: owner_name.to_string(),
        insider_title: officer_title.clone(),
        is_director,
        is_officer,
        is_ten_percent_owner,
        transaction_code,
        shares,
        price_per_share: price,
        value,
        acquired_disposed,
        shares_owned_after,
    })
}

async fn find_form4_xml_from_index(client: &reqwest::Client, index_url: &str) -> Result<String> {
    let resp = client
        .get(index_url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("index fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider("index fetch failed".to_string()));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("index parse failed: {e}")))?;

    // Look for XML file in directory listing
    if let Some(directory) = json.get("directory") {
        if let Some(items) = directory.get("item").and_then(|i| i.as_array()) {
            for item in items {
                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                    if name.ends_with(".xml") && !name.starts_with("xsl") {
                        return Ok(name.to_string());
                    }
                }
            }
        }
    }

    Err(Error::Provider("no xml file found in index".to_string()))
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{}>", tag);
    let end_tag = format!("</{}>", tag);

    let start = xml.find(&start_tag)? + start_tag.len();
    let end = xml.find(&end_tag)?;
    if end <= start { return None; }

    Some(xml[start..end].to_string())
}

fn cache_key(
    req: &TimeseriesRequest,
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<String> {
    #[derive(Serialize)]
    struct Key<'a> {
        v: u32,
        provider: &'a ProviderKind,
        tickers: Vec<&'a str>,
        range: String,
        granularity: String,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        max_points_per_ticker: usize,
    }

    let mut tickers_sorted: Vec<&str> = tickers.iter().map(|s| s.as_str()).collect();
    tickers_sorted.sort_unstable();

    let key = Key {
        v: 1,
        provider: &req.provider,
        tickers: tickers_sorted,
        range: req.range.to_string_compact(),
        granularity: req.granularity.to_string_compact(),
        start,
        end,
        max_points_per_ticker: req
            .max_points_per_ticker
            .unwrap_or(MAX_POINTS_PER_TICKER_DEFAULT),
    };

    let raw = serde_json::to_vec(&key)?;
    let mut hasher = Sha256::new();
    hasher.update(raw);
    Ok(format!("{:x}", hasher.finalize()))
}

fn cache_path(cache_dir: &Path, key: &str) -> PathBuf {
    cache_dir.join("finance").join("timeseries").join(format!("{key}.json"))
}

fn generate_mock_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
) -> Vec<TickerSeries> {
    tickers
        .iter()
        .map(|ticker| TickerSeries {
            ticker: ticker.clone(),
            candles: generate_mock_candles(ticker, start, end, step),
        })
        .collect()
}

fn generate_mock_candles(
    ticker: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
) -> Vec<Candle> {
    let mut rng = XorShift64::new(seed_from_str(ticker));
    let mut t = start;
    let mut price = base_price_from_seed(rng.next_u64());
    let mut out = Vec::new();

    while t <= end {
        let open = price;
        let move_pct = (rng.next_f64() - 0.5) * 0.02; // +/-1%
        price = (price * (1.0 + move_pct)).max(0.01);
        let close = price;

        let wick = rng.next_f64() * 0.005; // up to 0.5%
        let high = open.max(close) * (1.0 + wick);
        let low = open.min(close) * (1.0 - wick).max(0.0);
        let vol = Some((rng.next_f64() * 1_000_000.0).round());

        out.push(Candle {
            t,
            o: round_4(open),
            h: round_4(high),
            l: round_4(low),
            c: round_4(close),
            v: vol,
        });

        match t.checked_add_signed(step) {
            Some(next) => t = next,
            None => break,
        }
    }

    out
}

fn seed_from_str(s: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let bytes = hasher.finalize();
    let mut seed = 0u64;
    for b in bytes[..8].iter() {
        seed = (seed << 8) | (*b as u64);
    }
    seed
}

fn base_price_from_seed(seed: u64) -> f64 {
    let v = (seed % 20_000) as f64;
    10.0 + v / 10.0 // 10..2010
}

fn generate_mock_snapshots(tickers: &[String]) -> Vec<TickerSnapshot> {
    tickers
        .iter()
        .map(|ticker| {
            let seed = seed_from_str(ticker);
            let price = round_4(base_price_from_seed(seed));
            // 0.1B .. 20.1B shares
            let shares = 100_000_000u64 + (seed % 20_000_000_000u64);
            let market_cap = (price * (shares as f64)).round() as u64;

            TickerSnapshot {
                ticker: ticker.clone(),
                currency: Some("USD".to_string()),
                exchange: Some("MOCK".to_string()),
                short_name: Some(format!("{ticker} Corp")),
                long_name: Some(format!("{ticker} Corporation")),
                current_price: Some(price),
                previous_close: Some(round_4(price * 0.995)),
                open: Some(round_4(price * 1.002)),
                day_low: Some(round_4(price * 0.99)),
                day_high: Some(round_4(price * 1.01)),
                market_cap: Some(market_cap),
                enterprise_value: Some(market_cap as i64),
                shares_outstanding: Some(shares),
                float_shares: Some(shares.saturating_sub(shares / 10)),
                last_split_factor: None,
                last_split_date: None,
            }
        })
        .collect()
}

async fn fetch_yahoo_snapshots(tickers: &[String]) -> Result<Vec<TickerSnapshot>> {
    let mut connector = yahoo_finance_api::YahooConnector::new()
        .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

    let mut out = Vec::with_capacity(tickers.len());
    for ticker in tickers {
        let info = connector
            .get_ticker_info(ticker)
            .await
            .map_err(|e| Error::Provider(format!("yahoo quote summary failed for '{ticker}': {e}")))?;

        let qs = info.quote_summary.ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary missing for '{ticker}'"))
        })?;
        let result = qs.result.ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary result missing for '{ticker}'"))
        })?;
        let first = result.get(0).ok_or_else(|| {
            Error::Provider(format!("yahoo quote summary result empty for '{ticker}'"))
        })?;

        let quote_type = first.quote_type.as_ref();
        let summary = first.summary_detail.as_ref();
        let stats = first.default_key_statistics.as_ref();
        let fin = first.financial_data.as_ref();

        let currency = summary.and_then(|s| s.currency.clone());
        let exchange = quote_type.and_then(|q| q.exchange.clone());
        let short_name = quote_type.and_then(|q| q.short_name.clone());
        let long_name = quote_type.and_then(|q| q.long_name.clone());

        let current_price = fin.and_then(|f| f.current_price);
        let previous_close = summary
            .and_then(|s| s.regular_market_previous_close.or(s.previous_close));
        let open = summary.and_then(|s| s.regular_market_open.or(s.open));
        let day_low = summary.and_then(|s| s.regular_market_day_low.or(s.day_low));
        let day_high = summary.and_then(|s| s.regular_market_day_high.or(s.day_high));

        let enterprise_value = stats.and_then(|s| s.enterprise_value);
        let shares_outstanding = stats.and_then(|s| s.shares_outstanding);
        let float_shares = stats.and_then(|s| s.float_shares);

        let mut market_cap = summary.and_then(|s| s.market_cap);
        if market_cap.is_none() {
            if let (Some(px), Some(sh)) = (current_price, shares_outstanding) {
                if px.is_finite() && px > 0.0 {
                    market_cap = Some((px * (sh as f64)).round() as u64);
                }
            }
        }

        let last_split_factor = stats.and_then(|s| s.last_split_factor.clone());
        let last_split_date = stats
            .and_then(|s| s.last_split_date)
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single());

        out.push(TickerSnapshot {
            ticker: ticker.clone(),
            currency,
            exchange,
            short_name,
            long_name,
            current_price,
            previous_close,
            open,
            day_low,
            day_high,
            market_cap,
            enterprise_value,
            shares_outstanding,
            float_shares,
            last_split_factor,
            last_split_date,
        });
    }

    Ok(out)
}

async fn fetch_yahoo_series(
    tickers: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: Span,
    max_points_per_ticker: usize,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let (interval, base_span) = yahoo_base_interval(granularity);
    let include_prepost = matches!(base_span.unit, SpanUnit::Minute | SpanUnit::Hour);

    let base_step = base_span.approx_duration();
    let base_step_seconds = base_step.num_seconds();
    if base_step_seconds <= 0 {
        return Err(Error::InvalidInput("invalid base interval".to_string()));
    }

    let approx_points =
        ((end - start).num_seconds() / base_step_seconds).max(1) as usize + 1;
    if approx_points > max_points_per_ticker {
        return Err(Error::InvalidInput(format!(
            "requested ~{approx_points} raw points per ticker exceeds limit {max_points_per_ticker}; increase granularity or shrink range"
        )));
    }

    let start_ts = time::OffsetDateTime::from_unix_timestamp(start.timestamp())
        .map_err(|e| Error::Provider(format!("invalid start timestamp: {e}")))?;
    let request_end = end + Duration::days(1);
    let end_ts = time::OffsetDateTime::from_unix_timestamp(request_end.timestamp())
        .map_err(|e| Error::Provider(format!("invalid end timestamp: {e}")))?;

    let requested_step = granularity.approx_duration();
    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();

    for ticker in tickers {
        let quotes =
            match yahoo_fetch_quotes_retry(ticker, start_ts, end_ts, interval, include_prepost)
                .await
            {
                Ok(quotes) => quotes,
                Err(err) => {
                    errors.push(TimeseriesError {
                        ticker: ticker.clone(),
                        stage: Some("fetch".to_string()),
                        message: err.to_string(),
                    });
                    continue;
                }
            };

        let mut candles = Vec::with_capacity(quotes.len());
        let mut invalid_timestamp = None;
        for q in quotes {
            let Some(t) = Utc.timestamp_opt(q.timestamp as i64, 0).single() else {
                invalid_timestamp = Some(q.timestamp);
                break;
            };
            if t < start || t > end {
                continue;
            }
            candles.push(Candle {
                t,
                o: q.open,
                h: q.high,
                l: q.low,
                c: q.close,
                v: Some(q.volume as f64),
            });
        }

        if let Some(ts) = invalid_timestamp {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("parse".to_string()),
                message: format!("yahoo invalid timestamp: {ts}"),
            });
            continue;
        }

        candles.sort_by_key(|c| c.t);
        let candles = if requested_step == base_step {
            candles
        } else {
            resample_candles(&candles, start, requested_step)
        };

        if candles.is_empty() {
            errors.push(TimeseriesError {
                ticker: ticker.clone(),
                stage: Some("fetch".to_string()),
                message: "yahoo returned no data points in the requested range".to_string(),
            });
            continue;
        }

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
        });
    }

    Ok((out, errors))
}

async fn yahoo_fetch_quotes_retry(
    ticker: &str,
    start: time::OffsetDateTime,
    end: time::OffsetDateTime,
    interval: &str,
    include_prepost: bool,
) -> Result<Vec<yahoo_finance_api::Quote>> {
    const MAX_ATTEMPTS: usize = 3;
    let mut last_err: Option<String> = None;

    for attempt in 0..MAX_ATTEMPTS {
        let connector = yahoo_finance_api::YahooConnector::new()
            .map_err(|e| Error::Provider(format!("yahoo init failed: {e}")))?;

        let resp = if include_prepost {
            connector
                .get_quote_history_interval_prepost(ticker, start, end, interval, true)
                .await
        } else {
            connector
                .get_quote_history_interval(ticker, start, end, interval)
                .await
        };

        match resp {
            Ok(resp) => match resp.quotes() {
                Ok(quotes) if !quotes.is_empty() => return Ok(quotes),
                Ok(_) => last_err = Some("yahoo returned 0 quotes".to_string()),
                Err(e) => last_err = Some(e.to_string()),
            },
            Err(e) => {
                last_err = Some(e.to_string());

                let retryable = matches!(
                    e,
                    yahoo_finance_api::YahooError::ConnectionFailed(_)
                        | yahoo_finance_api::YahooError::FetchFailed(_)
                        | yahoo_finance_api::YahooError::DeserializeFailed(_)
                        | yahoo_finance_api::YahooError::DeserializeFailedDebug(_)
                        | yahoo_finance_api::YahooError::TooManyRequests(_)
                        | yahoo_finance_api::YahooError::Unauthorized
                        | yahoo_finance_api::YahooError::InvalidCrumb
                        | yahoo_finance_api::YahooError::NoCookies
                        | yahoo_finance_api::YahooError::InvalidCookie
                );

                if !retryable {
                    break;
                }
            }
        }

        if attempt + 1 < MAX_ATTEMPTS {
            let backoff_ms = 250u64.saturating_mul((attempt as u64) + 1);
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }
    }

    Err(Error::Provider(format!(
        "yahoo returned no data for '{ticker}' ({})",
        last_err.unwrap_or_else(|| "unknown error".to_string())
    )))
}

fn yahoo_base_interval(granularity: Span) -> (&'static str, Span) {
    // Largest supported interval <= requested granularity.
    // Supported intervals: 1m,2m,5m,15m,30m,90m,1h,1d,5d,1wk,1mo,3mo
    let requested = granularity.approx_duration().num_seconds().max(1);
    let mut best: (&'static str, Span, i64) = ("1m", Span { n: 1, unit: SpanUnit::Minute }, 60);

    let candidates: &[(&str, Span)] = &[
        ("1m", Span { n: 1, unit: SpanUnit::Minute }),
        ("2m", Span { n: 2, unit: SpanUnit::Minute }),
        ("5m", Span { n: 5, unit: SpanUnit::Minute }),
        ("15m", Span { n: 15, unit: SpanUnit::Minute }),
        ("30m", Span { n: 30, unit: SpanUnit::Minute }),
        ("90m", Span { n: 90, unit: SpanUnit::Minute }),
        ("1h", Span { n: 1, unit: SpanUnit::Hour }),
        ("1d", Span { n: 1, unit: SpanUnit::Day }),
        ("5d", Span { n: 5, unit: SpanUnit::Day }),
        ("1wk", Span { n: 1, unit: SpanUnit::Week }),
        ("1mo", Span { n: 1, unit: SpanUnit::Month }),
        ("3mo", Span { n: 3, unit: SpanUnit::Month }),
    ];

    for (interval, span) in candidates {
        let secs = span.approx_duration().num_seconds();
        if secs <= 0 || secs > requested {
            continue;
        }
        if secs >= best.2 {
            best = (*interval, *span, secs);
        }
    }

    (best.0, best.1)
}

async fn fetch_fred_series(
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

    let client = reqwest::Client::builder().no_proxy().build().map_err(|e| {
        Error::Provider(format!("fred client init failed: {e}"))
    })?;
    let start_date = start.date_naive().format("%Y-%m-%d").to_string();
    let end_date = end.date_naive().format("%Y-%m-%d").to_string();
    let step = granularity.approx_duration();

    let mut out = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();
    for series_id in tickers {
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
                errors.push(TimeseriesError {
                    ticker: series_id.clone(),
                    stage: Some("fetch".to_string()),
                    message: format!("fred fetch failed: {e}"),
                });
                continue;
            }
        };

        if !resp.status().is_success() {
            errors.push(TimeseriesError {
                ticker: series_id.clone(),
                stage: Some("fetch".to_string()),
                message: format!("fred fetch failed: http {}", resp.status()),
            });
            continue;
        }

        let body = match resp.text().await {
            Ok(body) => body,
            Err(e) => {
                errors.push(TimeseriesError {
                    ticker: series_id.clone(),
                    stage: Some("read".to_string()),
                    message: format!("fred read failed: {e}"),
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

fn sanitize_for_filename(input: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_sep = false;
        } else if !out.is_empty() && !last_sep {
            out.push('_');
            last_sep = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "file".to_string()
    } else {
        out
    }
}

fn truncate_chars(input: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if input.len() <= max {
        return input.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in input.char_indices() {
        if idx >= max {
            break;
        }
        out.push(ch);
    }
    out.push_str("\n… [truncated]");
    out
}

fn best_effort_sec_filing_excerpt(
    text: &str,
    form: &str,
    items: Option<&str>,
    max_chars: usize,
) -> String {
    let max_chars = max_chars.max(256);
    if text.trim().is_empty() {
        return String::new();
    }

    let mut header_candidates: Vec<usize> = Vec::new();
    let mut item_candidates: Vec<usize> = Vec::new();

    // Common SEC filing anchors (prefer starting after any iXBRL/header noise).
    for needle in [
        "SECURITIES AND EXCHANGE COMMISSION",
        "Securities and Exchange Commission",
        "UNITED STATES\n\nSECURITIES",
    ] {
        if let Some(idx) = text.find(needle) {
            header_candidates.push(idx);
        }
    }

    let form = form.trim();
    if !form.is_empty() {
        for needle in [format!("FORM {form}"), format!("Form {form}")] {
            if let Some(idx) = text.find(&needle) {
                header_candidates.push(idx);
            }
        }
    }

    if let Some(raw) = items {
        // SEC "items" can look like "1.01,2.03" or "1.01 2.03"
        for item in raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            for needle in [format!("ITEM {item}"), format!("Item {item}")] {
                if let Some(idx) = text.find(&needle) {
                    item_candidates.push(idx);
                }
            }
        }
    }

    let header_idx = header_candidates.into_iter().min();
    let item_idx = item_candidates.into_iter().min();

    let mut start = match (header_idx, item_idx) {
        (Some(h), Some(i)) => {
            // If the filing cover page is huge, prefer jumping to the first disclosed item.
            let jump_to_item_threshold = 5_000usize;
            if i > h && i.saturating_sub(h) > jump_to_item_threshold {
                i
            } else {
                h.min(i)
            }
        }
        (Some(h), None) => h,
        (None, Some(i)) => i,
        (None, None) => 0,
    };

    // Snap start to a sensible boundary (previous blank line if possible).
    if start > 0 {
        if let Some(boundary) = text[..start].rfind("\n\n") {
            start = boundary + 2;
        } else if let Some(boundary) = text[..start].rfind('\n') {
            start = boundary + 1;
        }
    }

    let excerpt = text[start..].trim_start();
    truncate_chars(excerpt, max_chars)
}

fn html_to_text(raw: &str) -> String {
    // Best-effort HTML -> text. This isn't a full parser, but works well enough for SEC filings.
    let s = raw.replace("\r\n", "\n");
    let mut out = String::with_capacity(s.len().min(128_000));
    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut skip_depth = 0usize;

    for ch in s.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag_raw = tag_buf.trim();
                let tag = tag_raw.to_ascii_lowercase();
                let mut name = tag.split_whitespace().next().unwrap_or_default();
                let is_end = name.starts_with('/');
                name = name.trim_start_matches('/').trim_end_matches('/');

                let should_skip = matches!(name, "script" | "style" | "head" | "ix:hidden" | "ix:header");
                if should_skip {
                    if is_end {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !tag.ends_with('/') {
                        // Only bump for non-self-closing tags.
                        skip_depth = skip_depth.saturating_add(1);
                    }
                }

                // Newline-ish tags (only when not in a skipped section).
                if skip_depth == 0 {
                    if tag.starts_with("br")
                        || tag.starts_with("/p")
                        || tag.starts_with("p")
                        || tag.starts_with("/div")
                        || tag.starts_with("div")
                        || tag.starts_with("/tr")
                        || tag.starts_with("tr")
                        || tag.starts_with("/li")
                        || tag.starts_with("li")
                        || tag.starts_with("hr")
                    {
                        out.push('\n');
                    }
                }
                tag_buf.clear();
            } else {
                // cap tag buffer to avoid huge memory on malformed input
                if tag_buf.len() < 256 {
                    tag_buf.push(ch);
                }
            }
            continue;
        }

        if ch == '<' {
            in_tag = true;
            continue;
        }
        if skip_depth == 0 {
            out.push(ch);
        }
    }

    // Decode entities and normalize whitespace.
    let decoded = html_escape::decode_html_entities(&out).to_string();
    let mut cleaned = String::with_capacity(decoded.len());
    let mut last_ws = false;
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if ch == '\n' {
                cleaned.push('\n');
                last_ws = false;
            } else if !last_ws {
                cleaned.push(' ');
                last_ws = true;
            }
        } else {
            cleaned.push(ch);
            last_ws = false;
        }
    }

    // Collapse excessive blank lines.
    let mut final_out = String::with_capacity(cleaned.len());
    let mut blank_run = 0usize;
    for line in cleaned.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                final_out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        final_out.push_str(line);
        final_out.push('\n');
    }

    final_out.trim().to_string()
}

fn sec_user_agent(override_ua: Option<&str>) -> Result<String> {
    if let Some(s) = override_ua {
        let s = s.trim();
        if !s.is_empty() {
            return Ok(s.to_string());
        }
    }

    let ua = std::env::var("ELI_SEC_USER_AGENT").unwrap_or_default();
    let ua = ua.trim();
    if ua.is_empty() {
        return Err(Error::InvalidInput(
            "SEC EDGAR requires a User-Agent with contact email. Set `ELI_SEC_USER_AGENT=\"eli (me@example.com)\"`, pass `--user-agent \"eli (me@example.com)\"`, or run `eli config --set sec_user_agent --value \"eli (me@example.com)\"`.".to_string(),
        ));
    }
    Ok(ua.to_string())
}

fn sec_client(override_ua: Option<&str>) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(25))
        .user_agent(sec_user_agent(override_ua)?)
        .build()
        .map_err(|e| Error::Provider(format!("sec client init failed: {e}")))
}

async fn sec_get_json(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("sec fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "sec fetch failed: http {} ({url})",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Provider(format!("sec read failed: {e}")))
}

async fn sec_get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("accept", "text/html, text/plain;q=0.9, */*;q=0.1")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("sec fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "sec fetch failed: http {} ({url})",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Provider(format!("sec read failed: {e}")))
}

async fn sec_lookup_cik(ticker: &str, sec_dir: &Path, ua: Option<&str>) -> Result<(String, String)> {
    let client = sec_client(ua)?;
    let map_path = sec_dir.join("company_tickers.json");

    let use_cache = file_is_fresh(&map_path, SEC_COMPANY_TICKERS_TTL_SECS);
    let raw = if use_cache {
        std::fs::read_to_string(&map_path)?
    } else {
        let url = "https://www.sec.gov/files/company_tickers.json";
        let text = sec_get_json(&client, url).await?;
        std::fs::write(&map_path, &text)?;
        text
    };

    #[derive(Deserialize)]
    struct Entry {
        cik_str: u64,
        ticker: String,
        title: String,
    }

    let parsed: std::collections::HashMap<String, Entry> =
        serde_json::from_str(&raw).map_err(|e| Error::Provider(format!("sec map parse failed: {e}")))?;

    for (_k, entry) in parsed {
        if entry.ticker.trim().eq_ignore_ascii_case(ticker) {
            let cik_padded = format!("{:010}", entry.cik_str);
            return Ok((cik_padded, entry.title));
        }
    }

    Err(Error::InvalidInput(format!("unknown ticker '{ticker}' for SEC filings")))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SecSubmissions {
    cik: String,
    name: Option<String>,
    filings: Option<SecFilings>,
}

#[derive(Deserialize)]
struct SecFilings {
    recent: Option<SecRecent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SecRecent {
    accession_number: Vec<String>,
    filing_date: Vec<String>,
    #[serde(default)]
    report_date: Option<Vec<String>>,
    #[serde(default)]
    acceptance_date_time: Option<Vec<String>>,
    form: Vec<String>,
    #[serde(default)]
    items: Option<Vec<String>>,
    #[serde(default)]
    size: Option<Vec<u64>>,
    #[serde(default)]
    primary_document: Option<Vec<String>>,
    #[serde(default)]
    primary_doc_description: Option<Vec<String>>,
}

async fn sec_fetch_submissions(
    cik_padded: &str,
    fallback_name: &str,
    sec_dir: &Path,
    ua: Option<&str>,
) -> Result<SecSubmissions> {
    let client = sec_client(ua)?;
    let submissions_dir = sec_dir.join("submissions");
    std::fs::create_dir_all(&submissions_dir)?;
    let path = submissions_dir.join(format!("CIK{cik_padded}.json"));

    let use_cache = file_is_fresh(&path, SEC_SUBMISSIONS_TTL_SECS);
    let raw = if use_cache {
        std::fs::read_to_string(&path)?
    } else {
        let url = format!("https://data.sec.gov/submissions/CIK{cik_padded}.json");
        let text = sec_get_json(&client, &url).await?;
        std::fs::write(&path, &text)?;
        text
    };

    let mut parsed: SecSubmissions =
        serde_json::from_str(&raw).map_err(|e| Error::Provider(format!("sec submissions parse failed: {e}")))?;
    if parsed.name.as_deref().unwrap_or("").trim().is_empty() {
        parsed.name = Some(fallback_name.to_string());
    }
    Ok(parsed)
}

fn file_is_fresh(path: &Path, max_age_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return false;
    };
    elapsed.as_secs() <= max_age_secs
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

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let seed = if seed == 0 { 0x9e3779b97f4a7c15 } else { seed };
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        let v = self.next_u64();
        (v as f64) / (u64::MAX as f64)
    }
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
        let range = Span::parse(&req.range)
            .map_err(|e| anyhow::anyhow!("invalid range: {e}"))?;
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
        let resp = fetch_timeseries(ts_req, &cache_dir).await
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
                    candle.v.map(|v| v.to_string()).unwrap_or_else(|| "N/A".to_string())
                ));
            }
            output.push('\n');
        }

        Ok(output)
    }
}
