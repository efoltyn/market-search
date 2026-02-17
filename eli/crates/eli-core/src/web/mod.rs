use chrono::{DateTime, Utc};
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchResponse {
    pub hits: Vec<WebHit>,
    /// Alias for `hits` — agents expect `results` key.
    pub results: Vec<WebHit>,
}

pub struct ScoringPipeline;

impl ScoringPipeline {
    pub fn process(mut hits: Vec<WebHit>) -> Vec<WebHit> {
        // 1. Dedupe by URL
        let mut seen = std::collections::HashSet::new();
        hits.retain(|h| seen.insert(h.url.clone()));

        // 2. Sort by score (descending)
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        hits
    }
}
