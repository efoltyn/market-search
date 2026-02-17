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

    // Simple extraction: find sentences with numbers, dates, or key terms
    let sentences: Vec<&str> = req
        .content
        .split(|c| c == '.' || c == '!' || c == '?')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && s.len() > 20)
        .collect();

    let mut scored: Vec<(usize, &str)> = sentences
        .iter()
        .map(|&s| {
            let mut score = 0usize;

            // Boost sentences with numbers
            if s.chars().any(|c| c.is_ascii_digit()) {
                score += 3;
            }

            // Boost sentences with currency symbols
            if s.contains('$') || s.contains('%') {
                score += 2;
            }

            // Boost sentences with key financial terms
            let lower = s.to_lowercase();
            for term in &[
                "revenue", "profit", "loss", "growth", "decline", "increase", "decrease",
                "guidance", "forecast", "earnings", "quarter", "year",
            ] {
                if lower.contains(term) {
                    score += 1;
                }
            }

            // Boost if matches focus
            if let Some(ref focus) = req.focus {
                if lower.contains(&focus.to_lowercase()) {
                    score += 5;
                }
            }

            // Penalize very long sentences
            if s.len() > 300 {
                score = score.saturating_sub(1);
            }

            (score, s)
        })
        .collect();

    // Sort by score descending
    scored.sort_by(|a, b| b.0.cmp(&a.0));

    // Take top N
    let bullets: Vec<String> = scored
        .into_iter()
        .take(req.bullets)
        .filter(|(score, _)| *score > 0)
        .map(|(_, s)| {
            // Truncate if too long
            if s.len() > 200 {
                format!("{}...", &s[..197])
            } else {
                s.to_string()
            }
        })
        .collect();

    Ok(ExtractResponse {
        source: req.source,
        bullets,
        word_count,
        extracted_at: Utc::now(),
    })
}

/// Fetch URL and extract content
pub async fn extract_from_url(
    url: &str,
    bullets: usize,
    focus: Option<String>,
) -> Result<ExtractResponse> {
    // Fetch and extract readable content
    let article = crate::web::providers::read::read_url(url).await?;

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
