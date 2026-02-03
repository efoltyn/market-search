use crate::{Error, Result};
use readability::extractor;
use serde::Serialize;
use std::io::Cursor;

#[derive(Serialize)]
pub struct Article {
    pub title: String,
    pub text: String,
    // pub html: String, // option to keep html if needed, but text is cheaper for tokens
}

pub async fn read_url(url: &str) -> Result<Article> {
    // 1. Fetch the raw HTML
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.114 Safari/537.36")
        .build()
        .map_err(|e| Error::Provider(format!("read client init failed: {e}")))?;

    let resp = client.get(url).send().await
        .map_err(|e| Error::Provider(format!("fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!("fetch failed: http {}", resp.status())));
    }

    let raw_html = resp.text().await
        .map_err(|e| Error::Provider(format!("read failed: {e}")))?;

    // 2. Use readability to extract content
    // readability::extractor::extract expects a generic Read + Url, but the struct is simpler to use with a Cursor
    let mut reader = Cursor::new(raw_html);
    let url_obj = reqwest::Url::parse(url)
        .map_err(|e| Error::Provider(format!("invalid url: {e}")))?;

    let product = extractor::extract(&mut reader, &url_obj)
        .map_err(|e| Error::Provider(format!("extraction failed: {e}")))?;

    Ok(Article {
        title: product.title,
        text: product.text,
    })
}
