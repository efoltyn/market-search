use crate::web::WebHit;
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use spider::website::Website;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tracing::{info, warn};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrawlRequest {
    pub url: String,
    pub max_pages: Option<usize>,
    pub respect_robots: bool,
    pub include_subdomains: bool,
    pub include_sitemap: bool,
    pub smart_mode: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrawlResponse {
    pub base_url: String,
    pub generated_at: DateTime<Utc>,
    pub crawl_mode: String,
    pub pages_crawled: usize,
    pub pages: Vec<CrawledPage>,
    pub duration_ms: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrawledPage {
    pub url: String,
    pub title: Option<String>,
    pub text_preview: String,
    pub links_found: usize,
    pub fetched_at: DateTime<Utc>,
}

impl Default for CrawlRequest {
    fn default() -> Self {
        Self {
            url: String::new(),
            max_pages: Some(50),
            respect_robots: true,
            include_subdomains: false,
            include_sitemap: false,
            smart_mode: false,
        }
    }
}
