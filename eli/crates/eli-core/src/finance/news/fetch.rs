use super::super::*;

fn is_finance_like(title: &str, link: &str) -> bool {
    let t = title.to_ascii_lowercase();
    let l = link.to_ascii_lowercase();
    let finance_keywords = [
        "stock",
        "shares",
        "etf",
        "market",
        "s&p",
        "nasdaq",
        "dow",
        "invest",
        "earnings",
        "fed",
        "bond",
        "yield",
        "economy",
        "inflation",
    ];
    finance_keywords
        .iter()
        .any(|k| t.contains(k) || l.contains(k))
}

pub async fn fetch_news(req: NewsRequest) -> Result<NewsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    let date = req.date.trim();

    // Calculate window around the date
    let target_date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| Error::InvalidInput(format!("invalid date '{date}' (use YYYY-MM-DD)")))?;

    // Tighten window: after (date - 1), before (date + 1)
    let after = target_date.pred_opt().unwrap_or(target_date);
    let before = target_date.succ_opt().unwrap_or(target_date);

    let after_str = after.format("%Y-%m-%d").to_string();
    let before_str = before.format("%Y-%m-%d").to_string();

    // Add "stock" to disambiguate short tickers that are common English words.
    // e.g. "SPY" alone returns espionage news, "TLT" returns tlt.ng crypto spam.
    // Also exclude known spam domains that match ticker names.
    let base_query = if ticker.starts_with('$') {
        format!("{ticker} stock")
    } else {
        format!("${ticker} stock")
    };
    let query = format!("{base_query} -site:tlt.ng");

    let url = format!(
        "https://news.google.com/rss/search?q={}+after:{}+before:{}&hl=en-US&gl=US&ceid=US:en",
        query, after_str, before_str
    );

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| Error::Provider(format!("news client init failed: {e}")))?;
    let start_time = std::time::Instant::now();
    let resp = client
        .get(url.clone())
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("news fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "news fetch failed: http {}",
            resp.status()
        )));
    }

    let status = resp.status();
    let xml = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("news read failed: {e}")))?;
    let elapsed_ms = start_time.elapsed().as_millis();
    info!(
        target: "eli.finance.news",
        url = %url,
        status = %status,
        bytes = xml.len(),
        elapsed_ms = elapsed_ms,
        "news fetch"
    );

    // Simple manual XML parsing (extracting <item> tags)
    let mut news = Vec::new();
    let mut cursor = 0;
    while let Some(start) = xml[cursor..].find("<item>") {
        let abs_start = cursor + start;
        let end = match xml[abs_start..].find("</item>") {
            Some(e) => abs_start + e + 7,
            None => break,
        };
        let item_xml = &xml[abs_start..end];

        let title = extract_xml_tag(item_xml, "title").unwrap_or_default();
        let link = extract_xml_tag(item_xml, "link").unwrap_or_default();
        let pub_date = extract_xml_tag(item_xml, "pubDate").unwrap_or_default();

        news.push(NewsItem {
            title: html_escape::decode_html_entities(&title).to_string(),
            link,
            date: pub_date,
        });

        cursor = end;
        if news.len() >= 25 {
            break;
        }
    }

    // For short/ambiguous tickers, filter obvious non-finance noise while preserving coverage.
    if ticker.len() <= 4 {
        let filtered: Vec<NewsItem> = news
            .iter()
            .filter(|n| is_finance_like(&n.title, &n.link))
            .cloned()
            .collect();
        if filtered.len() >= 5 {
            news = filtered;
        }
    }

    Ok(NewsResponse {
        ticker,
        date: date.to_string(),
        news,
    })
}
