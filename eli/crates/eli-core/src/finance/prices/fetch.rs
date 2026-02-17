use super::super::timeseries::fetch::write_debug_payload;
use super::super::*;

pub async fn fetch_prices(req: PricesRequest) -> Result<PricesResponse> {
    #[derive(Deserialize)]
    struct HermesFeed {
        id: String,
        #[serde(default)]
        attributes: std::collections::HashMap<String, String>,
    }
    #[derive(Deserialize)]
    struct HermesPrice {
        conf: String,
        expo: i32,
        price: String,
        publish_time: i64,
    }
    #[derive(Deserialize)]
    struct HermesParsedUpdate {
        id: String,
        price: HermesPrice,
    }
    #[derive(Deserialize)]
    struct HermesLatest {
        parsed: Vec<HermesParsedUpdate>,
    }

    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(10))
        .build()
        .map_err(|e| Error::Provider(format!("prices client init failed: {e}")))?;

    let asset_type = req
        .asset_type
        .as_deref()
        .unwrap_or("crypto")
        .to_ascii_lowercase();
    let mut ids: Vec<String> = req
        .ids
        .iter()
        .map(|s| s.trim().trim_start_matches("0x").to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut id_to_symbol: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let query = req
        .query
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let feed_id = |feed: &HermesFeed| feed.id.trim_start_matches("0x").to_string();
    let feed_symbol = |feed: &HermesFeed| feed.attributes.get("symbol").cloned();
    let feed_description = |feed: &HermesFeed| {
        feed.attributes
            .get("description")
            .cloned()
            .or_else(|| feed.attributes.get("name").cloned())
    };

    let make_candidate = |feed: &HermesFeed| PriceCandidate {
        id: feed_id(feed),
        symbol: feed.attributes.get("symbol").cloned(),
        description: feed
            .attributes
            .get("description")
            .cloned()
            .or_else(|| feed.attributes.get("name").cloned()),
        asset_type: feed.attributes.get("asset_type").cloned(),
    };

    let score_candidate = |query: &str, symbol: Option<&str>, description: Option<&str>| -> i32 {
        let q = query.to_ascii_lowercase();
        let mut score = 0;
        if let Some(sym) = symbol {
            let sym_l = sym.to_ascii_lowercase();
            if sym_l == q {
                score += 100;
            }
            if sym_l.starts_with(&q) {
                score += 40;
            }
            if sym_l.contains(&q) {
                score += 20;
            }
            let len_diff = (sym_l.len() as i32 - q.len() as i32).abs().min(20);
            score -= len_diff;
        }
        if let Some(desc) = description {
            let desc_l = desc.to_ascii_lowercase();
            if desc_l.contains(&q) {
                score += 10;
            }
        }
        score
    };

    if ids.is_empty() {
        let mut url = reqwest::Url::parse("https://hermes.pyth.network/v2/price_feeds")
            .map_err(|e| Error::Provider(format!("prices feeds url failed: {e}")))?;
        if let Some(query) = query.as_deref() {
            url.query_pairs_mut().append_pair("query", query);
        }
        if !asset_type.is_empty() {
            url.query_pairs_mut().append_pair("asset_type", &asset_type);
        }
        let feeds_url = url.to_string();
        let start_time = std::time::Instant::now();
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("prices feeds fetch failed: {e}")))?;
        let status_code = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Provider(format!("prices feeds read failed: {e}")))?;
        info!(
            target: "eli.finance.prices",
            url = %feeds_url,
            status = %status_code,
            bytes = body.len(),
            elapsed_ms = start_time.elapsed().as_millis(),
            "prices feeds fetch"
        );
        if !status_code.is_success() {
            return Err(Error::Provider(format!(
                "prices feeds fetch failed: http {}",
                status_code
            )));
        }
        let mut feeds: Vec<HermesFeed> = match serde_json::from_str(&body) {
            Ok(parsed) => parsed,
            Err(e) => {
                let debug_path = write_debug_payload("prices_feeds", &feeds_url, &body);
                warn!(
                    target: "eli.finance.prices",
                    url = %feeds_url,
                    error = %e,
                    debug_path = debug_path.as_deref().unwrap_or(""),
                    "prices feeds parse failed"
                );
                return Err(Error::Provider(format!("prices feeds parse failed: {e}")));
            }
        };

        if let Some(query) = query.as_deref() {
            if feeds.is_empty() {
                let mut url = reqwest::Url::parse("https://hermes.pyth.network/v2/price_feeds")
                    .map_err(|e| Error::Provider(format!("prices feeds url failed: {e}")))?;
                if !asset_type.is_empty() {
                    url.query_pairs_mut().append_pair("asset_type", &asset_type);
                }
                let feeds_url = url.to_string();
                let start_time = std::time::Instant::now();
                let resp = client
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| Error::Provider(format!("prices feeds fetch failed: {e}")))?;
                let status_code = resp.status();
                let body = resp
                    .text()
                    .await
                    .map_err(|e| Error::Provider(format!("prices feeds read failed: {e}")))?;
                info!(
                    target: "eli.finance.prices",
                    url = %feeds_url,
                    status = %status_code,
                    bytes = body.len(),
                    elapsed_ms = start_time.elapsed().as_millis(),
                    "prices feeds fetch"
                );
                if !status_code.is_success() {
                    return Err(Error::Provider(format!(
                        "prices feeds fetch failed: http {}",
                        status_code
                    )));
                }
                feeds = match serde_json::from_str(&body) {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        let debug_path = write_debug_payload("prices_feeds", &feeds_url, &body);
                        warn!(
                            target: "eli.finance.prices",
                            url = %feeds_url,
                            error = %e,
                            debug_path = debug_path.as_deref().unwrap_or(""),
                            "prices feeds parse failed"
                        );
                        return Err(Error::Provider(format!("prices feeds parse failed: {e}")));
                    }
                };
            }

            if feeds.is_empty() {
                let error = ToolErrorInfo {
                    error: "NoMatches".to_string(),
                    message: "No price feeds matched the query.".to_string(),
                    hint: Some("Provide a more specific query or explicit feed IDs.".to_string()),
                    debug: None,
                };
                return Ok(PricesResponse {
                    source: "pyth".to_string(),
                    generated_at: Utc::now(),
                    prices: vec![],
                    status: Some("error".to_string()),
                    error: Some(error),
                    disambiguation: None,
                });
            }

            let exact: Vec<&HermesFeed> = feeds
                .iter()
                .filter(|feed| {
                    feed_symbol(feed)
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case(query))
                        .unwrap_or(false)
                        || feed_id(feed).eq_ignore_ascii_case(query)
                })
                .collect();

            if exact.len() == 1 {
                let feed = exact[0];
                let id = feed_id(feed);
                let symbol = feed
                    .attributes
                    .get("symbol")
                    .cloned()
                    .unwrap_or_else(|| id.clone());
                id_to_symbol.insert(id.clone(), symbol);
                ids.push(id);
            } else {
                let mut candidates: Vec<PriceCandidate> = if exact.is_empty() {
                    let mut scored: Vec<(i32, PriceCandidate)> = feeds
                        .iter()
                        .map(|feed| {
                            (
                                score_candidate(
                                    query,
                                    feed_symbol(feed).as_deref(),
                                    feed_description(feed).as_deref(),
                                ),
                                make_candidate(feed),
                            )
                        })
                        .collect();
                    scored.sort_by(|a, b| b.0.cmp(&a.0));
                    scored.into_iter().take(5).map(|(_, c)| c).collect()
                } else {
                    exact.into_iter().map(make_candidate).collect()
                };

                if candidates.len() > 5 {
                    candidates.truncate(5);
                }

                if req.auto_select {
                    if let Some(best) = candidates.first() {
                        let id = best.id.trim_start_matches("0x").to_string();
                        let symbol = best.symbol.clone().unwrap_or_else(|| id.clone());
                        id_to_symbol.insert(id.clone(), symbol);
                        ids.push(id);
                    }
                }

                if req.auto_select && !ids.is_empty() {
                    // Continue to live fetch with the selected feed id.
                } else {
                    let disambiguation = PriceDisambiguation {
                        query: query.to_string(),
                        candidates,
                        message: Some("Ambiguous query; choose a specific feed id.".to_string()),
                    };

                    return Ok(PricesResponse {
                        source: "pyth".to_string(),
                        generated_at: Utc::now(),
                        prices: vec![],
                        status: Some("disambiguation".to_string()),
                        error: None,
                        disambiguation: Some(disambiguation),
                    });
                }
            }
        } else {
            if feeds.is_empty() {
                let error = ToolErrorInfo {
                    error: "NoMatches".to_string(),
                    message: "No price feeds available for the requested asset type.".to_string(),
                    hint: Some("Specify a query or explicit feed IDs.".to_string()),
                    debug: None,
                };
                return Ok(PricesResponse {
                    source: "pyth".to_string(),
                    generated_at: Utc::now(),
                    prices: vec![],
                    status: Some("error".to_string()),
                    error: Some(error),
                    disambiguation: None,
                });
            }

            for feed in feeds {
                let id = feed_id(&feed);
                let symbol = feed
                    .attributes
                    .get("symbol")
                    .cloned()
                    .unwrap_or_else(|| id.clone());
                id_to_symbol.insert(id.clone(), symbol);
                ids.push(id);
            }
        }
    } else {
        for id in &ids {
            id_to_symbol.insert(id.clone(), id.clone());
        }
    }

    // Batch IDs into chunks of 50 to avoid URL length limits (Pyth rejects huge URLs).
    const BATCH_SIZE: usize = 50;
    let mut all_parsed: Vec<HermesParsedUpdate> = Vec::new();

    for chunk in ids.chunks(BATCH_SIZE) {
        let mut url = reqwest::Url::parse("https://hermes.pyth.network/v2/updates/price/latest")
            .map_err(|e| Error::Provider(format!("prices latest url failed: {e}")))?;
        for id in chunk {
            url.query_pairs_mut().append_pair("ids[]", id);
        }
        url.query_pairs_mut().append_pair("parsed", "true");

        let batch_url = url.clone();
        let start_time = std::time::Instant::now();
        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "eli.finance.prices", url = %batch_url, error = %e, "prices batch fetch failed");
                continue;
            }
        };
        let status = resp.status();
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "eli.finance.prices", error = %e, "prices batch read failed");
                continue;
            }
        };
        info!(
            target: "eli.finance.prices",
            url = %batch_url,
            status = %status,
            bytes = body.len(),
            elapsed_ms = start_time.elapsed().as_millis(),
            batch_size = chunk.len(),
            "prices latest fetch"
        );

        match serde_json::from_str::<HermesLatest>(&body) {
            Ok(parsed) => all_parsed.extend(parsed.parsed),
            Err(e) => {
                let debug_path = write_debug_payload("prices_latest", &batch_url.to_string(), &body);
                warn!(
                    target: "eli.finance.prices",
                    url = %batch_url,
                    error = %e,
                    debug_path = debug_path.as_deref().unwrap_or(""),
                    "prices latest parse failed"
                );
            }
        }
    }

    if all_parsed.is_empty() {
        let error = ToolErrorInfo {
            error: "NoPrices".to_string(),
            message: "No prices returned for the requested feed IDs.".to_string(),
            hint: Some("Verify feed IDs or run a query to discover valid IDs.".to_string()),
            debug: None,
        };
        return Ok(PricesResponse {
            source: "pyth".to_string(),
            generated_at: Utc::now(),
            prices: vec![],
            status: Some("error".to_string()),
            error: Some(error),
            disambiguation: None,
        });
    }

    let mut prices = Vec::new();
    for item in all_parsed {
        let expo = item.price.expo;
        let price_raw: f64 = item.price.price.parse().unwrap_or(0.0);
        let value = price_raw * 10f64.powi(expo);
        let symbol = id_to_symbol
            .get(&item.id.trim_start_matches("0x").to_string())
            .cloned()
            .unwrap_or_else(|| item.id.clone());
        prices.push(PricePoint {
            source: "pyth".to_string(),
            symbol,
            value,
            timestamp: item.price.publish_time as u64,
            received_at: Utc::now(),
        });
    }

    prices.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    Ok(PricesResponse {
        source: "pyth".to_string(),
        generated_at: Utc::now(),
        prices,
        status: None,
        error: None,
        disambiguation: None,
    })
}
