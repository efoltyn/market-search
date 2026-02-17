use crate::web::WebHit;
use crate::{Error, Result};
use scraper::{Html, Selector};
// Re-export or use creates
use urlencoding;

pub async fn search_general(query: &str) -> Result<Vec<WebHit>> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.114 Safari/537.36")
        .build()
        .map_err(|e| Error::Provider(format!("general client init failed: {e}")))?;

    // DDG Lite URL
    let url = "https://html.duckduckgo.com/html/";

    // DDG expects form params usually, but query params work for lite
    let params = [("q", query)];

    let resp = client
        .post(url)
        .form(&params)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("duckduckgo fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "duckduckgo fetch failed: http {}",
            resp.status()
        )));
    }

    let html = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("duckduckgo read failed: {e}")))?;

    let document = Html::parse_document(&html);

    // Selectors for DDG Lite
    // .result is the container
    // .result__a is the title link
    // .result__snippet is the text
    let result_selector = Selector::parse(".result").unwrap();
    let title_selector = Selector::parse(".result__a").unwrap();
    let snippet_selector = Selector::parse(".result__snippet").unwrap();

    let mut hits = Vec::new();

    for element in document.select(&result_selector) {
        let title_el = match element.select(&title_selector).next() {
            Some(el) => el,
            None => continue,
        };

        let title = title_el.text().collect::<Vec<_>>().join("");
        let url = match title_el.value().attr("href") {
            Some(u) => u.to_string(),
            None => continue,
        };

        // DDG tracking links look like /l/?kh=-1&uddg=https%3A%2F%2F...
        // We really want the real URL. Usually parsing the 'uddg' param is needed.
        // But simply, let's try to extract it or just use the link as is if it's direct.
        // Actually, for a simple scraper, let's just decoding the url if it is wrapped.
        let clean_url = decode_ddg_url(&url);

        let snippet = match element.select(&snippet_selector).next() {
            Some(el) => el.text().collect::<Vec<_>>().join(""),
            None => String::new(),
        };

        if !title.is_empty() && !clean_url.is_empty() {
            hits.push(WebHit {
                title: title.trim().to_string(),
                url: clean_url,
                snippet: snippet.trim().to_string(),
                source: "DuckDuckGo".to_string(),
                score: 1.0, // No easy ranking from generic search
                published: None,
                provenance: "web_search".to_string(),
            });
        }
    }

    Ok(hits)
}

fn decode_ddg_url(url: &str) -> String {
    if let Some(start) = url.find("uddg=") {
        let rest = &url[start + 5..];
        if let Some(end) = rest.find('&') {
            return urlencoding::decode(&rest[..end])
                .unwrap_or_default()
                .to_string();
        }
        return urlencoding::decode(rest).unwrap_or_default().to_string();
    }
    url.to_string()
}
