use chrono::{NaiveDate, Utc};
use serde::Deserialize;

use crate::{Error, Result};

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    created: Option<f64>,
    context_length: Option<f64>,
    expiration_date: Option<String>,
}

fn is_free_model(id: &str) -> bool {
    id.to_ascii_lowercase().ends_with(":free")
}

fn is_expired(expiration_date: &Option<String>) -> bool {
    let Some(raw) = expiration_date.as_deref() else {
        return false;
    };
    let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") else {
        return false;
    };
    date < Utc::now().date_naive()
}

fn expiration_rank(expiration_date: &Option<String>) -> u8 {
    match expiration_date.as_deref() {
        None => 0,
        Some(raw) => {
            if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
                let today = Utc::now().date_naive();
                if date >= today {
                    return 1;
                }
            }
            2
        }
    }
}

pub async fn select_free_model(
    base_url: &str,
    api_key: Option<&str>,
    preferred: Option<&str>,
) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| Error::Provider(format!("openrouter client init failed: {e}")))?;

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(url);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| Error::Provider(format!("openrouter models fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "openrouter models fetch failed: http {}",
            resp.status()
        )));
    }

    let body: ModelsResponse = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("openrouter models parse failed: {e}")))?;

    if let Some(preferred_id) = preferred {
        if let Some(m) = body.data.iter().find(|m| m.id == preferred_id) {
            if is_free_model(&m.id) && !is_expired(&m.expiration_date) {
                return Ok(Some(m.id.clone()));
            }
        }
    }

    let mut candidates: Vec<&ModelEntry> = body
        .data
        .iter()
        .filter(|m| is_free_model(&m.id) && !is_expired(&m.expiration_date))
        .collect();

    if candidates.is_empty() {
        return Ok(None);
    }

    candidates.sort_by(|a, b| {
        expiration_rank(&a.expiration_date)
            .cmp(&expiration_rank(&b.expiration_date))
            .then_with(|| {
                b.created
                    .unwrap_or(0.0)
                    .partial_cmp(&a.created.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                b.context_length
                    .unwrap_or(0.0)
                    .partial_cmp(&a.context_length.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.id.cmp(&b.id))
    });

    Ok(candidates.first().map(|m| m.id.clone()))
}
