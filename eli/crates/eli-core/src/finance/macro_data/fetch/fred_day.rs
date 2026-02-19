pub(crate) async fn fetch_fred_macro_for_day(
    client: &reqwest::Client,
    date: NaiveDate,
) -> Result<Vec<MacroScheduleEvent>> {
    #[derive(Debug, Deserialize)]
    struct FredPagerResp {
        pager: String,
    }

    let date_s = date.format("%Y-%m-%d").to_string();
    let mut url = reqwest::Url::parse("https://fred.stlouisfed.org/releases/calendar")
        .map_err(|e| Error::Provider(format!("fred calendar url build failed: {e}")))?;
    url.query_pairs_mut()
        .append_pair("po", "1")
        .append_pair("ptic", "0")
        .append_pair("vs", &date_s)
        .append_pair("ve", &date_s)
        .append_pair("rid", "0");
    let url_s = url.to_string();

    let start_time = std::time::Instant::now();
    let resp = client
        .get(url)
        .header("Accept", "application/json, text/plain, */*")
        .header("User-Agent", "eli/finance-schedule")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("fred calendar day fetch failed: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("fred calendar day read failed: {e}")))?;
    info!(
        target: "eli.finance.schedule",
        provider = "fred",
        endpoint = "releases/calendar po=1",
        url = %url_s,
        status = %status,
        bytes = body.len(),
        elapsed_ms = start_time.elapsed().as_millis(),
        "schedule fetch"
    );
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "fred calendar day fetch failed: http {}",
            status
        )));
    }
    let parsed: FredPagerResp = serde_json::from_str(&body)
        .map_err(|e| Error::Provider(format!("fred calendar day parse failed: {e}")))?;

    let fragment = scraper::Html::parse_fragment(&parsed.pager);
    let row_sel = scraper::Selector::parse("tr")
        .map_err(|e| Error::Provider(format!("fred html parse failed: {e}")))?;
    let td_sel = scraper::Selector::parse("td")
        .map_err(|e| Error::Provider(format!("fred html parse failed: {e}")))?;
    let a_sel = scraper::Selector::parse("a[href]")
        .map_err(|e| Error::Provider(format!("fred html parse failed: {e}")))?;

    let mut out = Vec::new();
    for row in fragment.select(&row_sel) {
        let cells: Vec<_> = row.select(&td_sel).collect();
        if cells.len() < 2 {
            continue;
        }
        let time = collapse_ws(&cells[0].text().collect::<Vec<_>>().join(" "));
        let detail_cell = cells[1];
        let Some(anchor) = detail_cell.select(&a_sel).next() else {
            continue;
        };
        let title = collapse_ws(&anchor.text().collect::<Vec<_>>().join(" "));
        if title.is_empty() {
            continue;
        }
        let href = anchor
            .value()
            .attr("href")
            .unwrap_or_default()
            .trim()
            .to_string();
        let release_url = if href.starts_with("/release?rid=") {
            Some(format!("https://fred.stlouisfed.org{href}"))
        } else {
            None
        };
        let release_id = href
            .split("rid=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .and_then(|s| s.parse::<u32>().ok());

        out.push(MacroScheduleEvent {
            date: date_s.clone(),
            time: if time.is_empty() { None } else { Some(time) },
            title,
            release_id,
            release_url,
            source: "fred".to_string(),
        });
    }

    Ok(out)
}
