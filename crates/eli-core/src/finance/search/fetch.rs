use super::super::*;

pub async fn fetch_search(req: SearchRequest) -> Result<SearchResponse> {
    if matches!(req.provider, ProviderKind::Ibkr) {
        return crate::finance::fetch_ibkr_search(&req).await;
    }
    let started = std::time::Instant::now();
    let generated_at = chrono::Utc::now();
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Err(Error::InvalidInput("search query is required".to_string()));
    }
    let policy_mode = req.policy_mode.unwrap_or_default();
    let policy_file = req.policy_file.as_deref().map(std::path::Path::new);
    let resolved_policy = crate::finance::policy::load_policy(policy_file, policy_mode)?;

    let client = &*crate::finance::shared_client::GENERAL;

    let resp = client
        .get(YAHOO_SEARCH_URL)
        .query(&[
            ("q", query.as_str()),
            ("quotesCount", "25"),
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

    // FRED search: live API if key available (800K+ series), else hardcoded catalog (60 series).
    results.extend(search_fred_live_or_catalog(&query).await);

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
    if results.is_empty() && !suggestions.is_empty() {
        results = suggestions.clone();
    }

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

/// Search FRED series. Tries the live API first (if FRED_API_KEY is available),
/// falls back to the hardcoded catalog. Live search covers 800K+ series vs 60 hardcoded.
async fn search_fred_live_or_catalog(query: &str) -> Vec<SearchItem> {
    // Try live FRED API search first.
    if let Ok(api_key) = crate::finance::credentials::resolve_fred_api_key() {
        if let Ok(results) = search_fred_api(query, &api_key).await {
            if !results.is_empty() {
                return results;
            }
        }
    }
    // Fall back to hardcoded catalog (no key needed, instant).
    search_fred_catalog(query)
}

/// Search FRED via fred/series/search API. Returns up to 20 results ranked by popularity.
async fn search_fred_api(query: &str, api_key: &str) -> Result<Vec<SearchItem>> {
    let client = &*crate::finance::shared_client::GENERAL;
    let url = format!(
        "https://api.stlouisfed.org/fred/series/search?search_text={}&api_key={}&file_type=json&limit=20&order_by=search_rank&sort_order=desc",
        urlencoding::encode(query),
        api_key,
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("fred search failed: {e}")))?;

    if !resp.status().is_success() {
        return Ok(Vec::new()); // silently fall back to catalog
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("fred search parse failed: {e}")))?;

    let series = body
        .get("seriess")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut items = Vec::new();
    for s in &series {
        let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let freq = s.get("frequency_short").and_then(|v| v.as_str()).unwrap_or("");
        let units = s.get("units_short").and_then(|v| v.as_str()).unwrap_or("");
        let sa = s.get("seasonal_adjustment_short").and_then(|v| v.as_str()).unwrap_or("");
        let popularity = s.get("popularity").and_then(|v| v.as_i64()).unwrap_or(0);
        let last_updated = s.get("last_updated").and_then(|v| v.as_str()).unwrap_or("");

        if id.is_empty() { continue; }

        let name = if !freq.is_empty() || !units.is_empty() {
            format!("{} [{} {} {}]", title, freq, units, sa).trim_end().to_string()
        } else {
            title
        };

        items.push(SearchItem {
            symbol: id,
            name: Some(name),
            exchange: Some("FRED".to_string()),
            asset_type: Some(format!("MACRO (pop:{}, updated:{})", popularity, &last_updated[..10.min(last_updated.len())])),
            score: Some(popularity as f64),
        });
    }

    Ok(items)
}

/// Search common FRED macro series by name. Hardcoded catalog covers 60+ popular series.
/// No API key needed — instant, offline, always works.
fn search_fred_catalog(query: &str) -> Vec<SearchItem> {
    static FRED_CATALOG: &[(&str, &str)] = &[
        ("UMCSENT", "University of Michigan Consumer Sentiment"),
        ("UMCSENT1", "University of Michigan Consumer Sentiment 1-Year Ahead"),
        ("MICH", "University of Michigan Inflation Expectation"),
        ("UNRATE", "Unemployment Rate"),
        ("PAYEMS", "All Employees, Total Nonfarm"),
        ("CPIAUCSL", "Consumer Price Index for All Urban Consumers"),
        ("CPILFESL", "Consumer Price Index Less Food and Energy (Core CPI)"),
        ("PCEPI", "Personal Consumption Expenditures Price Index"),
        ("PCEPILFE", "Personal Consumption Expenditures Excluding Food and Energy (Core PCE)"),
        ("GDP", "Gross Domestic Product"),
        ("GDPC1", "Real Gross Domestic Product"),
        ("FEDFUNDS", "Federal Funds Effective Rate"),
        ("DFF", "Federal Funds Effective Rate (Daily)"),
        ("T10Y2Y", "10-Year Treasury Minus 2-Year Treasury Spread"),
        ("T10Y3M", "10-Year Treasury Minus 3-Month Treasury Spread"),
        ("DGS1MO", "1-Month Treasury Constant Maturity Rate"),
        ("DGS3MO", "3-Month Treasury Constant Maturity Rate"),
        ("DGS6MO", "6-Month Treasury Constant Maturity Rate"),
        ("DGS1", "1-Year Treasury Constant Maturity Rate"),
        ("DGS2", "2-Year Treasury Constant Maturity Rate"),
        ("DGS5", "5-Year Treasury Constant Maturity Rate"),
        ("DGS10", "10-Year Treasury Constant Maturity Rate"),
        ("DGS20", "20-Year Treasury Constant Maturity Rate"),
        ("DGS30", "30-Year Treasury Constant Maturity Rate"),
        ("WALCL", "Federal Reserve Total Assets (Balance Sheet)"),
        ("WTREGEN", "Treasury General Account (TGA)"),
        ("RRPONTSYD", "Overnight Reverse Repurchase Agreements (RRP)"),
        ("GFDEGDQ188S", "Federal Debt to GDP Ratio"),
        ("MORTGAGE30US", "30-Year Fixed Rate Mortgage Average"),
        ("DCOILWTICO", "Crude Oil Prices: WTI"),
        ("DCOILBRENTEU", "Crude Oil Prices: Brent"),
        ("GOLDAMGBD228NLBM", "Gold Fixing Price, London"),
        ("VIXCLS", "CBOE Volatility Index: VIX"),
        ("NASDAQCOM", "NASDAQ Composite Index"),
        ("SP500", "S&P 500 Index"),
        ("DTWEXBGS", "Trade Weighted U.S. Dollar Index"),
        ("DEXUSEU", "U.S./Euro Foreign Exchange Rate"),
        ("DEXJPUS", "Japan/U.S. Foreign Exchange Rate"),
        ("DEXUSUK", "U.S./U.K. Foreign Exchange Rate"),
        ("CSUSHPINSA", "Case-Shiller Home Price Index"),
        ("HOUST", "Housing Starts"),
        ("RSAFS", "Advance Retail Sales"),
        ("INDPRO", "Industrial Production Index"),
        ("PERMIT", "New Privately-Owned Housing Units Authorized"),
        ("M2SL", "M2 Money Stock"),
        ("ICSA", "Initial Claims (Jobless Claims)"),
        ("CCSA", "Continued Claims"),
        ("JTSJOL", "Job Openings: JOLTS"),
        ("CIVPART", "Labor Force Participation Rate"),
        ("A191RL1Q225SBEA", "Real GDP Growth Rate (Quarterly)"),
        ("CPALTT01USM657N", "CPI Annual Change"),
        ("BAMLH0A0HYM2", "ICE BofA High Yield Spread"),
        ("BAMLC0A0CM", "ICE BofA Corporate Bond Spread"),
        ("TEDRATE", "TED Spread"),
        ("WILL5000IND", "Wilshire 5000 Total Market Index"),
        ("STLFSI4", "St. Louis Fed Financial Stress Index"),
        ("USREC", "NBER Recession Indicator"),
        ("SAHMREALTIME", "Sahm Rule Recession Indicator"),
        ("PSAVERT", "Personal Saving Rate"),
        ("W875RX1", "Real Disposable Personal Income"),
    ];

    let q = query.to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();

    FRED_CATALOG
        .iter()
        .filter(|(id, name)| {
            let id_lower = id.to_lowercase();
            let name_lower = name.to_lowercase();
            // Match if all query words appear in either the ID or name
            words.iter().all(|w| id_lower.contains(w) || name_lower.contains(w))
        })
        .map(|(id, name)| SearchItem {
            symbol: id.to_string(),
            name: Some(name.to_string()),
            exchange: Some("FRED".to_string()),
            asset_type: Some("MACRO".to_string()),
            score: Some(100.0), // rank above Yahoo noise
        })
        .collect()
}
