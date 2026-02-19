pub(crate) async fn fetch_fred_macro_counts(
    client: &reqwest::Client,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> Result<Vec<MacroScheduleDay>> {
    #[derive(Debug, Deserialize)]
    struct FredCountResp {
        events: Vec<FredCountEvent>,
    }
    #[derive(Debug, Deserialize)]
    struct FredCountEvent {
        title: String,
        start: String,
    }

    let mut url = reqwest::Url::parse("https://fred.stlouisfed.org/releases/calendar")
        .map_err(|e| Error::Provider(format!("fred calendar url build failed: {e}")))?;
    url.query_pairs_mut()
        .append_pair("rdc", "1")
        .append_pair("vs", &start_date.format("%Y-%m-%d").to_string())
        .append_pair("ve", &end_date.format("%Y-%m-%d").to_string())
        .append_pair("rid", "0");
    let url_s = url.to_string();

    let start_time = std::time::Instant::now();
    let resp = client
        .get(url)
        .header("Accept", "application/json, text/plain, */*")
        .header("User-Agent", "eli/finance-schedule")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("fred calendar counts fetch failed: {e}")))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("fred calendar counts read failed: {e}")))?;
    info!(
        target: "eli.finance.schedule",
        provider = "fred",
        endpoint = "releases/calendar rdc=1",
        url = %url_s,
        status = %status,
        bytes = body.len(),
        elapsed_ms = start_time.elapsed().as_millis(),
        "schedule fetch"
    );
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "fred calendar counts fetch failed: http {}",
            status
        )));
    }

    let parsed: FredCountResp = serde_json::from_str(&body)
        .map_err(|e| Error::Provider(format!("fred calendar counts parse failed: {e}")))?;
    let days = parsed
        .events
        .into_iter()
        .map(|e| {
            let release_count = e
                .title
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0);
            MacroScheduleDay {
                date: e.start,
                release_count,
            }
        })
        .collect();
    Ok(days)
}

