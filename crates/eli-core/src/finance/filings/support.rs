use super::super::*;

pub(crate) fn sanitize_for_filename(input: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_sep = false;
        } else if !out.is_empty() && !last_sep {
            out.push('_');
            last_sep = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "file".to_string()
    } else {
        out
    }
}

fn truncate_chars(input: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if input.len() <= max {
        return input.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in input.char_indices() {
        if idx >= max {
            break;
        }
        out.push(ch);
    }
    out.push_str("\n… [truncated]");
    out
}

pub(crate) fn best_effort_sec_filing_excerpt(
    text: &str,
    form: &str,
    items: Option<&str>,
    max_chars: usize,
) -> String {
    let max_chars = max_chars.max(256);
    if text.trim().is_empty() {
        return String::new();
    }

    let mut header_candidates: Vec<usize> = Vec::new();
    let mut item_candidates: Vec<usize> = Vec::new();

    // Common SEC filing anchors (prefer starting after any iXBRL/header noise).
    for needle in [
        "SECURITIES AND EXCHANGE COMMISSION",
        "Securities and Exchange Commission",
        "UNITED STATES\n\nSECURITIES",
    ] {
        if let Some(idx) = text.find(needle) {
            header_candidates.push(idx);
        }
    }

    let form = form.trim();
    if !form.is_empty() {
        for needle in [format!("FORM {form}"), format!("Form {form}")] {
            if let Some(idx) = text.find(&needle) {
                header_candidates.push(idx);
            }
        }
    }

    if let Some(raw) = items {
        // SEC "items" can look like "1.01,2.03" or "1.01 2.03"
        for item in raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            for needle in [format!("ITEM {item}"), format!("Item {item}")] {
                if let Some(idx) = text.find(&needle) {
                    item_candidates.push(idx);
                }
            }
        }
    }

    let header_idx = header_candidates.into_iter().min();
    let item_idx = item_candidates.into_iter().min();

    let mut start = match (header_idx, item_idx) {
        (Some(h), Some(i)) => {
            // If the filing cover page is huge, prefer jumping to the first disclosed item.
            let jump_to_item_threshold = 5_000usize;
            if i > h && i.saturating_sub(h) > jump_to_item_threshold {
                i
            } else {
                h.min(i)
            }
        }
        (Some(h), None) => h,
        (None, Some(i)) => i,
        (None, None) => 0,
    };

    // Snap start to a sensible boundary (previous blank line if possible).
    if start > 0 {
        if let Some(boundary) = text[..start].rfind("\n\n") {
            start = boundary + 2;
        } else if let Some(boundary) = text[..start].rfind('\n') {
            start = boundary + 1;
        }
    }

    let excerpt = text[start..].trim_start();
    truncate_chars(excerpt, max_chars)
}

pub(crate) fn html_to_text(raw: &str) -> String {
    // Best-effort HTML -> text. This isn't a full parser, but works well enough for SEC filings.
    let s = raw.replace("\r\n", "\n");
    let mut out = String::with_capacity(s.len().min(128_000));
    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut skip_depth = 0usize;

    for ch in s.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag_raw = tag_buf.trim();
                let tag = tag_raw.to_ascii_lowercase();
                let mut name = tag.split_whitespace().next().unwrap_or_default();
                let is_end = name.starts_with('/');
                name = name.trim_start_matches('/').trim_end_matches('/');

                let should_skip = matches!(
                    name,
                    "script" | "style" | "head" | "ix:hidden" | "ix:header"
                );
                if should_skip {
                    if is_end {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !tag.ends_with('/') {
                        // Only bump for non-self-closing tags.
                        skip_depth = skip_depth.saturating_add(1);
                    }
                }

                // Newline-ish tags (only when not in a skipped section).
                if skip_depth == 0 {
                    if tag.starts_with("br")
                        || tag.starts_with("/p")
                        || tag.starts_with("p")
                        || tag.starts_with("/div")
                        || tag.starts_with("div")
                        || tag.starts_with("/tr")
                        || tag.starts_with("tr")
                        || tag.starts_with("/li")
                        || tag.starts_with("li")
                        || tag.starts_with("hr")
                    {
                        out.push('\n');
                    }
                }
                tag_buf.clear();
            } else {
                // cap tag buffer to avoid huge memory on malformed input
                if tag_buf.len() < 256 {
                    tag_buf.push(ch);
                }
            }
            continue;
        }

        if ch == '<' {
            in_tag = true;
            continue;
        }
        if skip_depth == 0 {
            out.push(ch);
        }
    }

    // Decode entities and normalize whitespace.
    let decoded = html_escape::decode_html_entities(&out).to_string();
    let mut cleaned = String::with_capacity(decoded.len());
    let mut last_ws = false;
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if ch == '\n' {
                cleaned.push('\n');
                last_ws = false;
            } else if !last_ws {
                cleaned.push(' ');
                last_ws = true;
            }
        } else {
            cleaned.push(ch);
            last_ws = false;
        }
    }

    // Collapse excessive blank lines.
    let mut final_out = String::with_capacity(cleaned.len());
    let mut blank_run = 0usize;
    for line in cleaned.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                final_out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        final_out.push_str(line);
        final_out.push('\n');
    }

    final_out.trim().to_string()
}

fn sec_user_agent(override_ua: Option<&str>) -> Result<String> {
    if let Some(s) = override_ua {
        let s = s.trim();
        if !s.is_empty() {
            return Ok(s.to_string());
        }
    }

    let ua = std::env::var("ELI_SEC_USER_AGENT").unwrap_or_default();
    let ua = ua.trim();
    if ua.is_empty() {
        return Err(Error::InvalidInput(
            "SEC EDGAR requires a User-Agent with contact email. Set `ELI_SEC_USER_AGENT=\"eli (me@example.com)\"`, pass `--user-agent \"eli (me@example.com)\"`, or run `eli config --set sec_user_agent --value \"eli (me@example.com)\"`.".to_string(),
        ));
    }
    Ok(ua.to_string())
}

pub(crate) fn sec_client(override_ua: Option<&str>) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(25))
        .user_agent(sec_user_agent(override_ua)?)
        .build()
        .map_err(|e| Error::Provider(format!("sec client init failed: {e}")))
}

async fn sec_get_json(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("sec fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "sec fetch failed: http {} ({url})",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Provider(format!("sec read failed: {e}")))
}

pub(crate) async fn sec_get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("accept", "text/html, text/plain;q=0.9, */*;q=0.1")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("sec fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "sec fetch failed: http {} ({url})",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Provider(format!("sec read failed: {e}")))
}

pub(crate) async fn sec_lookup_cik(
    ticker: &str,
    sec_dir: &Path,
    ua: Option<&str>,
) -> Result<(String, String)> {
    let client = sec_client(ua)?;
    let map_path = sec_dir.join("company_tickers.json");

    let use_cache = file_is_fresh(&map_path, SEC_COMPANY_TICKERS_TTL_SECS);
    let raw = if use_cache {
        std::fs::read_to_string(&map_path)?
    } else {
        let url = "https://www.sec.gov/files/company_tickers.json";
        let text = sec_get_json(&client, url).await?;
        std::fs::write(&map_path, &text)?;
        text
    };

    #[derive(Deserialize)]
    struct Entry {
        cik_str: u64,
        ticker: String,
        title: String,
    }

    let parsed: std::collections::HashMap<String, Entry> = serde_json::from_str(&raw)
        .map_err(|e| Error::Provider(format!("sec map parse failed: {e}")))?;

    for (_k, entry) in parsed {
        if entry.ticker.trim().eq_ignore_ascii_case(ticker) {
            let cik_padded = format!("{:010}", entry.cik_str);
            return Ok((cik_padded, entry.title));
        }
    }

    Err(Error::InvalidInput(format!(
        "unknown ticker '{ticker}' for SEC filings"
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SecSubmissions {
    pub cik: String,
    pub name: Option<String>,
    pub filings: Option<SecFilings>,
}

#[derive(Deserialize)]
pub(super) struct SecFilings {
    pub recent: Option<SecRecent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SecRecent {
    pub accession_number: Vec<String>,
    pub filing_date: Vec<String>,
    #[serde(default)]
    pub report_date: Option<Vec<String>>,
    #[serde(default)]
    pub acceptance_date_time: Option<Vec<String>>,
    pub form: Vec<String>,
    #[serde(default)]
    pub items: Option<Vec<String>>,
    #[serde(default)]
    pub size: Option<Vec<u64>>,
    #[serde(default)]
    pub primary_document: Option<Vec<String>>,
    #[serde(default)]
    pub primary_doc_description: Option<Vec<String>>,
}

pub(crate) async fn sec_fetch_submissions(
    cik_padded: &str,
    fallback_name: &str,
    sec_dir: &Path,
    ua: Option<&str>,
) -> Result<SecSubmissions> {
    let client = sec_client(ua)?;
    let submissions_dir = sec_dir.join("submissions");
    std::fs::create_dir_all(&submissions_dir)?;
    let path = submissions_dir.join(format!("CIK{cik_padded}.json"));

    let use_cache = file_is_fresh(&path, SEC_SUBMISSIONS_TTL_SECS);
    let raw = if use_cache {
        std::fs::read_to_string(&path)?
    } else {
        let url = format!("https://data.sec.gov/submissions/CIK{cik_padded}.json");
        let text = sec_get_json(&client, &url).await?;
        std::fs::write(&path, &text)?;
        text
    };

    let mut parsed: SecSubmissions = serde_json::from_str(&raw)
        .map_err(|e| Error::Provider(format!("sec submissions parse failed: {e}")))?;
    if parsed.name.as_deref().unwrap_or("").trim().is_empty() {
        parsed.name = Some(fallback_name.to_string());
    }
    Ok(parsed)
}

fn file_is_fresh(path: &Path, max_age_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return false;
    };
    elapsed.as_secs() <= max_age_secs
}
