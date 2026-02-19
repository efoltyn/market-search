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

