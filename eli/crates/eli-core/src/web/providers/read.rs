use crate::web::{
    WebContentQuality, WebReadAttempt, WebReadBatchResponse, WebReadFetchStatus,
    WebReadProbeSummary, WebReadResponse,
};
use crate::Result;
use futures::StreamExt;
use readability::extractor;
use regex::Regex;
use scraper::{Html, Selector};
use std::collections::HashSet;
use std::io::Cursor;
use tokio::time::{sleep, Duration};

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
const SUCCESS_CHARS_THRESHOLD: usize = 180;
const PARTIAL_CHARS_THRESHOLD: usize = 1;
const LOW_SIGNAL_TAGS: [&str; 10] = [
    "script", "style", "nav", "footer", "header", "aside", "noscript", "svg", "template", "form",
];

pub async fn read_url(url: &str) -> Result<WebReadResponse> {
    Ok(read_url_with_diagnostics(url).await)
}

pub async fn read_url_with_diagnostics(url: &str) -> WebReadResponse {
    let fetched_at = chrono::Utc::now();
    let mut attempts = Vec::<WebReadAttempt>::new();

    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(err) => {
            return WebReadResponse {
                url: url.to_string(),
                final_url: None,
                title: String::new(),
                text: String::new(),
                fetch_status: WebReadFetchStatus::Error,
                blocked_reason: Some("invalid_url".to_string()),
                attempts: vec![WebReadAttempt {
                    attempt: 1,
                    method: "validate_url".to_string(),
                    ok: false,
                    http_status: None,
                    error: Some(err.to_string()),
                    extractor: None,
                    text_chars: None,
                    blocked_reason: Some("invalid_url".to_string()),
                }],
                content_quality: WebContentQuality::Low,
                quality_notes: vec!["invalid_url".to_string()],
                fetched_at,
            };
        }
    };

    let client = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return WebReadResponse {
                url: url.to_string(),
                final_url: None,
                title: String::new(),
                text: String::new(),
                fetch_status: WebReadFetchStatus::Error,
                blocked_reason: Some("network_error".to_string()),
                attempts: vec![WebReadAttempt {
                    attempt: 1,
                    method: "client_init".to_string(),
                    ok: false,
                    http_status: None,
                    error: Some(err.to_string()),
                    extractor: None,
                    text_chars: None,
                    blocked_reason: Some("network_error".to_string()),
                }],
                content_quality: WebContentQuality::Low,
                quality_notes: vec!["client_init_failed".to_string()],
                fetched_at,
            };
        }
    };

    let mut raw_html: Option<String> = None;
    let mut final_url: Option<String> = None;
    let mut failure_reason: Option<String> = None;

    for try_idx in 0..2 {
        let method = if try_idx == 0 {
            "http_primary"
        } else {
            "http_retry"
        };

        let resp = client.get(parsed.clone()).send().await;
        let attempt_no = try_idx + 1;
        match resp {
            Err(err) => {
                let reason = classify_reqwest_error(&err).to_string();
                attempts.push(WebReadAttempt {
                    attempt: attempt_no,
                    method: method.to_string(),
                    ok: false,
                    http_status: None,
                    error: Some(err.to_string()),
                    extractor: None,
                    text_chars: None,
                    blocked_reason: Some(reason.clone()),
                });
                failure_reason = Some(reason.clone());
                if try_idx == 0 && (reason == "timeout" || reason == "network_error") {
                    sleep(Duration::from_millis(250)).await;
                    continue;
                }
                return failed_web_read_response(
                    url,
                    final_url,
                    attempts,
                    fetched_at,
                    reason,
                    WebReadFetchStatus::Error,
                );
            }
            Ok(resp) => {
                let status = resp.status();
                final_url = Some(resp.url().to_string());
                let status_code = status.as_u16();
                let body = match resp.text().await {
                    Ok(body) => body,
                    Err(err) => {
                        attempts.push(WebReadAttempt {
                            attempt: attempt_no,
                            method: method.to_string(),
                            ok: false,
                            http_status: Some(status_code),
                            error: Some(err.to_string()),
                            extractor: None,
                            text_chars: None,
                            blocked_reason: Some("network_error".to_string()),
                        });
                        failure_reason = Some("network_error".to_string());
                        if try_idx == 0 && status.is_server_error() {
                            sleep(Duration::from_millis(250)).await;
                            continue;
                        }
                        return failed_web_read_response(
                            url,
                            final_url,
                            attempts,
                            fetched_at,
                            "network_error".to_string(),
                            WebReadFetchStatus::Error,
                        );
                    }
                };

                if !status.is_success() {
                    let mut reason = classify_http_status(status_code).to_string();
                    if let Some(override_reason) = infer_blocked_reason_from_body(&body) {
                        reason = override_reason.to_string();
                    }
                    attempts.push(WebReadAttempt {
                        attempt: attempt_no,
                        method: method.to_string(),
                        ok: false,
                        http_status: Some(status_code),
                        error: Some(format!("http {}", status_code)),
                        extractor: None,
                        text_chars: None,
                        blocked_reason: Some(reason.clone()),
                    });
                    failure_reason = Some(reason.clone());
                    if try_idx == 0
                        && (status == reqwest::StatusCode::TOO_MANY_REQUESTS
                            || status.is_server_error())
                    {
                        sleep(Duration::from_millis(300)).await;
                        continue;
                    }
                    return failed_web_read_response(
                        url,
                        final_url,
                        attempts,
                        fetched_at,
                        reason.clone(),
                        failure_status_for_reason(&reason),
                    );
                }

                attempts.push(WebReadAttempt {
                    attempt: attempt_no,
                    method: method.to_string(),
                    ok: true,
                    http_status: Some(status_code),
                    error: None,
                    extractor: None,
                    text_chars: Some(body.chars().count()),
                    blocked_reason: None,
                });
                raw_html = Some(body);
                break;
            }
        }
    }

    let raw_html = match raw_html {
        Some(raw_html) => raw_html,
        None => {
            let reason = failure_reason.unwrap_or_else(|| "network_error".to_string());
            return failed_web_read_response(
                url,
                final_url,
                attempts,
                fetched_at,
                reason.clone(),
                failure_status_for_reason(&reason),
            );
        }
    };

    let extraction_url = final_url.as_deref().unwrap_or(url);
    let cleaned_html = strip_low_signal_html(&raw_html);
    let mut title = String::new();
    let mut text = String::new();

    match extract_with_readability(extraction_url, &cleaned_html) {
        Ok((t, extracted)) => {
            title = t;
            text = extracted;
            attempts.push(WebReadAttempt {
                attempt: attempts.len() + 1,
                method: "extract_readability".to_string(),
                ok: true,
                http_status: None,
                error: None,
                extractor: Some("readability".to_string()),
                text_chars: Some(text.chars().count()),
                blocked_reason: None,
            });
        }
        Err(err) => match extract_with_readability(extraction_url, &raw_html) {
            Ok((t, extracted)) => {
                title = t;
                text = extracted;
                attempts.push(WebReadAttempt {
                    attempt: attempts.len() + 1,
                    method: "extract_readability_raw_fallback".to_string(),
                    ok: true,
                    http_status: None,
                    error: Some(err),
                    extractor: Some("readability".to_string()),
                    text_chars: Some(text.chars().count()),
                    blocked_reason: None,
                });
            }
            Err(raw_err) => {
                attempts.push(WebReadAttempt {
                    attempt: attempts.len() + 1,
                    method: "extract_readability".to_string(),
                    ok: false,
                    http_status: None,
                    error: Some(format!("{err}; raw_fallback={raw_err}")),
                    extractor: Some("readability".to_string()),
                    text_chars: None,
                    blocked_reason: Some("empty_or_js_rendered".to_string()),
                });
            }
        },
    }

    if text.chars().count() < SUCCESS_CHARS_THRESHOLD {
        let semantic_text = extract_semantic_fallback(&cleaned_html);
        let semantic_ok = semantic_text.chars().count() >= PARTIAL_CHARS_THRESHOLD;
        attempts.push(WebReadAttempt {
            attempt: attempts.len() + 1,
            method: "extract_semantic_fallback".to_string(),
            ok: semantic_ok,
            http_status: None,
            error: (!semantic_ok).then_some("semantic extraction returned empty".to_string()),
            extractor: Some("scraper".to_string()),
            text_chars: Some(semantic_text.chars().count()),
            blocked_reason: (!semantic_ok).then_some("empty_or_js_rendered".to_string()),
        });
        if semantic_text.chars().count() > text.chars().count() {
            text = semantic_text;
        }
    }

    if text.chars().count() < SUCCESS_CHARS_THRESHOLD {
        let body_text = extract_body_fallback(&cleaned_html);
        let (body_quality, body_notes) = assess_content_quality(&body_text, &raw_html);
        let body_ok = body_text.chars().count() >= PARTIAL_CHARS_THRESHOLD
            && body_quality != WebContentQuality::Low;
        attempts.push(WebReadAttempt {
            attempt: attempts.len() + 1,
            method: "extract_body_fallback".to_string(),
            ok: body_ok,
            http_status: None,
            error: (!body_ok).then_some(format!(
                "body fallback rejected due to low quality: {}",
                body_notes.join(", ")
            )),
            extractor: Some("scraper".to_string()),
            text_chars: Some(body_text.chars().count()),
            blocked_reason: (!body_ok).then_some("empty_or_js_rendered".to_string()),
        });
        if body_ok && body_text.chars().count() > text.chars().count() {
            text = body_text;
        }
    }

    if title.trim().is_empty() {
        title = extract_title_from_html(&raw_html);
    }

    let text_chars = text.chars().count();
    let (content_quality, quality_notes) = assess_content_quality(&text, &raw_html);
    let (fetch_status, blocked_reason) =
        if text_chars >= SUCCESS_CHARS_THRESHOLD && content_quality != WebContentQuality::Low {
            (WebReadFetchStatus::Success, None)
        } else if text_chars >= PARTIAL_CHARS_THRESHOLD {
            (WebReadFetchStatus::Partial, None)
        } else {
            (
                WebReadFetchStatus::Error,
                Some("empty_or_js_rendered".to_string()),
            )
        };

    WebReadResponse {
        url: url.to_string(),
        final_url,
        title,
        text,
        fetch_status,
        blocked_reason,
        attempts,
        content_quality,
        quality_notes,
        fetched_at,
    }
}

pub async fn read_urls_with_diagnostics(
    urls: &[String],
    max_parallel: usize,
) -> WebReadBatchResponse {
    let requested = urls.len();
    let mut seen = HashSet::<String>::new();
    let mut deduped_urls = Vec::<String>::new();
    for url in urls {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            deduped_urls.push(trimmed.to_string());
        }
    }
    let deduped = deduped_urls.len();
    let parallel = max_parallel.max(1);

    let mut indexed_results = futures::stream::iter(deduped_urls.into_iter().enumerate())
        .map(|(idx, url)| async move { (idx, read_url_with_diagnostics(&url).await) })
        .buffer_unordered(parallel)
        .collect::<Vec<_>>()
        .await;
    indexed_results.sort_by_key(|(idx, _)| *idx);
    let results = indexed_results
        .into_iter()
        .map(|(_, response)| response)
        .collect::<Vec<_>>();

    let mut success_count = 0usize;
    let mut partial_count = 0usize;
    let mut blocked_count = 0usize;
    let mut error_count = 0usize;
    for response in &results {
        match response.fetch_status {
            WebReadFetchStatus::Success => success_count += 1,
            WebReadFetchStatus::Partial => partial_count += 1,
            WebReadFetchStatus::Blocked => blocked_count += 1,
            WebReadFetchStatus::Error => error_count += 1,
        }
    }

    WebReadBatchResponse {
        mode: "batch".to_string(),
        requested,
        deduped,
        completed: results.len(),
        success_count,
        partial_count,
        blocked_count,
        error_count,
        results,
    }
}

pub fn to_probe_summary(response: &WebReadResponse) -> WebReadProbeSummary {
    WebReadProbeSummary {
        fetch_status: response.fetch_status,
        blocked_reason: response.blocked_reason.clone(),
        attempts_count: response.attempts.len(),
        text_chars: response.text.chars().count(),
    }
}

fn failed_web_read_response(
    url: &str,
    final_url: Option<String>,
    attempts: Vec<WebReadAttempt>,
    fetched_at: chrono::DateTime<chrono::Utc>,
    reason: String,
    fetch_status: WebReadFetchStatus,
) -> WebReadResponse {
    WebReadResponse {
        url: url.to_string(),
        final_url,
        title: String::new(),
        text: String::new(),
        fetch_status,
        blocked_reason: Some(reason),
        attempts,
        content_quality: WebContentQuality::Low,
        quality_notes: vec!["fetch_failed".to_string()],
        fetched_at,
    }
}

fn classify_reqwest_error(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() || err.is_request() {
        "network_error"
    } else {
        "network_error"
    }
}

fn classify_http_status(status: u16) -> &'static str {
    match status {
        401 => "auth_required",
        403 => "forbidden",
        404 => "not_found",
        429 => "rate_limited",
        451 => "legal_restriction",
        500..=599 => "server_error",
        _ => "server_error",
    }
}

fn infer_blocked_reason_from_body(body: &str) -> Option<&'static str> {
    let sample = body
        .chars()
        .take(2_000)
        .collect::<String>()
        .to_ascii_lowercase();
    let markers = [
        "captcha",
        "verify you are human",
        "cloudflare",
        "bot detection",
        "access denied",
        "are you a robot",
    ];
    if markers.iter().any(|m| sample.contains(m)) {
        Some("captcha_or_bot_challenge")
    } else {
        None
    }
}

fn failure_status_for_reason(reason: &str) -> WebReadFetchStatus {
    match reason {
        "auth_required"
        | "forbidden"
        | "rate_limited"
        | "captcha_or_bot_challenge"
        | "not_found"
        | "legal_restriction" => WebReadFetchStatus::Blocked,
        _ => WebReadFetchStatus::Error,
    }
}

fn extract_with_readability(
    url: &str,
    raw_html: &str,
) -> std::result::Result<(String, String), String> {
    let url_obj = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    let mut reader = Cursor::new(raw_html.as_bytes());
    let product = extractor::extract(&mut reader, &url_obj).map_err(|e| e.to_string())?;
    Ok((
        product.title.trim().to_string(),
        product.text.trim().to_string(),
    ))
}

fn extract_semantic_fallback(raw_html: &str) -> String {
    let document = Html::parse_document(raw_html);
    let selectors = [
        "article",
        "main",
        "[role='main']",
        ".article-body",
        ".post-content",
    ];

    for css in selectors {
        let Ok(selector) = Selector::parse(css) else {
            continue;
        };
        let mut text = String::new();
        for el in document.select(&selector) {
            let chunk = el
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if chunk.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&chunk);
        }
        if !text.trim().is_empty() {
            return text.trim().to_string();
        }
    }
    String::new()
}

fn extract_body_fallback(raw_html: &str) -> String {
    let document = Html::parse_document(raw_html);
    let Ok(body_selector) = Selector::parse("body") else {
        return String::new();
    };
    let mut text = String::new();
    for el in document.select(&body_selector) {
        let chunk = el
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if chunk.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&chunk);
    }
    text.trim().to_string()
}

fn strip_low_signal_html(raw_html: &str) -> String {
    let mut cleaned = raw_html.to_string();
    for tag in LOW_SIGNAL_TAGS {
        let pattern = format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}>");
        if let Ok(re) = Regex::new(&pattern) {
            cleaned = re.replace_all(&cleaned, " ").into_owned();
        }
    }
    if let Ok(cookie_re) = Regex::new(
        r#"(?is)<(div|section|aside)[^>]*(id|class)\s*=\s*["'][^"']*(cookie|consent|newsletter|subscribe|popup|banner)[^"']*["'][^>]*>.*?</\1>"#,
    ) {
        cleaned = cookie_re.replace_all(&cleaned, " ").into_owned();
    }
    cleaned
}

fn assess_content_quality(text: &str, raw_html: &str) -> (WebContentQuality, Vec<String>) {
    let text_chars = text.chars().count();
    if text_chars == 0 {
        return (WebContentQuality::Low, vec!["empty_text".to_string()]);
    }

    let text_tokens = text.split_whitespace().collect::<Vec<_>>();
    if text_tokens.is_empty() {
        return (WebContentQuality::Low, vec!["empty_tokens".to_string()]);
    }

    let html_chars = raw_html.chars().count().max(1);
    let density = text_chars as f64 / html_chars as f64;
    let mut notes = Vec::new();

    let css_like_tokens = text_tokens
        .iter()
        .filter(|tok| {
            let t = tok.to_ascii_lowercase();
            t.contains('{')
                || t.contains('}')
                || t.contains("color:")
                || t.contains("font-")
                || t.contains("padding")
                || t.contains("margin")
                || t.contains("display:")
                || t.contains("px")
        })
        .count();
    let code_like_tokens = text_tokens
        .iter()
        .filter(|tok| {
            let t = tok.to_ascii_lowercase();
            t.contains("function")
                || t.contains("const")
                || t.contains("var")
                || t.contains("=>")
                || t.contains("import ")
                || t.contains("export ")
                || t.contains(';')
        })
        .count();

    let css_ratio = css_like_tokens as f64 / text_tokens.len() as f64;
    let code_ratio = code_like_tokens as f64 / text_tokens.len() as f64;

    let mut unique = std::collections::HashSet::new();
    for token in &text_tokens {
        unique.insert(token.to_ascii_lowercase());
    }
    let repetition_ratio = 1.0 - (unique.len() as f64 / text_tokens.len() as f64);

    if density < 0.03 {
        notes.push(format!("low_text_density={density:.4}"));
    }
    if css_ratio > 0.10 {
        notes.push(format!("high_css_ratio={css_ratio:.3}"));
    }
    if code_ratio > 0.10 {
        notes.push(format!("high_code_ratio={code_ratio:.3}"));
    }
    if repetition_ratio > 0.65 {
        notes.push(format!("high_repetition_ratio={repetition_ratio:.3}"));
    }

    let quality =
        if density >= 0.08 && css_ratio <= 0.04 && code_ratio <= 0.05 && repetition_ratio <= 0.45 {
            WebContentQuality::High
        } else if density >= 0.03 && css_ratio <= 0.10 && code_ratio <= 0.12 {
            WebContentQuality::Medium
        } else {
            WebContentQuality::Low
        };

    if notes.is_empty() {
        notes.push("quality_ok".to_string());
    }
    (quality, notes)
}

fn extract_title_from_html(raw_html: &str) -> String {
    let document = Html::parse_document(raw_html);
    let Ok(selector) = Selector::parse("title") else {
        return String::new();
    };
    document
        .select(&selector)
        .next()
        .map(|el| {
            el.text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_http_status_maps_blocked_and_error() {
        assert_eq!(classify_http_status(401), "auth_required");
        assert_eq!(classify_http_status(403), "forbidden");
        assert_eq!(classify_http_status(429), "rate_limited");
        assert_eq!(classify_http_status(503), "server_error");
    }

    #[test]
    fn infer_blocked_reason_detects_captcha_markers() {
        let body = "<html><body>Please verify you are human before continuing</body></html>";
        assert_eq!(
            infer_blocked_reason_from_body(body),
            Some("captcha_or_bot_challenge")
        );
        assert_eq!(
            infer_blocked_reason_from_body("<html>plain content</html>"),
            None
        );
    }

    #[test]
    fn semantic_fallback_extracts_main_text() {
        let html = r#"
            <html>
              <head><title>Test</title></head>
              <body>
                <main>
                  <p>First sentence with useful content.</p>
                  <p>Second sentence with numbers 123.</p>
                </main>
              </body>
            </html>
        "#;
        let text = extract_semantic_fallback(html);
        assert!(text.contains("First sentence"));
        assert!(text.contains("Second sentence"));
    }

    #[test]
    fn strip_low_signal_html_removes_script_and_style_blocks() {
        let html = r#"
            <html>
              <head>
                <style>.hero { color: red; }</style>
                <script>const x = 1;</script>
              </head>
              <body><main>Readable text here.</main></body>
            </html>
        "#;
        let stripped = strip_low_signal_html(html);
        assert!(!stripped.contains("const x = 1"));
        assert!(!stripped.contains(".hero"));
        assert!(stripped.contains("Readable text here"));
    }

    #[test]
    fn assess_content_quality_flags_css_noise() {
        let noisy_text = "color: red; margin: 0; padding: 0; display: block; font-size: 12px;";
        let html = "<html><body>test</body></html>";
        let (quality, notes) = assess_content_quality(noisy_text, html);
        assert_eq!(quality, WebContentQuality::Low);
        assert!(notes.iter().any(|n| n.contains("high_css_ratio")));
    }
}
