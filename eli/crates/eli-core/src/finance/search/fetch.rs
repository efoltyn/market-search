use super::super::*;

pub async fn fetch_search(req: SearchRequest) -> Result<SearchResponse> {
    let started = std::time::Instant::now();
    let generated_at = chrono::Utc::now();
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Err(Error::InvalidInput("search query is required".to_string()));
    }
    let policy_mode = req.policy_mode.unwrap_or_default();
    let policy_file = req
        .policy_file
        .as_deref()
        .map(std::path::Path::new);
    let resolved_policy = crate::finance::policy::load_policy(policy_file, policy_mode)?;

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| Error::Provider(format!("search client init failed: {e}")))?;

    let resp = client
        .get(YAHOO_SEARCH_URL)
        .query(&[
            ("q", query.as_str()),
            ("quotesCount", "10"),
            ("newsCount", "0"),
        ])
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("yahoo search fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "yahoo search fetch failed: http {}",
            resp.status()
        )));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("yahoo search parse failed: {e}")))?;

    let mut results = Vec::new();
    if let Some(quotes) = json["quotes"].as_array() {
        for q in quotes {
            let symbol = q["symbol"].as_str().unwrap_or_default();
            if symbol.is_empty() {
                continue;
            }
            let score = q["score"].as_f64().unwrap_or(0.0);
            let exchange = q["exchange"].as_str().unwrap_or_default();

            results.push(SearchItem {
                symbol: symbol.to_string(),
                name: q["shortname"]
                    .as_str()
                    .or(q["longname"].as_str())
                    .map(|s| s.to_string()),
                exchange: Some(exchange.to_string()),
                asset_type: q["quoteType"].as_str().map(|s| s.to_string()),
                score: Some(score),
            });
        }
    }

    // Sort by boosted score
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Policy-driven macro suggestions from catalog.
    let macro_items: Vec<SearchItem> = resolved_policy
        .policy
        .macro_catalog
        .indicators
        .iter()
        .map(|ind| SearchItem {
            symbol: ind.id.clone(),
            name: Some(ind.name.clone()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        })
        .collect();

    let query_lower = query.to_lowercase();
    let suggestions = if query_lower.len() > 2 {
        macro_items
            .into_iter()
            .filter(|item| {
                item.symbol.to_lowercase().contains(&query_lower)
                    || item
                        .name
                        .as_ref()
                        .map(|n| n.to_lowercase().contains(&query_lower))
                        .unwrap_or(false)
            })
            .collect()
    } else {
        Vec::new()
    };

    let data_as_of = Some(generated_at);
    Ok(SearchResponse {
        query,
        generated_at,
        schema_version: "finance.search.v2".to_string(),
        freshness_summary: FreshnessSummary {
            data_as_of,
            max_age_seconds: Some(0),
            stale_count: 0,
        },
        applied_policy: AppliedPolicy {
            mode: resolved_policy.mode,
            sources: resolved_policy.sources,
        },
        decision_trace: vec![
            "policy_driven_macro_suggestions=true".to_string(),
            format!("results={}", results.len()),
            format!("macro_suggestions={}", suggestions.len()),
        ],
        run_meta: RunMeta {
            latency_ms: started.elapsed().as_millis() as u64,
            stdout_chars: 0,
            stored_bytes: 0,
            coverage_counts: std::collections::BTreeMap::from([
                ("results".to_string(), results.len()),
                ("macro_suggestions".to_string(), suggestions.len()),
            ]),
            token_efficiency: None,
        },
        results,
        macro_suggestions: suggestions,
    })
}
