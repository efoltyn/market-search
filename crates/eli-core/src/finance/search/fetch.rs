use super::super::*;

pub async fn fetch_search(req: SearchRequest) -> Result<SearchResponse> {
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Err(Error::InvalidInput("search query is required".to_string()));
    }

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
            let mut score = q["score"].as_f64().unwrap_or(0.0);
            let exchange = q["exchange"].as_str().unwrap_or_default();

            // Boost major US exchanges to surface primary assets (AAPL, GC=F, etc) over obscure ETFs
            if matches!(
                exchange,
                "NYQ" | "NMS" | "CMX" | "NYM" | "CBT" | "PNK" | "BATS"
            ) {
                score *= 10.0;
            }

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

    // Curated Macro Suggestions (FRED IDs)
    let macro_items = vec![
        SearchItem {
            symbol: "CPIAUCSL".into(),
            name: Some("CPI (Headline Inflation)".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
        SearchItem {
            symbol: "UNRATE".into(),
            name: Some("Unemployment Rate".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
        SearchItem {
            symbol: "PAYEMS".into(),
            name: Some("Non-farm Payrolls".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
        SearchItem {
            symbol: "FEDFUNDS".into(),
            name: Some("Fed Funds Rate".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
        SearchItem {
            symbol: "GDPC1".into(),
            name: Some("Real GDP".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
        SearchItem {
            symbol: "T10Y2Y".into(),
            name: Some("10Y-2Y Yield Spread".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
        SearchItem {
            symbol: "M2SL".into(),
            name: Some("M2 Money Supply".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
        SearchItem {
            symbol: "INDPRO".into(),
            name: Some("Industrial Production".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
        SearchItem {
            symbol: "DCOILWTICO".into(),
            name: Some("WTI Oil Price".into()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        },
    ];

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

    Ok(SearchResponse {
        query,
        results,
        macro_suggestions: suggestions,
    })
}
