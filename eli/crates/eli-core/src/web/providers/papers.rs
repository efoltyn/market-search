use crate::web::WebHit;
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

pub async fn search_papers(query: &str) -> Result<Vec<WebHit>> {
    let mut hits = Vec::new();

    // 1. OpenAlex
    if let Ok(mut openalex_hits) = search_openalex(query).await {
        hits.append(&mut openalex_hits);
    }

    // 2. ArXiv
    if let Ok(mut arxiv_hits) = search_arxiv(query).await {
        hits.append(&mut arxiv_hits);
    }

    Ok(hits)
}

async fn search_openalex(query: &str) -> Result<Vec<WebHit>> {
    let client = reqwest::Client::builder()
        .user_agent("Eli-Terminal-Agent/0.1.0 (https://github.com/efoltyn/eli; research-mode)")
        .build()
        .map_err(|e| Error::Provider(format!("openalex client init failed: {e}")))?;

    let url = format!(
        "https://api.openalex.org/works?search={}",
        urlencoding::encode(query)
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("openalex fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "openalex fetch failed: http {}",
            resp.status()
        )));
    }

    #[derive(Deserialize)]
    struct OpenAlexResponse {
        results: Vec<OpenAlexWork>,
    }

    #[derive(Deserialize)]
    struct OpenAlexWork {
        id: String,
        display_name: Option<String>,
        publication_date: Option<String>,
        relevance_score: Option<f32>,
    }

    let oa: OpenAlexResponse = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("openalex parse failed: {e}")))?;

    let hits = oa
        .results
        .into_iter()
        .map(|w| WebHit {
            title: w.display_name.unwrap_or_else(|| "Untitled".to_string()),
            url: w.id,
            snippet: "".to_string(), // OpenAlex snippets are in different endpoints
            source: "OpenAlex".to_string(),
            score: w.relevance_score.unwrap_or(0.5) / 100.0, // OpenAlex score is usually high
            published: w.publication_date.and_then(|d| {
                DateTime::parse_from_rfc3339(&format!("{}T00:00:00Z", d))
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            provenance: "scholarly".to_string(),
        })
        .collect();

    Ok(hits)
}

async fn search_arxiv(query: &str) -> Result<Vec<WebHit>> {
    let url = format!(
        "http://export.arxiv.org/api/query?search_query=all:{}&max_results=10",
        urlencoding::encode(query)
    );
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| Error::Provider(format!("arxiv fetch failed: {e}")))?;

    let body = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("arxiv read failed: {e}")))?;

    // Minimal manual XML parsing for arXiv to avoid heavy dependencies
    let mut hits = Vec::new();
    for entry in body.split("<entry>").skip(1) {
        let title = entry
            .split("<title>")
            .nth(1)
            .and_then(|s| s.split("</title>").next())
            .unwrap_or("Untitled")
            .trim();
        let id = entry
            .split("<id>")
            .nth(1)
            .and_then(|s| s.split("</id>").next())
            .unwrap_or("")
            .trim();
        let summary = entry
            .split("<summary>")
            .nth(1)
            .and_then(|s| s.split("</summary>").next())
            .unwrap_or("")
            .trim();
        let published_raw = entry
            .split("<published>")
            .nth(1)
            .and_then(|s| s.split("</published>").next())
            .unwrap_or("")
            .trim();

        hits.push(WebHit {
            title: title.to_string(),
            url: id.to_string(),
            snippet: summary.chars().take(200).collect(),
            source: "arXiv".to_string(),
            score: 0.8, // arXive is high signal
            published: DateTime::parse_from_rfc3339(published_raw)
                .ok()
                .map(|dt| dt.with_timezone(&Utc)),
            provenance: "preprint".to_string(),
        });
    }

    Ok(hits)
}
