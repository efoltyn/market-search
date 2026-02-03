use crate::{Error, Result};
use crate::web::WebHit;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use spider::tokio;
use spider::website::Website;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrawlRequest {
    pub url: String,
    pub max_pages: Option<usize>,
    pub respect_robots: bool,
    pub include_subdomains: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrawlResponse {
    pub base_url: String,
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
}

impl Default for CrawlRequest {
    fn default() -> Self {
        Self {
            url: String::new(),
            max_pages: Some(50),
            respect_robots: true,
            include_subdomains: false,
        }
    }
}

/// Crawl a website and return discovered pages with content previews.
pub async fn crawl_website(req: CrawlRequest) -> Result<CrawlResponse> {
    let start = std::time::Instant::now();

    let mut website = Website::new(&req.url);

    // Configure crawl behavior
    website.with_respect_robots_txt(req.respect_robots);

    if !req.include_subdomains {
        website.with_subdomains(false);
    }

    // Set up page collection
    let max_pages = req.max_pages.unwrap_or(50);
    let pages: Arc<Mutex<Vec<CrawledPage>>> = Arc::new(Mutex::new(Vec::new()));
    let pages_clone = Arc::clone(&pages);

    // Subscribe to crawl events
    let mut rx = website.subscribe(16).ok_or_else(|| {
        Error::Provider("Failed to subscribe to crawl events".to_string())
    })?;

    // Spawn task to collect pages
    let collector = tokio::spawn(async move {
        while let Ok(page) = rx.recv().await {
            let mut pages_guard = pages_clone.lock().await;
            if pages_guard.len() >= max_pages {
                break;
            }

            let url = page.get_url().to_string();
            let html = page.get_html();

            // Extract title and text preview
            let (title, text_preview, links_count) = extract_page_info(&html);

            pages_guard.push(CrawledPage {
                url,
                title,
                text_preview,
                links_found: links_count,
            });
        }
    });

    // Run the crawl
    website.crawl().await;

    // Wait for collector to finish
    let _ = collector.await;

    let pages_result = pages.lock().await.clone();
    let duration_ms = start.elapsed().as_millis();

    Ok(CrawlResponse {
        base_url: req.url,
        pages_crawled: pages_result.len(),
        pages: pages_result,
        duration_ms,
    })
}

/// Extract title, text preview, and link count from HTML
fn extract_page_info(html: &str) -> (Option<String>, String, usize) {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);

    // Extract title
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|el| el.text().collect::<Vec<_>>().join("").trim().to_string());

    // Extract body text (truncated preview)
    let body_text = Selector::parse("body")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|el| {
            el.text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .take(100)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    // Count links
    let links_count = Selector::parse("a[href]")
        .ok()
        .map(|sel| document.select(&sel).count())
        .unwrap_or(0);

    (title, body_text, links_count)
}

/// Convert crawl results to WebHit format for unified scoring
pub fn crawl_to_hits(response: &CrawlResponse) -> Vec<WebHit> {
    response.pages.iter().map(|page| {
        WebHit {
            title: page.title.clone().unwrap_or_else(|| page.url.clone()),
            url: page.url.clone(),
            snippet: page.text_preview.clone(),
            source: "Spider Crawl".to_string(),
            score: 1.0,
            published: None,
            provenance: "crawl".to_string(),
        }
    }).collect()
}
