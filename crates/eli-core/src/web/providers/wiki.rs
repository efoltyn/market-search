use crate::{Error, Result};
use crate::web::WebHit;
use serde::Deserialize;

pub async fn search_wiki(query: &str) -> Result<Vec<WebHit>> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&format=json",
        urlencoding::encode(query)
    );

    let resp = client.get(&url).send().await
        .map_err(|e| Error::Provider(format!("wiki fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!("wiki fetch failed: http {}", resp.status())));
    }

    #[derive(Deserialize)]
    struct WikiResponse {
        query: WikiQuery,
    }

    #[derive(Deserialize)]
    struct WikiQuery {
        search: Vec<WikiSearchItem>,
    }

    #[derive(Deserialize)]
    struct WikiSearchItem {
        title: String,
        snippet: String,
        pageid: u64,
    }

    let wiki_resp: WikiResponse = resp.json().await
        .map_err(|e| Error::Provider(format!("wiki parse failed: {e}")))?;

    let hits = wiki_resp.query.search.into_iter().map(|item| WebHit {
        title: item.title.clone(),
        url: format!("https://en.wikipedia.org/?curid={}", item.pageid),
        snippet: html_escape::decode_html_entities(&item.snippet).to_string().replace("<span class=\"searchmatch\">", "").replace("</span>", ""),
        source: "Wikipedia".to_string(),
        score: 0.9, // Wikipedia is highly authoritative
        published: None,
        provenance: "encyclopedic".to_string(),
    }).collect();

    Ok(hits)
}
