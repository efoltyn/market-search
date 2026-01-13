use crate::{Error, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

const MAX_POINTS_PER_TICKER_DEFAULT: usize = 5_000;
const SEC_COMPANY_TICKERS_TTL_SECS: u64 = 60 * 60 * 24 * 7; // 7 days
const SEC_SUBMISSIONS_TTL_SECS: u64 = 60 * 60 * 24; // 1 day
const SEC_DEFAULT_TEXT_MAX_CHARS: usize = 10_000;
const YAHOO_SEARCH_URL: &str = "https://query2.finance.yahoo.com/v1/finance/search";
const YAHOO_QUOTE_SUMMARY_URL: &str = "https://query2.finance.yahoo.com/v7/finance/quoteSummary";

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
                "invalid span '{raw}' (expected like 10m, 1h, 30d, 12mo, 5y)"
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
            "m" | "min" | "mins" | "minute" | "minutes" => SpanUnit::Minute,
            "h" | "hr" | "hrs" | "hour" | "hours" => SpanUnit::Hour,
            "d" | "day" | "days" => SpanUnit::Day,
            "w" | "wk" | "wks" | "week" | "weeks" => SpanUnit::Week,
            "mo" | "mon" | "month" | "months" => SpanUnit::Month,
            "y" | "yr" | "yrs" | "year" | "years" => SpanUnit::Year,
            _ => {
                return Err(Error::InvalidInput(format!(
                    "invalid span unit '{unit_raw}' (expected m,h,d,w,mo,y)"
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
            SpanUnit::Minute => "m",
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
        resp.cache = Some(CacheInfo {
            hit: true,
            path: cache_path.display().to_string(),
            key: cache_key,
        });
        return Ok(resp);
    }

    std::fs::create_dir_all(cache_path.parent().unwrap_or(cache_dir))?;

    let generated_at = Utc::now();
    let series = match req.provider {
        ProviderKind::Mock => generate_mock_series(&tickers, start, end, step),
        ProviderKind::Yahoo => {
            fetch_yahoo_series(&tickers, start, end, req.granularity, max_points).await?
        }
        ProviderKind::Fred => fetch_fred_series(&tickers, start, end, req.granularity).await?,
    };

    let resp = TimeseriesResponse {
        provider: req.provider,
        tickers: tickers.clone(),
        granularity: req.granularity,
        range: req.range,
        start,
        end,
        generated_at,
        series,
        cache: Some(CacheInfo {
            hit: false,
            path: cache_path.display().to_string(),
            key: cache_key.clone(),
        }),
    };

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

    Ok(SnapshotResponse {
        provider: req.provider,
        tickers,
        generated_at,
        snapshots,
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
                doc.text_excerpt = Some(truncate_chars(&text, excerpt_max));
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
        if let Ok(mut svec) = series {
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
) -> Result<Vec<TickerSeries>> {
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

    for ticker in tickers {
        let quotes =
            yahoo_fetch_quotes_retry(ticker, start_ts, end_ts, interval, include_prepost).await?;

        let mut candles = Vec::with_capacity(quotes.len());
        for q in quotes {
            let Some(t) = Utc.timestamp_opt(q.timestamp as i64, 0).single() else {
                return Err(Error::Provider(format!(
                    "yahoo invalid timestamp for '{ticker}': {}",
                    q.timestamp
                )));
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

        candles.sort_by_key(|c| c.t);
        let candles = if requested_step == base_step {
            candles
        } else {
            resample_candles(&candles, start, requested_step)
        };

        out.push(TickerSeries {
            ticker: ticker.clone(),
            candles,
        });
    }

    Ok(out)
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
) -> Result<Vec<TickerSeries>> {
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
    for series_id in tickers {
        let resp = client
            .get("https://fred.stlouisfed.org/graph/fredgraph.csv")
            .query(&[
                ("id", series_id.as_str()),
                ("cosd", start_date.as_str()),
                ("coed", end_date.as_str()),
            ])
            .send()
            .await
            .map_err(|e| Error::Provider(format!("fred fetch failed for '{series_id}': {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "fred fetch failed for '{series_id}': http {}",
                resp.status()
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| Error::Provider(format!("fred read failed for '{series_id}': {e}")))?;

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

            let date = chrono::NaiveDate::parse_from_str(date_raw, "%Y-%m-%d")
                .map_err(|_| Error::Provider(format!("fred invalid date '{date_raw}'")))?;
            let t = DateTime::<Utc>::from_naive_utc_and_offset(
                date.and_hms_opt(0, 0, 0)
                    .ok_or_else(|| Error::Provider("fred invalid datetime".to_string()))?,
                Utc,
            );
            if t < start || t > end {
                continue;
            }

            let v: f64 = val_raw
                .parse()
                .map_err(|_| Error::Provider(format!("fred invalid value '{val_raw}'")))?;
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

        out.push(TickerSeries {
            ticker: series_id.clone(),
            candles,
        });
    }

    Ok(out)
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

fn html_to_text(raw: &str) -> String {
    // Best-effort HTML -> text. This isn't a full parser, but works well enough for SEC filings.
    let s = raw.replace("\r\n", "\n");
    let mut out = String::with_capacity(s.len().min(128_000));
    let mut in_tag = false;
    let mut tag_buf = String::new();

    for ch in s.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag = tag_buf.trim().to_ascii_lowercase();
                // Newline-ish tags
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
        out.push(ch);
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
        // Provide a safe default that identifies the tool as an agent.
        // SEC requires contact info, so we use a placeholder that encourages the user to set their own.
        return Ok("Eli-Terminal-Agent/0.1.0 (https://github.com/elifoltyn/eli; research-mode)".to_string());
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
