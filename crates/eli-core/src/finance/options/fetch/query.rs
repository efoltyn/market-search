async fn yahoo_lookup_quote_type(client: &reqwest::Client, ticker: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(YAHOO_SEARCH_URL).ok()?;
    url.query_pairs_mut()
        .append_pair("q", ticker)
        .append_pair("quotesCount", "8")
        .append_pair("newsCount", "0");

    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let json: serde_json::Value = resp.json().await.ok()?;
    let quotes = json["quotes"].as_array()?;

    let mut fallback: Option<&serde_json::Value> = None;
    for q in quotes {
        if fallback.is_none() {
            fallback = Some(q);
        }
        if q["symbol"]
            .as_str()
            .map(|s| s.eq_ignore_ascii_case(ticker))
            .unwrap_or(false)
        {
            return q["quoteType"].as_str().map(|s| s.to_string());
        }
    }

    fallback.and_then(|q| q["quoteType"].as_str().map(|s| s.to_string()))
}

