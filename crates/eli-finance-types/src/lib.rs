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
    /// Interactive Brokers TWS / IB Gateway via the official IB API.
    Ibkr,
    /// Pyth Network Benchmarks — 24/7 oracle candles (crypto, commodities, FX, metals).
    Pyth,
    /// Kalshi prediction market — probability candlesticks via public API.
    Kalshi,
    /// Polymarket prediction market — probability history via CLOB API.
    Polymarket,
    /// Binance — crypto OHLCV with depth back to 2017/2019.
    Binance,
    /// EIA — U.S. Energy Information Administration (petroleum inventories, nat gas storage).
    Eia,
    /// ECB — European Central Bank SDMX (EUR rates, balance sheet, M3, EURIBOR).
    Ecb,
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
            "m" | "min" | "mins" | "minute" | "minutes" => SpanUnit::Minute,
            "h" | "hr" | "hrs" | "hour" | "hours" => SpanUnit::Hour,
            "d" | "day" | "days" => SpanUnit::Day,
            "w" | "wk" | "wks" | "week" | "weeks" => SpanUnit::Week,
            "mo" | "mon" | "month" | "months" => SpanUnit::Month,
            "y" | "yr" | "yrs" | "year" | "years" => SpanUnit::Year,
            _ => {
                return Err(FinanceTypesError::InvalidInput(format!(
                    "invalid span unit '{unit_raw}' (expected m|min,h,d,w,mo,y)"
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

#[cfg(test)]
mod tests {
    use super::{Span, SpanUnit};

    #[test]
    fn parse_m_alias_maps_to_minutes() {
        let span = Span::parse("15m").expect("15m should parse as minutes");
        assert_eq!(span.n, 15);
        assert_eq!(span.unit, SpanUnit::Minute);
    }

    #[test]
    fn parse_mo_alias_maps_to_months() {
        let span = Span::parse("1mo").expect("1mo should parse as months");
        assert_eq!(span.n, 1);
        assert_eq!(span.unit, SpanUnit::Month);
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

pub fn parse_event_at(raw: &str) -> Result<DateTime<Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(FinanceTypesError::InvalidInput(
            "empty event-at".to_string(),
        ));
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
        FinanceTypesError::InvalidInput(format!(
            "invalid event-at '{raw}' (use YYYY-MM-DD or RFC3339)"
        ))
    })?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0).ok_or_else(|| {
            FinanceTypesError::InvalidInput(format!("invalid event-at date: '{raw}'"))
        })?,
        Utc,
    ))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Observe,
    Assist,
    Enforce,
}

impl Default for PolicyMode {
    fn default() -> Self {
        Self::Observe
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AppliedPolicy {
    #[serde(default)]
    pub mode: PolicyMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessState {
    Live,
    Delayed,
    Eod,
    Historical,
    Stale,
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessOrigin {
    ProviderTimestamp,
    ProviderLastUpdated,
    TransportReceived,
    Derived,
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessQuality {
    Exact,
    Estimated,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Freshness {
    pub observed_at: DateTime<Utc>,
    pub collected_at: DateTime<Utc>,
    pub age_seconds: i64,
    pub state: FreshnessState,
    pub origin: FreshnessOrigin,
    pub quality: FreshnessQuality,
}

impl Default for Freshness {
    fn default() -> Self {
        let epoch = DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_default();
        Self {
            observed_at: epoch,
            collected_at: epoch,
            age_seconds: 0,
            state: FreshnessState::Unknown,
            origin: FreshnessOrigin::Unknown,
            quality: FreshnessQuality::Estimated,
        }
    }
}

impl Freshness {
    pub fn new(
        observed_at: DateTime<Utc>,
        collected_at: DateTime<Utc>,
        state: FreshnessState,
        origin: FreshnessOrigin,
        quality: FreshnessQuality,
    ) -> Self {
        let age_seconds = (collected_at - observed_at).num_seconds().max(0);
        Self {
            observed_at,
            collected_at,
            age_seconds,
            state,
            origin,
            quality,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct FreshnessSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_as_of: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<i64>,
    #[serde(default)]
    pub stale_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct IbkrConnectionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<i32>,
    /// IBKR market data type. Common values:
    /// 1 = live, 2 = frozen, 3 = delayed, 4 = delayed-frozen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_data_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RunMeta {
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub stdout_chars: usize,
    #[serde(default)]
    pub stored_bytes: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub coverage_counts: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_efficiency: Option<f64>,
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

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ibkr: Option<IbkrConnectionConfig>,
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

    /// Optional candle kind. None = standard OHLC. Some("point") = single
    /// data point projected onto OHLC fields (o == h == l == c) — typically
    /// nowcast/forecast series like CLEV:CPI. Downstream chart/vol code should
    /// avoid candlestick rendering and skip volatility calc when kind == "point".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TickerSeries {
    pub ticker: String,
    pub candles: Vec<Candle>,
    /// Upstream data source identifier ("yahoo", "fred", "pyth", "ibkr", "binance",
    /// "cleveland_fed", "fed_h15", "polymarket", "kalshi", "mock").
    /// Lets callers tell which provider served each series in mixed-source responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Upstream identifier (post-prefix-strip): for `FRED:DGS10` source="fred",
    /// upstream_id="DGS10". For `IBKR:FUT:CL:NYMEX` source="ibkr",
    /// upstream_id="FUT:CL:NYMEX". Lets callers reconstruct the upstream URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
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
    /// Dispatch-time provider choice. On mixed-source calls (Yahoo + Pyth +
    /// Polymarket etc.) this only reflects the first / "main" bucket and
    /// is misleading on its own — see `sources` for the actual provider mix
    /// across returned series.
    pub provider: ProviderKind,
    /// Distinct upstream sources actually represented in `series` after merge.
    /// `["yahoo"]` for a single-source call, `["yahoo","pyth","polymarket"]`
    /// for a mixed call, `["mixed"]` shorthand never used here — the array
    /// is the truth.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
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

    // Cache hit/path/key are internal plumbing (incl. a local file path) — never surfaced.
    #[serde(skip_serializing)]
    pub cache: Option<CacheInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRequest {
    pub tickers: Vec<String>,

    #[serde(default)]
    pub as_of: Option<DateTime<Utc>>,

    #[serde(default)]
    pub provider: ProviderKind,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ibkr: Option<IbkrConnectionConfig>,
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

    /// Alias for current_price — kept for internal use, excluded from JSON output.
    #[serde(skip_serializing)]
    pub price: Option<f64>,

    /// Session return vs previous close — trivially derivable, excluded from JSON output.
    #[serde(skip_serializing)]
    pub daily_return: Option<f64>,

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

    /// Per-item freshness — used internally for FreshnessSummary computation.
    /// Excluded from JSON output to reduce context bloat; response-level
    /// freshness_summary provides the same info once instead of N times.
    #[serde(skip_serializing, default)]
    pub freshness: Freshness,
    pub price_source_kind: String,
    pub session_state: String,
    #[serde(default)]
    pub market_closed_fallback: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity_note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotResponse {
    pub provider: ProviderKind,
    pub tickers: Vec<String>,
    pub generated_at: DateTime<Utc>,
    pub snapshots: Vec<TickerSnapshot>,
    #[serde(skip_serializing, default)]
    pub schema_version: String,
    #[serde(skip_serializing, default)]
    pub freshness_summary: FreshnessSummary,
    #[serde(skip_serializing, default)]
    pub applied_policy: AppliedPolicy,
    #[serde(skip_serializing, default)]
    pub decision_trace: Vec<String>,
    #[serde(skip_serializing, default)]
    pub run_meta: RunMeta,
    #[serde(skip_serializing, default)]
    pub market_closed_fallback_count: usize,
    #[serde(skip_serializing, default)]
    pub has_market_closed_fallback: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<SnapshotError>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_tickers: Option<Vec<String>>,

    /// Full analytics kept for internal use, excluded from JSON output.
    #[serde(skip_serializing)]
    pub analytics: Option<SnapshotAnalytics>,

    /// Optional trailing returns by ticker and period (decimal returns).
    /// Shape: ticker -> period -> return
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_returns: Option<BTreeMap<String, BTreeMap<String, f64>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotError {
    pub ticker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeseriesAnalytics {
    pub stats: BTreeMap<String, TimeseriesStats>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeseriesStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_return: Option<f64>,
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

    /// If true, download the filing index JSON and primary document under cache_dir.
    #[serde(default = "default_true")]
    pub download: bool,

    /// If true, download every document listed in the filing index, not just the primary document.
    #[serde(default)]
    pub download_all: bool,

    /// If true, download likely useful text exhibits such as EX-99, earnings releases, and slides.
    #[serde(default = "default_true")]
    pub important_exhibits: bool,

    /// If true, download only one file: the primary document for the latest matching filing.
    #[serde(default)]
    pub single_file: bool,

    /// If true, include the primary filing document's raw decoded text inline.
    #[serde(default)]
    pub raw_text: bool,

    /// If true, for earnings 8-Ks (item 2.02), locate and inline the press-release exhibit
    /// (typically EX-99.1) as parsed plain text in `press_release_text`. Skips the XBRL cover.
    #[serde(default)]
    pub press_release_text: bool,

    /// Deprecated compatibility alias for download. No text parsing or excerpting is performed.
    #[serde(default)]
    pub include_text: bool,

    /// Deprecated compatibility option. Filings are downloaded raw; no inline excerpt is generated.
    #[serde(default)]
    pub max_chars: Option<usize>,

    /// Optional SEC User-Agent override (e.g. "eli-cli (mailto:...)").
    #[serde(default)]
    pub user_agent: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilingDownload {
    /// index_json, primary_document, or attachment.
    pub kind: String,
    pub filename: String,
    pub url: String,
    pub path: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
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
    pub download_dir: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_doc_path: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_json_path: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub downloaded_files: Vec<FilingDownload>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_path: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_text_bytes: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_text_truncated: Option<bool>,

    /// Path to the detected press-release exhibit, set when --press-release-text was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub press_release_path: Option<String>,

    /// Parsed plain text of the press-release exhibit, set when --press-release-text was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub press_release_text: Option<String>,

    /// Deprecated. No filing text is parsed into an excerpt.
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
pub struct FundamentalsMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_cap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise_value: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_pe: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward_pe: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_eps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward_eps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_to_book: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_value_per_share: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise_to_revenue: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise_to_ebitda: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profit_margin: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gross_margin: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_margin: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ebitda_margin: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_on_assets: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_on_equity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debt_to_equity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue_growth: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earnings_growth: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue_per_share: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cash_per_share: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shares_outstanding: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub float_shares: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_percent_of_float: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyst_target_mean_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation_mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyst_count: Option<u64>,
    /// Trailing-twelve-month dividend yield, expressed as a decimal (0.04 = 4%).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dividend_yield: Option<f64>,
    /// Timestamp (RFC3339) of the price used to compute the price-derived multiples
    /// (current_price, market_cap, trailing_pe, price_to_book, enterprise_value, EV ratios).
    /// Sourced from Yahoo's v8 chart `regularMarketTime`. Lets a consumer see exactly how
    /// fresh the price is (live, prior close, or stale) instead of guessing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_as_of: Option<String>,
    /// The data provider behind the price (e.g. "yahoo"). Freshness is conveyed by
    /// `price_as_of`; the internal endpoint is not named in output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_provider: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundamentalsProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub industry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_time_employees: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundamentalsResponse {
    pub ticker: String,
    pub company_name: Option<String>,
    pub currency: Option<String>,
    pub generated_at: DateTime<Utc>,
    pub statements: Vec<FinancialStatement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<FundamentalsMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<FundamentalsProfile>,
    /// Informational note (e.g. "ETF — financial statements not available").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ibkr: Option<IbkrConnectionConfig>,
    #[serde(default)]
    pub policy_file: Option<String>,
    #[serde(default)]
    pub policy_mode: Option<PolicyMode>,
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
    pub generated_at: DateTime<Utc>,
    #[serde(default)]
    pub schema_version: String,
    /// "yahoo" | "fred" | "both" — which provider the agent should read first.
    pub preferred_provider: String,
    pub yahoo_results: Vec<SearchItem>,
    pub fred_results: Vec<SearchItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_trace: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroRequest {
    pub range: Option<Span>,

    #[serde(default)]
    pub compare_to: Option<NaiveDate>,
    #[serde(default)]
    pub policy_file: Option<String>,
    #[serde(default)]
    pub policy_mode: Option<PolicyMode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroIndicator {
    pub symbol: String,
    pub name: String,
    pub category: String,
    pub current_value: f64,
    pub change_1y: Option<f64>,
    #[serde(skip_serializing, default)]
    pub freshness: Freshness,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare_value: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_abs: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_pct: Option<f64>,

    /// Data source: "fred", "pyth", "kalshi", "polymarket", "yahoo"
    #[serde(default = "default_source_fred")]
    pub source: String,
}

fn default_source_fred() -> String {
    "fred".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroResponse {
    pub generated_at: DateTime<Utc>,
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub freshness_summary: FreshnessSummary,
    #[serde(default, skip_serializing)]
    pub applied_policy: AppliedPolicy,
    #[serde(default, skip_serializing)]
    pub decision_trace: Vec<String>,
    #[serde(default, skip_serializing)]
    pub run_meta: RunMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare_to: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories: Option<BTreeMap<String, Vec<MacroIndicator>>>,
    pub indicators: Vec<MacroIndicator>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexRequest {
    /// Optional explicit Yahoo FX tickers (e.g. EURUSD=X, USDJPY=X).
    /// If empty, uses Eli's curated FX basket.
    #[serde(default)]
    pub pairs: Vec<String>,
    /// Optional currency codes to compare against USD (e.g. CAD,JPY,EUR).
    #[serde(default)]
    pub currencies: Vec<String>,
    /// Optional country codes mapped to currencies (e.g. US,CA,JP,GB,EU).
    #[serde(default)]
    pub countries: Vec<String>,
    /// Optional preset groups (majors, g10, em, europe, americas, asia, commodity).
    #[serde(default)]
    pub groups: Vec<String>,
    /// Include selected EM USD pairs in the default basket.
    #[serde(default)]
    pub include_em: bool,
    pub range: Span,
    pub granularity: Span,
    #[serde(default)]
    pub as_of: Option<DateTime<Utc>>,
    /// Optional event timestamp (RFC3339 or YYYY-MM-DD) to run pre/post window analysis.
    #[serde(default)]
    pub event_at: Option<DateTime<Utc>>,
    /// Optional pre/post window around event_at (e.g. 6h,12h,1d,3d).
    #[serde(default)]
    pub event_window: Option<Span>,
    /// Optional historical comparison anchors.
    #[serde(default)]
    pub compare_as_of: Vec<DateTime<Utc>>,
    /// Optional horizon windows for change calculations (e.g. 1d,1w,1mo,3mo,1y).
    /// If empty, Eli uses defaults.
    #[serde(default)]
    pub horizons: Vec<Span>,
    /// Optional cap on resolved pair count (after filters, before fetch).
    #[serde(default)]
    pub max_pairs: Option<usize>,
    /// Optional number of latest close points to include per pair.
    #[serde(default)]
    pub recent_points: Option<usize>,
    /// Number of largest daily USD-impact moves to include.
    #[serde(default)]
    pub top: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexPricePoint {
    pub t: DateTime<Utc>,
    pub c: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexPairPerformance {
    pub ticker: String,
    pub pair: String,
    pub base_currency: String,
    pub quote_currency: String,
    pub first_observation_at: DateTime<Utc>,
    pub last_observation_at: DateTime<Utc>,
    pub observations: usize,
    pub rate: f64,
    pub start_rate: f64,
    pub end_rate: f64,
    pub change_pct: f64,
    /// Positive means USD strengthened vs this pair's non-USD side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_change_pct: Option<f64>,
    /// Change map keyed by horizon label (e.g. "1d", "1w", "1mo").
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub horizon_change_pct: BTreeMap<String, f64>,
    /// Annualized volatility estimate in percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annualized_vol_pct: Option<f64>,
    /// Optional compact tail series for context (latest closes only).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub recent_prices: Vec<ForexPricePoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub biggest_daily_drop_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub biggest_daily_drop_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub biggest_daily_rise_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub biggest_daily_rise_date: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexHitEvent {
    pub ticker: String,
    pub pair: String,
    pub date: String,
    pub daily_change_pct: f64,
    /// Positive means USD strengthened on that day.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_impact_pct: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexPairMove {
    pub pair: String,
    pub usd_change_pct: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexDateCluster {
    pub date: String,
    pub move_count: usize,
    pub max_abs_usd_impact_pct: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexEventPairShift {
    pub pair: String,
    pub pre_usd_change_pct: f64,
    pub post_usd_change_pct: f64,
    pub shift_pct: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexSessionAttribution {
    pub session: String,
    pub move_count: usize,
    pub avg_usd_impact_pct: f64,
    pub max_abs_usd_impact_pct: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexEventWindowSummary {
    pub event_at: DateTime<Utc>,
    pub window: Span,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_usd_strength_score_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_usd_strength_score_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shift_usd_strength_pct: Option<f64>,
    pub pre_pairs_up: usize,
    pub pre_pairs_down: usize,
    pub post_pairs_up: usize,
    pub post_pairs_down: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_pair_shifts: Vec<ForexEventPairShift>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub session_attribution: Vec<ForexSessionAttribution>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexComparisonPoint {
    pub as_of: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_strength_score_pct: Option<f64>,
    pub usd_pairs_up: usize,
    pub usd_pairs_down: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strongest_usd_pair: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weakest_usd_pair: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexComparisonDelta {
    pub as_of: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_usd_strength_pct: Option<f64>,
    pub delta_usd_pairs_up: i64,
    pub delta_usd_pairs_down: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexDeltaPairMove {
    pub pair: String,
    pub previous_usd_change_pct: f64,
    pub current_usd_change_pct: f64,
    pub delta_usd_change_pct: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexDeltaContext {
    pub previous_as_of: DateTime<Utc>,
    pub current_as_of: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_synced_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_synced_at: Option<DateTime<Utc>>,
    pub compared_pairs: usize,
    pub changed_pairs: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_pair_deltas: Vec<ForexDeltaPairMove>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexUsdBenchmark {
    pub source: String,
    pub symbol: String,
    pub as_of: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_pct: Option<f64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub horizon_change_pct: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexSummary {
    /// Average USD % change across pairs where USD impact is defined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_strength_score_pct: Option<f64>,
    pub usd_pairs_up: usize,
    pub usd_pairs_down: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strongest_usd_pair: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weakest_usd_pair: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_usd_gainers: Vec<ForexPairMove>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_usd_losers: Vec<ForexPairMove>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub hot_dates: Vec<ForexDateCluster>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexSelection {
    pub requested_groups: Vec<String>,
    pub requested_countries: Vec<String>,
    pub requested_currencies: Vec<String>,
    pub resolved_currencies: Vec<String>,
    pub resolved_pairs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForexResponse {
    pub generated_at: DateTime<Utc>,
    pub as_of: DateTime<Utc>,
    pub range: Span,
    pub granularity: Span,
    pub basket: String,
    pub pair_count: usize,
    pub selection: ForexSelection,
    pub pairs: Vec<ForexPairPerformance>,
    pub summary: ForexSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basket_summary: Option<ForexSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub comparisons: Vec<ForexComparisonPoint>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub comparison_deltas: Vec<ForexComparisonDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_window: Option<ForexEventWindowSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_context: Option<ForexDeltaContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_benchmark: Option<ForexUsdBenchmark>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub biggest_daily_usd_moves: Vec<ForexHitEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_daily_impact_moves: Vec<ForexHitEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
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
    /// P(no change at this meeting)
    pub hold_prob: f64,
    /// P(any cut at this meeting)
    pub cut_prob: f64,
    /// P(cut exactly 25bp)
    pub cut_25bp_prob: f64,
    /// P(cut >25bp, i.e. 50bp+)
    pub cut_50bp_plus_prob: f64,
    /// P(hike)
    pub hike_prob: f64,
    /// Total prediction-market volume across all contracts for this meeting (proxy for market conviction).
    /// Kept under the `volume` name for backwards compatibility; `volume_total` is the canonical field.
    pub volume: i64,
    /// Same as `volume` — exposed under the explicit name agents look for.
    pub volume_total: i64,
    /// Number of distinct markets aggregated into this meeting's probabilities.
    /// 1 = single thin market (be skeptical), 5+ = deep volume-weighted consensus.
    pub n_markets: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RatePathResponse {
    pub generated_at: DateTime<Utc>,
    pub as_of: DateTime<Utc>,
    pub age_seconds: i64,
    /// Current policy anchor rate used for meeting-bucket classification.
    pub current_rate: f64,
    pub current_rate_basis: String,
    pub current_rates: RatePathCurrentRates,
    pub meetings: Vec<RatePathMeeting>,
    pub source_mode: String,
    /// Fraction of expected FOMC meetings (8 per year) with data in the cache.
    pub coverage_ratio: f64,

    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,

    /// Cumulative "hike/cut by date X" signals from prediction markets.
    /// NOT per-meeting — represents P(any hike/cut occurs by deadline).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cumulative_signals: Vec<CumulativeFedSignal>,

    /// Year-end aggregates: cardinality markets ("How many cuts in 2026?",
    /// "What will rate be at end of 2026?", "Cut by which meeting?"). Separate
    /// epistemic layer from per-meeting hold/cut probabilities.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub year_view: Option<YearView>,

    /// Joint multi-meeting compound probabilities (e.g. "Pause-Pause-Pause"
    /// across Mar-Jun). Kept separate from per-meeting buckets — these are
    /// joint outcomes, not single-meeting marginals.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub compound_paths: Vec<CompoundPath>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YearView {
    pub year: i32,
    /// "How many cuts in <year>?" market: {0: 0.59, 1: 0.17, 2: 0.13, ...}
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cuts_distribution: Option<std::collections::BTreeMap<i32, f64>>,
    /// "What will rate be at end of <year>?" market: {"3.75%": 0.55, ...}
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub eoy_rate_distribution: Option<std::collections::BTreeMap<String, f64>>,
    /// "Fed rate cut by ... meeting?" market: {"december": 0.46, ...}
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cut_by_meeting_distribution: Option<std::collections::BTreeMap<String, f64>>,
    /// Total volume across the markets that contributed.
    pub volume_total: i64,
    pub n_markets: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompoundPath {
    /// e.g. "Mar-Jun", "Apr-Jul", "Jun-Sep"
    pub label: String,
    /// e.g. "Pause-Pause-Pause", "Pause-Cut-Pause"
    pub outcome: String,
    pub probability: f64,
    pub volume: i64,
    /// "polymarket" / "kalshi"
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CumulativeFedSignal {
    pub direction: String,
    pub by_date: String,
    pub probability: f64,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RatePathCurrentRates {
    /// Same value as `current_rate`, repeated so all current-rate facts live together.
    pub classification_anchor_rate: f64,
    pub classification_anchor_basis: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_lower_bound: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_upper_bound: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_midpoint: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_range_as_of: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_rate_as_of: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_average_effective_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_average_effective_rate_as_of: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YieldCurveRequest {
    #[serde(default)]
    pub compare_1mo: bool,

    #[serde(default)]
    pub compare_3mo: bool,

    #[serde(default)]
    pub compare_6mo: bool,

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

    /// Change vs 1 month ago in basis points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_1mo_bps: Option<f64>,

    /// Change vs 3 months ago in basis points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_3mo_bps: Option<f64>,

    /// Change vs 6 months ago in basis points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_6mo_bps: Option<f64>,

    /// Change vs 1 year ago in basis points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_1y_bps: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YieldCurveSpreads {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread_2y10y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread_3mo10y: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YieldCurveComparison {
    pub label: String,
    pub as_of: DateTime<Utc>,
    pub curve: Vec<YieldCurvePoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread_2y10y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread_3mo10y: Option<f64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spreads: Option<YieldCurveSpreads>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub comparisons: Vec<YieldCurveComparison>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yield_curve: Option<YieldCurveResponse>,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleMacroProfile {
    Broad,
    Market,
    Major,
}

impl Default for ScheduleMacroProfile {
    fn default() -> Self {
        Self::Market
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
    #[serde(default)]
    pub macro_profile: ScheduleMacroProfile,
    #[serde(default)]
    pub min_market_cap: Option<f64>,
    #[serde(default)]
    pub time_filter: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EarningsScheduleEvent {
    pub date: String,
    pub symbol: String,
    pub company_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// Market cap in USD (e.g. 587_802_343_381). Parsed from Nasdaq's
    /// "$587,802,343,381"-style strings; null when source value is missing or
    /// unparseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_cap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fiscal_quarter_ending: Option<String>,
    /// EPS estimate in USD (e.g. 1.06). Parsed from "$1.06"-style strings;
    /// null when source value is missing or unparseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eps_forecast: Option<f64>,
    /// Number of analyst estimates (e.g. 11). Null when missing/unparseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_of_estimates: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_year_report_date: Option<String>,
    /// Last-year EPS in USD. Parsed from "$0.78"-style strings; null when
    /// source value is missing or unparseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_year_eps: Option<f64>,
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
    pub macro_profile: ScheduleMacroProfile,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
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
    #[serde(default)]
    pub provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ibkr: Option<IbkrConnectionConfig>,
    /// If true, fetch Yahoo as the baseline and opportunistically overlay IBKR
    /// fields such as delayed last/model ticks and Greeks when Gateway is reachable.
    #[serde(default)]
    pub ibkr_overlay: bool,
    /// Optional as-of timestamp. Current providers expose live/delayed option-chain
    /// snapshots, not historical full-chain snapshots, so past dates are rejected
    /// by the provider layer instead of returning current data under a past label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
    /// Specific expiration date (YYYY-MM-DD). If None, returns first available expiry.
    #[serde(default)]
    pub expiry: Option<String>,
    /// Target days-to-expiry. If set and expiry is omitted, chooses the closest listed expiry.
    #[serde(default)]
    pub target_dte_days: Option<i64>,
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
    /// Absolute change from prior close. None when the upstream feed didn't
    /// report it (e.g. weekend/holiday Yahoo response where the field is null).
    /// Distinct from Some(0.0), which means "the contract genuinely didn't move".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<f64>,
    /// Percent change from prior close. None when the upstream feed didn't
    /// report it. Distinct from Some(0.0), which means "no movement".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct_change: Option<f64>,
    pub volume: u64,
    pub open_interest: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implied_volatility: Option<f64>,
    pub in_the_money: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gamma: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theta: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vega: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptionsMetrics {
    pub underlying_price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put_call_ratio_volume: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put_call_ratio_oi: Option<f64>,
    pub total_call_volume: u64,
    pub total_put_volume: u64,
    pub total_call_oi: u64,
    pub total_put_oi: u64,
    pub atm_iv_call: Option<f64>,
    pub atm_iv_put: Option<f64>,
    /// Combined ATM IV (average of call and put ATM IV).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atm_iv: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skew_near_put_call_iv_ratio: Option<f64>,
    #[serde(default)]
    pub has_iv_data: bool,
    #[serde(default)]
    pub has_liquid_near_money: bool,
    pub max_pain: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expirations_analyzed: Option<usize>,
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
    pub requested_expiry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_expiry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_dte_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_days_to_expiry: Option<i64>,
    #[serde(default)]
    pub auto_selected_expiry: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_reason: Option<String>,
    pub calls: Vec<OptionContract>,
    pub puts: Vec<OptionContract>,
    /// Top-level mirror of `metrics.atm_iv` for ergonomic single-jq access.
    /// In `--all` mode this reflects the nearest non-expired snapshot's ATM IV.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atm_iv: Option<f64>,
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
    pub call_oi: u64,
    pub put_oi: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put_call_ratio_volume: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put_call_ratio_oi: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_pain: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atm_iv: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_monthly: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MultiExpirySummary {
    pub snapshots: Vec<ExpirySnapshot>,
    pub aggregate_volume: u64,
    pub aggregate_oi: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_put_call_ratio: Option<f64>,
    /// Top 3 expirations by OI — where the action concentrates
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oi_concentration: Option<Vec<OiConcentration>>,
    /// IV term structure: ATM IV at each expiry date
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iv_term_structure: Option<Vec<IvTermPoint>>,
    /// Max pain across all expirations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_pain_range: Option<MaxPainRange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OiConcentration {
    pub expiry: String,
    pub days_to_expiry: i64,
    pub oi: u64,
    pub pct_of_total: f64,
    pub is_monthly: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IvTermPoint {
    pub expiry: String,
    pub days_to_expiry: i64,
    pub atm_iv: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaxPainRange {
    pub min_expiry: String,
    pub min_pain: f64,
    pub max_expiry: String,
    pub max_pain: f64,
    pub nearest_monthly_pain: Option<f64>,
    pub nearest_monthly_expiry: Option<String>,
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
    #[serde(skip_serializing, default)]
    pub freshness: Freshness,
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
    #[serde(skip_serializing, default)]
    pub freshness: Freshness,
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
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub freshness_summary: FreshnessSummary,
    #[serde(default, skip_serializing)]
    pub applied_policy: AppliedPolicy,
    #[serde(default, skip_serializing)]
    pub decision_trace: Vec<String>,
    #[serde(default, skip_serializing)]
    pub run_meta: RunMeta,
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
    pub include_sports: bool,
    #[serde(default)]
    pub include_historical: bool,
    #[serde(default)]
    pub stream_refresh: bool,
    #[serde(default)]
    pub refresh_heartbeat_hours: Option<u64>,
    #[serde(default)]
    pub stream_refresh_timeout_secs: Option<u64>,
    #[serde(default)]
    pub strict: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OddsSyncBackfillProfile {
    Fast,
    Balanced,
    Full,
}

impl Default for OddsSyncBackfillProfile {
    fn default() -> Self {
        Self::Balanced
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OddsSyncBaselineQuality {
    Trusted,
    Reset,
    Untrusted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OddsSyncStatus {
    Complete,
    Partial,
}

impl Default for OddsSyncStatus {
    fn default() -> Self {
        Self::Complete
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OddsSyncMode {
    Exhaustive,
    FrontierSample,
    StreamRefresh,
}

impl Default for OddsSyncMode {
    fn default() -> Self {
        Self::Exhaustive
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncCoverage {
    #[serde(default)]
    pub sync_mode: OddsSyncMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_max_pages: Option<usize>,
    pub events_pages_fetched: usize,
    pub events_exhausted: bool,
    pub markets_pages_fetched: usize,
    pub markets_exhausted: bool,
    #[serde(default)]
    pub events_requests: usize,
    #[serde(default)]
    pub markets_requests: usize,
    #[serde(default)]
    pub retry_count_429: usize,
    #[serde(default)]
    pub retry_count_5xx: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_backfill_calls: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_backfill_cap: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_backfill_truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_warning: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<OddsSyncSourceDelta>,
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
pub struct OddsSyncMarketDelta {
    pub source: String,
    pub ticker: String,
    pub title: String,
    pub event_ticker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// One of: new, removed, updated.
    pub change_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_probability_yes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_probability_yes: Option<f64>,
    /// Decimal probability delta on 0..1 scale (current - previous).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability_delta: Option<f64>,
    /// Same delta in percentage points, useful for quick scanning (e.g. +2.50pp).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probability_delta_pct_points: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_yes_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_yes_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_price_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_status: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncSourceDelta {
    pub source: String,
    pub previous_markets: usize,
    pub current_markets: usize,
    pub compared_markets: usize,
    /// Existing markets whose values changed (excludes brand-new and removed markets).
    pub updated_markets: usize,
    /// Churn count (new + removed), useful to separate lifecycle turnover from true updates.
    pub churn_markets: usize,
    pub new_markets: usize,
    pub removed_markets: usize,
    pub changed_markets: usize,
    pub unchanged_markets: usize,
    pub probability_changed_markets: usize,
    pub yes_price_changed_markets: usize,
    pub volume_changed_markets: usize,
    pub status_changed_markets: usize,
    pub baseline_quality: OddsSyncBaselineQuality,
    #[serde(default)]
    pub baseline_reset_applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_reset_reason: Option<String>,
    pub top_probability_moves: Vec<OddsSyncMarketDelta>,
    pub top_yes_price_moves: Vec<OddsSyncMarketDelta>,
    pub top_volume_moves: Vec<OddsSyncMarketDelta>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncDeltaSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_sync_at: Option<DateTime<Utc>>,
    pub current_sync_at: DateTime<Utc>,
    pub previous_markets: usize,
    pub current_markets: usize,
    pub compared_markets: usize,
    /// Existing markets whose values changed (excludes brand-new and removed markets).
    pub updated_markets: usize,
    /// Churn count (new + removed), useful to separate lifecycle turnover from true updates.
    pub churn_markets: usize,
    pub new_markets: usize,
    pub removed_markets: usize,
    pub changed_markets: usize,
    pub unchanged_markets: usize,
    pub probability_changed_markets: usize,
    pub yes_price_changed_markets: usize,
    pub volume_changed_markets: usize,
    pub status_changed_markets: usize,
    pub top_probability_moves: Vec<OddsSyncMarketDelta>,
    pub top_yes_price_moves: Vec<OddsSyncMarketDelta>,
    pub top_volume_moves: Vec<OddsSyncMarketDelta>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncDeltaIndex {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_sync_at: Option<DateTime<Utc>>,
    pub current_sync_at: DateTime<Utc>,
    pub changed_markets: usize,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub market_deltas: std::collections::BTreeMap<String, OddsSyncMarketDelta>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub source_baselines: std::collections::BTreeMap<String, OddsSyncSourceBaseline>,
    pub top_probability_moves: Vec<OddsSyncMarketDelta>,
    pub top_yes_price_moves: Vec<OddsSyncMarketDelta>,
    pub top_volume_moves: Vec<OddsSyncMarketDelta>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OddsSyncSourceBaseline {
    pub baseline_quality: OddsSyncBaselineQuality,
    #[serde(default)]
    pub baseline_reset_applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_reset_reason: Option<String>,
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
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub freshness_summary: FreshnessSummary,
    #[serde(default, skip_serializing)]
    pub applied_policy: AppliedPolicy,
    #[serde(default, skip_serializing)]
    pub decision_trace: Vec<String>,
    #[serde(default, skip_serializing)]
    pub run_meta: RunMeta,
    #[serde(default)]
    pub sync_status: OddsSyncStatus,
    #[serde(default)]
    pub sync_mode: OddsSyncMode,
    pub sources: Vec<OddsSyncSourceResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<OddsSyncSourceResult>,
    pub total_events: usize,
    pub total_markets: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_csv_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_files: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis: Option<OddsSyncAnalysis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<OddsSyncDeltaSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_state_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_delta_index_path: Option<String>,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperMode {
    Simulated,
    LiveLike,
    KalshiDemo,
    PolymarketDemo,
}

impl Default for PaperMode {
    fn default() -> Self {
        Self::Simulated
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperCommand {
    Trade,
    Positions,
    Trades,
    Mark,
    Reset,
}

impl Default for PaperCommand {
    fn default() -> Self {
        Self::Trade
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperProvider {
    Kalshi,
    Polymarket,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperSide {
    Yes,
    No,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperOrderAction {
    Buy,
    Sell,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperRequest {
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub command: PaperCommand,
    #[serde(default)]
    pub mode: PaperMode,
    #[serde(default)]
    pub provider: Option<PaperProvider>,
    #[serde(default)]
    pub market_ticker: Option<String>,
    #[serde(default)]
    pub side: Option<PaperSide>,
    #[serde(default)]
    pub action: Option<PaperOrderAction>,
    #[serde(default)]
    pub quantity: Option<f64>,
    #[serde(default)]
    pub limit_price: Option<f64>,
    #[serde(default)]
    pub starting_cash: Option<f64>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cache_dir: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperTradeFill {
    pub id: String,
    pub ts: DateTime<Utc>,
    pub provider: PaperProvider,
    pub market_ticker: String,
    pub side: PaperSide,
    pub action: PaperOrderAction,
    pub quantity: f64,
    pub price: f64,
    pub notional: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperPosition {
    pub provider: PaperProvider,
    pub market_ticker: String,
    pub side: PaperSide,
    pub quantity: f64,
    pub avg_price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mark_price: Option<f64>,
    pub cost_basis: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrealized_pnl: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperAccountSummary {
    pub cash: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub equity: f64,
    pub positions_open: usize,
    pub trades_total: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperResponse {
    pub generated_at: DateTime<Utc>,
    pub account: String,
    pub command: PaperCommand,
    pub mode: PaperMode,
    pub summary: PaperAccountSummary,
    pub positions: Vec<PaperPosition>,
    pub trades: Vec<PaperTradeFill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_trade: Option<PaperTradeFill>,
    pub state_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidityRequest {
    /// How far back to fetch (e.g. "1y", "2y"). Defaults to 1y internally.
    #[serde(default)]
    pub range: Option<Span>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidityComponent {
    pub series_id: String,
    pub label: String,
    pub latest_value: f64,
    pub latest_date: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidityDataPoint {
    pub date: DateTime<Utc>,
    /// WALCL value (Fed balance sheet total assets, millions USD).
    pub walcl: f64,
    /// WTREGEN value (Treasury General Account, millions USD).
    pub tga: f64,
    /// RRPONTSYD value (Overnight Reverse Repo, millions USD).
    pub rrp: f64,
    /// Net liquidity = WALCL - TGA - RRP (millions USD).
    pub net_liquidity: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidityResponse {
    pub generated_at: DateTime<Utc>,
    pub as_of: DateTime<Utc>,
    pub age_seconds: i64,
    pub components: Vec<LiquidityComponent>,
    pub net_liquidity_latest: f64,
    /// Change from earliest to latest in the series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_liquidity_change: Option<f64>,
    /// Percentage change from earliest to latest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_liquidity_change_pct: Option<f64>,
    pub series: Vec<LiquidityDataPoint>,
    pub coverage_ratio: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuctionsRequest {
    /// Filter by security type: bill, note, bond, tips, frn, or "all".
    #[serde(default)]
    pub security_type: Option<String>,
    /// How many recent auctions to return. Default 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuctionResult {
    pub cusip: String,
    pub security_type: String,
    pub security_term: String,
    pub auction_date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maturity_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_yield: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bid_to_cover_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_accepted: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tendered: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_bidder_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indirect_bidder_pct: Option<f64>,
    /// Tail = high_yield - when_issued_yield. Positive tail = weak demand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail_bps: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuctionsResponse {
    pub generated_at: DateTime<Utc>,
    pub auctions: Vec<AuctionResult>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CotRequest {
    /// Search query to filter by contract name (e.g. "gold", "crude oil", "10y note").
    #[serde(default)]
    pub query: Option<String>,
    /// How many weeks of data to fetch. Default 12.
    #[serde(default)]
    pub weeks: Option<usize>,
    /// Report type: "disaggregated" (commodities) or "financial" (rates/FX/equity index). Default "disaggregated".
    #[serde(default)]
    pub report: Option<String>,
    /// Max number of distinct contracts to return. Default 15.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CotPosition {
    pub contract_name: String,
    pub report_date: String,
    pub open_interest: i64,
    /// Managed money (disaggregated) or leveraged funds (financial) net position.
    pub spec_net: i64,
    pub spec_long: i64,
    pub spec_short: i64,
    /// Producer/merchant (disaggregated) or dealer (financial) net position.
    pub commercial_net: i64,
    pub commercial_long: i64,
    pub commercial_short: i64,
    /// Spec net as percentage of open interest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_net_pct_oi: Option<f64>,
    /// Week-over-week change in spec net.
    /// Always serialized — emits `null` for the OLDEST returned week (no prior to diff against).
    pub spec_net_change: Option<i64>,
    /// Report family: "disaggregated" or "financial" (TFF).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_family: Option<String>,
    /// "FutOnly" or "Combined" (futures + options).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub futonly_or_combined: Option<String>,
    /// Underlying commodity name from CFTC (e.g. "CRUDE OIL", "GOLD").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commodity_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CotResponse {
    pub generated_at: DateTime<Utc>,
    pub report_type: String,
    pub positions: Vec<CotPosition>,
    pub contracts_found: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The date of the latest position data (CFTC reports as-of Tuesday)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_as_of: Option<String>,
    /// When this data was released by CFTC (typically the following Friday 3:30 PM ET)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_on: Option<String>,
    /// When the next CFTC report is expected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_release: Option<String>,
    /// How stale the data is in human-readable form
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staleness_days: Option<i64>,
}

// ──────────────────────────────────────────────────────────────
// NY Fed Markets API
// ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NyFedRequest {
    pub kind: String, // rates | rrp | soma | dealers
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NyFedRateItem {
    pub name: String,
    pub rate_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentile_1: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentile_25: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentile_75: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentile_99: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_billions: Option<f64>,
    pub effective_date: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NyFedRrpItem {
    pub effective_date: String,
    pub total_billions: f64,
    pub counterparty_count: u32,
    pub rate_pct: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NyFedSomaItem {
    pub security_type: String,
    pub par_value_billions: f64,
    pub percent_of_total: f64,
    pub as_of_date: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NyFedDealerItem {
    pub description: String,
    /// Always serialized — emits `null` when the upstream NY Fed API returns no
    /// value for this series on this date (e.g. PDSORA-UTSETTOT repo was retired
    /// and now ships "*"). Previously defaulted to 0.0, which silently looked
    /// like "zero repo" instead of "missing".
    pub value_millions: Option<f64>,
    pub report_date: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NyFedResponse {
    pub generated_at: DateTime<Utc>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rates: Option<Vec<NyFedRateItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rrp: Option<Vec<NyFedRrpItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soma: Option<Vec<NyFedSomaItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dealers: Option<Vec<NyFedDealerItem>>,
}

// ──────────────────────────────────────────────────────────────
// CBOE Volatility Indices / Term Structure
// ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolSurfaceRequest {
    pub symbols: Option<Vec<String>>,
    pub history: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolSurfaceDataPoint {
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// "point" when CBOE only ships a single value for this row (VVIX/OVX/GVZ/SKEW).
    /// In that case open == high == low == close and downstream rendering should
    /// skip candlestick wicks. Same convention as the Candle.kind field for
    /// Cleveland Fed nowcast series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolSurfaceIndex {
    pub symbol: String,
    pub latest: VolSurfaceDataPoint,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<VolSurfaceDataPoint>,
    /// "point" when CBOE only ships a single value per day for this symbol
    /// (VVIX, OVX, GVZ, SKEW). Absent for true OHLC series (VIX, VIX9D, etc.).
    /// Same convention as Cleveland Fed nowcast point series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolSurfaceResponse {
    pub generated_at: DateTime<Utc>,
    pub indices: Vec<VolSurfaceIndex>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ──────────────────────────────────────────────────────────────
// OFR Financial Stress Index
// ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StressRequest {
    pub range_days: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StressDataPoint {
    pub date: String,
    pub fsi: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_assets: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volatility: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StressResponse {
    pub generated_at: DateTime<Utc>,
    pub latest: StressDataPoint,
    pub history: Vec<StressDataPoint>,
    pub count: usize,
}

// ──────────────────────────────────────────────────────────────
// Treasury Fiscal Data API
// ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FiscalRequest {
    pub kind: String, // debt | statement | interest
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FiscalDebtItem {
    pub record_date: String,
    pub total_debt_billions: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_debt_billions: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intragovernmental_billions: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FiscalStatementItem {
    pub record_date: String,
    pub account: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_today_bal: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_today_bal: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FiscalInterestItem {
    pub record_date: String,
    pub security_desc: String,
    pub avg_interest_rate_pct: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FiscalResponse {
    pub generated_at: DateTime<Utc>,
    pub kind: String,
    /// Release cadence of this series: "daily" (debt, statement) or "monthly" (interest).
    /// Without this, the monthly `interest` feed (≈2-3 weeks stale at release) looks identical
    /// to the daily debt/statement feeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_cadence: Option<String>,
    /// Date of the latest record returned (YYYY-MM-DD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_as_of: Option<String>,
    /// Days between data_as_of and the time of the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staleness_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debt: Option<Vec<FiscalDebtItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<Vec<FiscalStatementItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interest: Option<Vec<FiscalInterestItem>>,
}
