use crate::{Error, Result};
use crate::web::WebHit;
use chrono::DateTime;
use serde::Deserialize;

pub async fn search_community(query: &str) -> Result<Vec<WebHit>> {
    let client = reqwest::Client::builder()
        .user_agent("Eli-Terminal-Agent/0.1.0 (https://github.com/efoltyn/eli; research-mode)")
        .build()
        .map_err(|e| Error::Provider(format!("community client init failed: {e}")))?;

    // StackExchange API - search/advanced
    let url = format!(
        "https://api.stackexchange.com/2.3/search/advanced?order=desc&sort=relevance&q={}&site=stackoverflow",
        urlencoding::encode(query)
    );

    let resp = client.get(&url).send().await
        .map_err(|e| Error::Provider(format!("stackexchange fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!("stackexchange fetch failed: http {}", resp.status())));
    }

    #[derive(Deserialize)]
    struct SEResponse {
        items: Vec<SEItem>,
    }

    #[derive(Deserialize)]
    struct SEItem {
        title: String,
        link: String,
        score: i32,
        creation_date: i64,
        is_answered: bool,
    }

    let se_resp: SEResponse = resp.json().await
        .map_err(|e| Error::Provider(format!("stackexchange parse failed: {e}")))?;

    let hits = se_resp.items.into_iter().map(|item| WebHit {
        title: html_escape::decode_html_entities(&item.title).to_string(),
        url: item.link,
        snippet: format!("Score: {}, Answered: {}", item.score, item.is_answered),
        source: "StackOverflow".to_string(),
        score: (item.score as f32 / 100.0).clamp(0.0, 1.0),
        published: DateTime::from_timestamp(item.creation_date, 0),
        provenance: "technical_qa".to_string(),
    }).collect();

    Ok(hits)
}
