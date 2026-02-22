use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

pub mod providers;

// Re-export crawl types for convenience
pub use providers::crawl::{crawl_website, CrawlRequest, CrawlResponse, CrawledPage};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source: String,
    pub score: f32,
    pub published: Option<DateTime<Utc>>,
    pub provenance: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebReadFetchStatus {
    Success,
    Partial,
    Blocked,
    Error,
}

impl WebReadFetchStatus {
    pub fn readability_score(self) -> f64 {
        match self {
            Self::Success => 1.0,
            Self::Partial => 0.5,
            Self::Blocked | Self::Error => 0.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebReadAttempt {
    pub attempt: usize,
    pub method: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extractor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebReadResponse {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    pub title: String,
    pub text: String,
    pub fetch_status: WebReadFetchStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    pub attempts: Vec<WebReadAttempt>,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebReadBatchResponse {
    pub mode: String,
    pub requested: usize,
    pub deduped: usize,
    pub completed: usize,
    pub success_count: usize,
    pub partial_count: usize,
    pub blocked_count: usize,
    pub error_count: usize,
    pub results: Vec<WebReadResponse>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebReadProbeSummary {
    pub fetch_status: WebReadFetchStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    pub attempts_count: usize,
    pub text_chars: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchMode {
    Auto,
    News,
    Finance,
    Research,
    Tech,
    Encyclopedia,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchRecency {
    Day,
    Week,
    Month,
    Year,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchRequest {
    pub query: String,
    pub mode: WebSearchMode,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub exclude_domains: Vec<String>,
    #[serde(default)]
    pub recency: Option<WebSearchRecency>,
    #[serde(default)]
    pub since: Option<NaiveDate>,
    #[serde(default)]
    pub until: Option<NaiveDate>,
    #[serde(default = "default_web_search_top")]
    pub top: usize,
    #[serde(default = "default_web_search_probe_top")]
    pub probe_top: usize,
    #[serde(default = "default_web_search_parallel")]
    pub max_parallel: usize,
    #[serde(default)]
    pub track_key: Option<String>,
}

fn default_web_search_top() -> usize {
    15
}

fn default_web_search_probe_top() -> usize {
    4
}

fn default_web_search_parallel() -> usize {
    6
}

impl Default for WebSearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            mode: WebSearchMode::Auto,
            domains: Vec::new(),
            exclude_domains: Vec::new(),
            recency: None,
            since: None,
            until: None,
            top: default_web_search_top(),
            probe_top: default_web_search_probe_top(),
            max_parallel: default_web_search_parallel(),
            track_key: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebProviderAttempt {
    pub provider: String,
    pub ok: bool,
    pub duration_ms: u64,
    pub raw_hits: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchScores {
    pub lexical: f64,
    pub freshness: f64,
    pub source_trust: f64,
    pub readability: f64,
    pub final_score: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchItem {
    pub rank: usize,
    pub title: String,
    pub url: String,
    pub domain: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<DateTime<Utc>>,
    pub source: String,
    pub provenance: String,
    pub scores: WebSearchScores,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_probe: Option<WebReadProbeSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchStats {
    pub total_raw_hits: usize,
    pub deduped_hits: usize,
    pub after_domain_filter: usize,
    pub after_time_filter: usize,
    pub returned_items: usize,
    pub probed_items: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebRankShift {
    pub url: String,
    pub from_rank: usize,
    pub to_rank: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchRunDelta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rank_up: Vec<WebRankShift>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rank_down: Vec<WebRankShift>,
    pub unchanged: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchResponse {
    pub query: String,
    pub mode: WebSearchMode,
    pub generated_at: DateTime<Utc>,
    pub providers: Vec<WebProviderAttempt>,
    pub items: Vec<WebSearchItem>,
    pub stats: WebSearchStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_delta: Option<WebSearchRunDelta>,
}
