use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum FinanceTypesError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

pub type Result<T> = std::result::Result<T, FinanceTypesError>;

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
            return Err(FinanceTypesError::InvalidInput("empty span".to_string()));
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
            return Err(FinanceTypesError::InvalidInput(format!(
                "invalid span '{raw}' (expected like 10min, 1h, 30d, 12mo, 5y)"
            )));
        }

        let n: i64 = s[..split_at].parse().map_err(|_| {
            FinanceTypesError::InvalidInput(format!("invalid span number: '{raw}'"))
        })?;
        if n <= 0 {
            return Err(FinanceTypesError::InvalidInput(format!(
                "span must be > 0: '{raw}'"
            )));
        }

        let unit_raw = s[split_at..].trim();
        let unit = match unit_raw {
            "m" => {
                return Err(FinanceTypesError::InvalidInput(format!(
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
                return Err(FinanceTypesError::InvalidInput(format!(
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
        return Err(FinanceTypesError::InvalidInput("empty as-of".to_string()));
    }

    // Prefer explicit RFC3339 when provided.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Accept YYYY-MM-DD as shorthand (assume end-of-day UTC).
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
        FinanceTypesError::InvalidInput(format!(
            "invalid as-of '{raw}' (use YYYY-MM-DD or RFC3339)"
        ))
    })?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(
        date.and_hms_opt(23, 59, 59).ok_or_else(|| {
            FinanceTypesError::InvalidInput(format!("invalid as-of date: '{raw}'"))
        })?,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug: Option<ToolDebugInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDebugInfo {
    pub raw_payload_path: String,
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

    /// Optional trailing returns by ticker and period (decimal returns).
    /// Shape: ticker -> period -> return
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_returns: Option<BTreeMap<String, BTreeMap<String, f64>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeseriesAnalytics {
    pub stats: BTreeMap<String, TimeseriesStats>,
    pub correlation_matrix: BTreeMap<String, BTreeMap<String, Option<f64>>>,
    pub periods_per_year: f64,

    #[serde(default = "default_risk_free_rate_annual")]
    pub risk_free_rate_annual: f64,
}

pub fn default_risk_free_rate_annual() -> f64 {
    0.02
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

    /// Warning when data may be stale (e.g. market closed, all returns 0.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_note: Option<String>,
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
    /// Informational note (e.g. "ETF — financial statements not available").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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

    #[serde(default)]
    pub compare_to: Option<NaiveDate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroIndicator {
    pub symbol: String,
    pub name: String,
    pub category: String,
    pub current_value: f64,
    pub change_1y: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare_value: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_abs: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_pct: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroResponse {
    pub generated_at: DateTime<Utc>,
    pub indicators: Vec<MacroIndicator>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RatePathRequest {
    #[serde(default)]
    pub cache_dir: Option<String>,
    #[serde(default)]
    pub source_mode: Option<RatePathSourceMode>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RatePathSourceMode {
    Auto,
    Meeting,
    Fallback,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RatePathMeeting {
    pub date: String,
    pub label: String,
    pub hold_prob: f64,
    pub cut_25bp_prob: f64,
    pub cut_50bp_plus_prob: f64,
    pub hike_prob: f64,
    pub implied_rate: f64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RatePathResponse {
    pub generated_at: DateTime<Utc>,
    pub as_of: DateTime<Utc>,
    pub age_seconds: i64,
    pub current_rate: f64,
    pub meetings: Vec<RatePathMeeting>,
    pub source_mode: String,
    pub coverage_ratio: f64,
    pub confidence: f64,

    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YieldCurveRequest {
    #[serde(default)]
    pub compare_3mo: bool,

    #[serde(default)]
    pub compare_1y: bool,
    #[serde(default)]
    pub strict: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YieldCurvePoint {
    pub maturity: String,
    pub maturity_months: u32,
    /// Percent level from FRED (e.g. 4.32 means 4.32%).
    pub current_yield: f64,

    /// Change vs 3 months ago in basis points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_3mo_bps: Option<f64>,

    /// Change vs 1 year ago in basis points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_1y_bps: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YieldCurveResponse {
    pub generated_at: DateTime<Utc>,
    pub as_of: DateTime<Utc>,
    pub age_seconds: i64,
    pub curve: Vec<YieldCurvePoint>,
    /// In percentage points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread_2y10y: Option<f64>,
    /// In percentage points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread_3mo10y: Option<f64>,
    pub coverage_ratio: f64,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub missing_symbols: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardRequest {
    pub preset: String,
    #[serde(default)]
    pub max_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardOddsMarket {
    pub source: String,
    pub ticker: String,
    pub title: String,
    pub event_ticker: String,
    pub yes_price: f64,
    pub volume: f64,
    pub volume_usd: f64,
    pub status: String,
    pub probability: f64,
    pub category: String,
    pub topic: String,
    pub match_score: i64,
    pub match_terms: Vec<String>,
    pub country_hints: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardOddsSearch {
    pub query: String,
    pub total_matches: usize,
    pub markets: Vec<DashboardOddsMarket>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardResponse {
    pub preset: String,
    pub generated_at: DateTime<Utc>,
    pub as_of: DateTime<Utc>,
    pub age_seconds: i64,
    // Named sections for the built-in `recession` preset (kept for backwards compat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub macro_data: Option<MacroResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<SnapshotResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odds: Option<Vec<DashboardOddsSearch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<OptionsResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_path: Option<RatePathResponse>,
    // Generic escape hatch: new presets use this instead of adding typed fields.
    // Each key is a section name; value is a JSON payload for that section.
    // Allows adding new presets in service.rs without touching this struct.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub sections: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_health: Option<BTreeMap<String, SectionHealth>>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SectionHealth {
    pub available: bool,
    pub coverage_ratio: f64,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    Earnings,
    Macro,
    All,
}

impl Default for ScheduleKind {
    fn default() -> Self {
        Self::All
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleRequest {
    pub kind: ScheduleKind,
    pub start_date: String, // YYYY-MM-DD
    pub end_date: String,   // YYYY-MM-DD
    #[serde(default)]
    pub tickers: Vec<String>,
    #[serde(default)]
    pub major_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EarningsScheduleEvent {
    pub date: String,
    pub symbol: String,
    pub company_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_cap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fiscal_quarter_ending: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eps_forecast: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_of_estimates: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_year_report_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_year_eps: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroScheduleEvent {
    pub date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroScheduleDay {
    pub date: String,
    pub release_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleResponse {
    pub generated_at: DateTime<Utc>,
    pub kind: ScheduleKind,
    pub start_date: String,
    pub end_date: String,
    pub earnings: Vec<EarningsScheduleEvent>,
    #[serde(rename = "macro")]
    pub macro_events: Vec<MacroScheduleEvent>,
    pub macro_days: Vec<MacroScheduleDay>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PricesRequest {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub asset_type: Option<String>,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub auto_select: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implied_volatility: Option<f64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_series: Option<Vec<OddsSeries>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_events: Option<Vec<OddsListedEvent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_markets: Option<Vec<OddsListedMarket>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_tags: Option<Vec<OddsTag>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analytics: Option<OddsAnalytics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<OddsSourceInfo>>,
    #[serde(default = "default_odds_field_semantics")]
    pub field_semantics: OddsFieldSemantics,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsAnalytics {
    pub markets_total: usize,
    pub open_markets: usize,
    pub active_markets: usize,
    pub initialized_markets: usize,
    pub markets_with_volume: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_probability_yes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_spread_cents: Option<f64>,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsFieldSemantics {
    pub probability_scale: String,
    pub yes_price_units: String,
    pub volume_units: String,
}

pub fn default_odds_field_semantics() -> OddsFieldSemantics {
    OddsFieldSemantics {
        probability_scale: "0_to_1".to_string(),
        yes_price_units: "cents".to_string(),
        volume_units: "cents".to_string(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncRequest {
    #[serde(default)]
    pub sources: Option<Vec<String>>,
    #[serde(default)]
    pub cache_dir: Option<String>,
    #[serde(default)]
    pub max_pages: Option<usize>,
    #[serde(default)]
    pub strict: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncCoverage {
    pub requested_max_pages: usize,
    pub events_pages_fetched: usize,
    pub events_exhausted: bool,
    pub markets_pages_fetched: usize,
    pub markets_exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_backfill_calls: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_backfill_cap: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_backfill_truncated: Option<bool>,
    pub strict_pass: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strict_fail_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncSourceResult {
    pub source: String,
    pub ok: bool,
    pub events_count: usize,
    pub markets_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_count: Option<usize>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csv_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analytics: Option<OddsSyncSourceAnalytics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<OddsSyncCoverage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncCategorySummary {
    pub category: String,
    pub markets: usize,
    pub volume_sum: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncProbabilityBucket {
    pub range: String,
    pub markets: usize,
    pub volume_sum: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncMarketSummary {
    pub source: String,
    pub ticker: String,
    pub title: String,
    pub event_ticker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability_yes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncSourceAnalytics {
    pub markets_with_probability: usize,
    pub markets_with_volume: usize,
    pub total_volume: i64,
    pub top_categories: Vec<OddsSyncCategorySummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncAnalysis {
    pub markets_with_probability: usize,
    pub markets_with_volume: usize,
    pub total_volume: i64,
    pub probability_buckets: Vec<OddsSyncProbabilityBucket>,
    pub zero_yes_with_volume_count: usize,
    pub zero_yes_with_1k_volume_count: usize,
    pub near_even_with_1k_volume_count: usize,
    pub high_confidence_with_10k_volume_count: usize,
    pub extreme_prob_with_1k_volume_count: usize,
    pub informative_prob_with_1k_volume_count: usize,
    pub extreme_prob_volume_sum: i64,
    pub informative_prob_volume_sum: i64,
    pub extreme_prob_volume_share_pct: f64,
    pub cross_source_event_overlap_by_title: usize,
    pub top_categories: Vec<OddsSyncCategorySummary>,
    pub top_markets_by_volume: Vec<OddsSyncMarketSummary>,
    pub top_markets_by_informative_volume: Vec<OddsSyncMarketSummary>,
    pub anomalous_zero_yes_markets: Vec<OddsSyncMarketSummary>,
    pub near_even_high_volume_markets: Vec<OddsSyncMarketSummary>,
    pub high_confidence_high_volume_markets: Vec<OddsSyncMarketSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncResponse {
    pub generated_at: DateTime<Utc>,
    pub sources: Vec<OddsSyncSourceResult>,
    pub total_events: usize,
    pub total_markets: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_csv_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis: Option<OddsSyncAnalysis>,
    #[serde(default = "default_odds_field_semantics")]
    pub field_semantics: OddsFieldSemantics,
}

impl Default for OddsRequest {
    fn default() -> Self {
        Self {
            provider: None,
            disable_kalshi: false,
            series_ticker: None,
            event_ticker: None,
            market_ticker: None,
            status: None,
            limit: None,
            cursor: None,
            max_pages: None,
            include_orderbook: false,
            orderbook_depth: None,
            list_series: false,
            list_events: false,
            list_markets: false,
            list_tags: false,
            category: None,
            search: None,
        }
    }
}
