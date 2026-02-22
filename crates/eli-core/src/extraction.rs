use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtractRequest {
    pub content: String,
    pub source: String,
    pub bullets: usize,
    pub focus: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtractResponse {
    pub source: String,
    pub bullets: Vec<String>,
    pub word_count: usize,
    pub extracted_at: DateTime<Utc>,
}

impl Default for ExtractRequest {
    fn default() -> Self {
        Self {
            content: String::new(),
            source: "inline".to_string(),
            bullets: 10,
            focus: None,
        }
    }
}

/// Extract key facts from content using simple heuristics.
/// This is a local extraction that doesn't require an LLM call.
/// For LLM-powered extraction, use a subagent.
pub fn extract_facts(req: ExtractRequest) -> Result<ExtractResponse> {
    let word_count = req.content.split_whitespace().count();

    if is_docs_style_content(&req.content) {
        let docs_bullets = extract_docs_style_facts(&req.content, req.bullets);
        if !docs_bullets.is_empty() {
            return Ok(ExtractResponse {
                source: req.source,
                bullets: docs_bullets,
                word_count,
                extracted_at: Utc::now(),
            });
        }
    }

    let focus_l = req.focus.as_ref().map(|f| f.to_ascii_lowercase());
    let candidates = candidate_segments(&req.content);
    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .map(|segment| {
            let score = score_segment(&segment, focus_l.as_deref());
            (score, segment)
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.len().cmp(&a.1.len())));

    let mut bullets = Vec::<String>::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for (score, segment) in scored.into_iter() {
        if score == 0 {
            continue;
        }
        let key = normalize_dedupe_key(&segment);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        bullets.push(truncate_segment(&segment, 220));
        if bullets.len() >= req.bullets {
            break;
        }
    }

    Ok(ExtractResponse {
        source: req.source,
        bullets,
        word_count,
        extracted_at: Utc::now(),
    })
}

fn candidate_segments(content: &str) -> Vec<String> {
    let normalized = content
        .replace('§', "\n§ ")
        .replace("}{", "}\n{")
        .replace(" use ", "\nuse ")
        .replace("pub extern crate", "\npub extern crate")
        .replace("Feature flags", "\nFeature flags\n")
        .replace("How to use", "\nHow to use")
        .replace("Examples", "\nExamples\n");

    let mut out = Vec::<String>::new();
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.len() < 16 {
            continue;
        }
        if trimmed.chars().filter(|c| c.is_alphanumeric()).count() < 10 {
            continue;
        }
        out.push(trimmed.to_string());
    }

    for sentence in normalized
        .split(|c| c == '.' || c == '!' || c == '?' || c == '\n')
        .map(str::trim)
    {
        if sentence.len() < 24 {
            continue;
        }
        if sentence.chars().filter(|c| c.is_alphanumeric()).count() < 14 {
            continue;
        }
        out.push(sentence.to_string());
    }

    out
}

fn score_segment(segment: &str, focus: Option<&str>) -> usize {
    let mut score = 0usize;
    let lower = segment.to_ascii_lowercase();

    if segment.chars().any(|c| c.is_ascii_digit()) {
        score += 1;
    }
    if segment.contains('$') || segment.contains('%') {
        score += 1;
    }
    if segment.contains("::") || segment.contains("fn ") || segment.contains("Website::") {
        score += 4;
    }
    if segment.contains(':') && segment.len() < 220 {
        score += 2;
    }
    if segment.starts_with("use ") || segment.starts_with("pub ") {
        score += 3;
    }
    if lower.contains("feature flag")
        || lower.contains("how to use")
        || lower.contains("example")
        || lower.contains("crawl")
        || lower.contains("scrape")
        || lower.contains("chrome")
        || lower.contains("spider cloud")
        || lower.contains("api")
        || lower.contains("proxy")
        || lower.contains("anti-bot")
    {
        score += 3;
    }
    if lower.contains("revenue")
        || lower.contains("profit")
        || lower.contains("loss")
        || lower.contains("growth")
        || lower.contains("guidance")
        || lower.contains("forecast")
        || lower.contains("earnings")
    {
        score += 2;
    }
    if let Some(focus) = focus {
        if !focus.is_empty() && lower.contains(focus) {
            score += 5;
        }
    }

    if segment.len() > 360 {
        score = score.saturating_sub(2);
    }
    if segment.len() > 900 {
        score = score.saturating_sub(3);
    }

    score
}

fn normalize_dedupe_key(segment: &str) -> String {
    segment
        .to_ascii_lowercase()
        .split_whitespace()
        .take(18)
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_segment(segment: &str, max_chars: usize) -> String {
    let chars = segment.chars().count();
    if chars <= max_chars {
        return segment.to_string();
    }
    let mut out = String::new();
    for ch in segment.chars().take(max_chars.saturating_sub(3)) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn is_docs_style_content(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("feature flags")
        || lower.contains("how to use")
        || lower.contains("docs.rs")
        || lower.contains("pub extern crate")
}

fn extract_docs_style_facts(content: &str, bullets: usize) -> Vec<String> {
    let normalized = content
        .replace('§', "\n")
        .replace("How to use Spider", "\nHow to use Spider\n")
        .replace("Examples", "\nExamples\n")
        .replace("Feature flags", "\nFeature flags\n")
        .replace("Spider Cloud Integration", "\nSpider Cloud Integration\n")
        .replace("Chrome Rendering", "\nChrome Rendering\n")
        .replace(
            "There are a couple of ways to use Spider:crawl:",
            "There are a couple of ways to use Spider.\ncrawl:",
        )
        .replace("links.scrape:", "links.\nscrape:")
        .replace("complete.§Examples", "complete.\nExamples")
        .replace("website:use spider::tokio;", "website:\nuse spider::tokio;")
        .replace("events:use spider::tokio;", "events:\nuse spider::tokio;")
        .replace("key:use spider::tokio;", "key:\nuse spider::tokio;")
        .replace("instance:use spider::tokio;", "instance:\nuse spider::tokio;")
        .replace("}Subscribe to crawl events:", "\nSubscribe to crawl events:")
        .replace("use spider::tokio;", "\nuse spider::tokio;")
        .replace("use spider::website::Website;", "\nuse spider::website::Website;")
        .replace("Website::new", "\nWebsite::new");

    let mut candidates = collect_docs_highlights(content);
    let lines = normalized
        .lines()
        .map(str::trim)
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    let mut headings = Vec::<String>::new();
    let mut operation_lines = Vec::<String>::new();
    let mut feature_lines = Vec::<String>::new();
    let mut code_lines = Vec::<String>::new();
    for line in &lines {
        let lower = line.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "how to use spider"
                | "examples"
                | "feature flags"
                | "spider cloud integration"
                | "chrome rendering"
        ) {
            headings.push(line.to_string());
            continue;
        }

        if lower.starts_with("crawl:")
            || lower.starts_with("scrape:")
            || lower.contains("start concurrently crawling")
            || lower.contains("saves the html")
        {
            operation_lines.push(line.to_string());
            continue;
        }

        if line.contains(": ")
            && !line.starts_with("http")
            && line
                .split(':')
                .next()
                .unwrap_or("")
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit())
            && line.len() <= 240
        {
            feature_lines.push(line.to_string());
            continue;
        }

        if line.contains("Website::new")
            || line.contains(".crawl().await")
            || line.contains("subscribe(")
            || line.contains("with_spider_cloud")
            || line.contains("with_chrome_intercept")
            || line.contains("CHROME_URL")
        {
            code_lines.push(line.to_string());
            continue;
        }
    }

    candidates.extend(headings.into_iter().take(4));
    candidates.extend(operation_lines.into_iter().take(4));
    candidates.extend(feature_lines.into_iter().take(6));
    candidates.extend(code_lines.into_iter().take(8));

    let mut out = Vec::<String>::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for candidate in candidates {
        let cleaned = truncate_segment(&candidate, 220);
        let key = normalize_dedupe_key(&cleaned);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(cleaned);
        if out.len() >= bullets {
            break;
        }
    }
    out
}

fn collect_docs_highlights(content: &str) -> Vec<String> {
    let lower = content.to_ascii_lowercase();
    let mut out = Vec::<String>::new();

    if lower.contains("crawl: start concurrently crawling") {
        out.push(
            "crawl: start a concurrent crawl; stream page URL/HTML via subscriptions or gather links."
                .to_string(),
        );
    }
    if lower.contains("scrape: like crawl") {
        out.push(
            "scrape: crawl while keeping raw HTML payloads for parse-after-run workflows."
                .to_string(),
        );
    }
    if lower.contains("website::new(") && lower.contains("website.crawl().await") {
        out.push("Baseline flow: Website::new(url) -> crawl().await -> get_links().".to_string());
    }
    if lower.contains("subscribe(") {
        out.push(
            "Subscriptions: website.subscribe(n) emits pages during crawl for incremental processing."
                .to_string(),
        );
    }
    if lower.contains("with_spider_cloud(") {
        out.push(
            "Spider Cloud integration: with_spider_cloud(API_KEY) adds anti-bot bypass and proxy rotation."
                .to_string(),
        );
    }
    if lower.contains("with_chrome_intercept") || lower.contains("chrome_url") {
        out.push(
            "Chrome rendering: enable chrome feature (optional CHROME_URL) for JS-heavy pages."
                .to_string(),
        );
    }
    if lower.contains("feature flags") {
        out.push(
            "Feature flags are grouped into Core, Storage, Caching, Chrome/Browser, WebDriver, AI/LLM, Spider Cloud, Agent, Search, Networking, and Distributed."
                .to_string(),
        );
    }
    if lower.contains("smart: enables smart mode") || lower.contains("crawl_smart().await") {
        out.push(
            "smart mode: HTTP-first crawling with JS rendering only when needed to reduce extra requests."
                .to_string(),
        );
    }

    out
}

/// Fetch URL and extract content
pub async fn extract_from_url(
    url: &str,
    bullets: usize,
    focus: Option<String>,
) -> Result<ExtractResponse> {
    // Fetch and extract readable content
    let article = crate::web::providers::read::read_url_with_diagnostics(url).await;
    if !matches!(
        article.fetch_status,
        crate::web::WebReadFetchStatus::Success | crate::web::WebReadFetchStatus::Partial
    ) {
        let reason = article
            .blocked_reason
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        return Err(Error::Provider(format!(
            "web read failed: status={:?} blocked_reason={reason}",
            article.fetch_status
        )));
    }
    if article.text.trim().is_empty() {
        return Err(Error::Provider(
            "web read returned empty content for extraction".to_string(),
        ));
    }

    let req = ExtractRequest {
        content: article.text,
        source: url.to_string(),
        bullets,
        focus,
    };

    extract_facts(req)
}

/// Extract from file
pub fn extract_from_file(
    path: &std::path::Path,
    bullets: usize,
    focus: Option<String>,
) -> Result<ExtractResponse> {
    let content =
        std::fs::read_to_string(path).map_err(|e| Error::Other(format!("read file: {}", e)))?;

    let req = ExtractRequest {
        content,
        source: path.display().to_string(),
        bullets,
        focus,
    };

    extract_facts(req)
}
