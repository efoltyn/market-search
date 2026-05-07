const SEC_MIN_REQUEST_SPACING: StdDuration = StdDuration::from_millis(200);
const SEC_MAX_RETRIES: usize = 3;

static SEC_REQUEST_GATE: std::sync::LazyLock<tokio::sync::Mutex<Option<std::time::Instant>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));

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
        .tcp_nodelay(true)
        .build()
        .map_err(|e| Error::Provider(format!("sec client init failed: {e}")))
}

async fn sec_get_json(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = sec_send(client, url, "application/json").await?;
    resp.text()
        .await
        .map_err(|e| Error::Provider(format!("sec read failed: {e}")))
}

pub(crate) async fn sec_get_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = sec_send(client, url, "*/*").await?;
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| Error::Provider(format!("sec read failed: {e}")))
}

async fn sec_send(client: &reqwest::Client, url: &str, accept: &str) -> Result<reqwest::Response> {
    let mut last_status = None;
    for attempt in 0..=SEC_MAX_RETRIES {
        sec_rate_limit().await;
        let resp = client
            .get(url)
            .header("accept", accept)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("sec fetch failed: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }

        last_status = Some(status);
        if attempt < SEC_MAX_RETRIES
            && (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
        {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(StdDuration::from_secs)
                .unwrap_or_else(|| StdDuration::from_millis(500 * (attempt as u64 + 1)));
            tokio::time::sleep(retry_after).await;
            continue;
        }

        return Err(Error::Provider(format!(
            "sec fetch failed: http {status} ({url})"
        )));
    }

    Err(Error::Provider(format!(
        "sec fetch failed: http {} ({url})",
        last_status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    )))
}

async fn sec_rate_limit() {
    let mut last_request = SEC_REQUEST_GATE.lock().await;
    if let Some(last) = *last_request {
        let elapsed = last.elapsed();
        if elapsed < SEC_MIN_REQUEST_SPACING {
            tokio::time::sleep(SEC_MIN_REQUEST_SPACING - elapsed).await;
        }
    }
    *last_request = Some(std::time::Instant::now());
}
