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

    let client = &*crate::finance::shared_client::GENERAL;

    // Yahoo search — instrument/ticker lookup.
    let mut yahoo_results = Vec::new();
    let yahoo_resp = client
        .get(YAHOO_SEARCH_URL)
        .query(&[
            ("q", query.as_str()),
            ("quotesCount", "25"),
            ("newsCount", "0"),
        ])
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await;

    if let Ok(resp) = yahoo_resp {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(quotes) = json["quotes"].as_array() {
                    for q in quotes {
                        let symbol = q["symbol"].as_str().unwrap_or_default();
                        if symbol.is_empty() { continue; }
                        yahoo_results.push(SearchItem {
                            symbol: symbol.to_string(),
                            name: q["shortname"]
                                .as_str()
                                .or(q["longname"].as_str())
                                .map(|s| s.to_string()),
                            exchange: Some(q["exchange"].as_str().unwrap_or_default().to_string()),
                            asset_type: q["quoteType"].as_str().map(|s| s.to_string()),
                            score: q["score"].as_f64(),
                        });
                    }
                }
            }
        }
    }

    // FRED search — macro series discovery.
    // Live API if key available (800K+ series), else hardcoded catalog.
    // Preserve FRED's native ordering. Don't stuff popularity into score.
    let mut fred_results = search_fred_live_or_catalog(&query).await;

    // Suppress weak FRED matches on common-noun ambiguous queries when Yahoo dominates.
    // FRED's search_rank misfires on words like "apple" → returns bananas/avocados/orange juice
    // commodity series while Yahoo correctly surfaces AAPL with a high score.
    const COMMON_NOUN_AMBIGUOUS: &[&str] = &[
        "apple", "orange", "peach", "cotton", "sugar", "butter",
        "lemon", "berry", "wheat", "corn", "rice", "salt",
    ];
    let q_lower = query.to_ascii_lowercase();
    let is_ambiguous = COMMON_NOUN_AMBIGUOUS.contains(&q_lower.as_str());
    let yahoo_top_score = yahoo_results.first().and_then(|r| r.score).unwrap_or(0.0);
    let mut suppressed_fred_count = 0usize;
    if is_ambiguous && yahoo_top_score > 25_000.0 {
        suppressed_fred_count = fred_results.len();
        fred_results.clear();
    }

    // Route by intent.
    let preferred_provider = determine_preferred_provider(&query, &yahoo_results, &fred_results);

    let latency_ms = started.elapsed().as_millis() as u64;

    let mut decision_trace = vec![format!("latency_ms={}", latency_ms)];
    if suppressed_fred_count > 0 {
        decision_trace.push(format!(
            "suppressed_fred_results={} reason=common_noun_ambiguous yahoo_top_score={:.0}",
            suppressed_fred_count, yahoo_top_score
        ));
    }

    Ok(SearchResponse {
        query,
        generated_at,
        schema_version: "finance.search.v3".to_string(),
        preferred_provider,
        yahoo_results,
        fred_results,
        decision_trace,
    })
}

/// Determine which provider is most relevant for this query.
fn determine_preferred_provider(
    query: &str,
    yahoo: &[SearchItem],
    fred: &[SearchItem],
) -> String {
    let raw = query.trim();
    let q = raw.to_ascii_lowercase();

    if raw.is_empty() {
        return "yahoo".to_string();
    }

    // Explicit macro intent → fred
    const FRED_TERMS: &[&str] = &[
        "inflation", "cpi", "pce", "ppi", "gdp", "unemployment", "employment", "payroll",
        "jobs", "labor", "mortgage rate", "yield", "treasury", "fed fund", "federal funds",
        "recession", "sentiment", "consumer confidence", "housing starts", "claims",
        "delinquency", "money supply", "m2", "balance sheet", "debt to gdp",
    ];
    if FRED_TERMS.iter().any(|term| q.contains(term)) {
        return "fred".to_string();
    }

    // Explicit instrument intent → yahoo
    const YAHOO_TERMS: &[&str] = &[
        "stock", "stocks", "share", "shares", "etf", "etfs", "option", "options",
        "future", "futures", "earnings", "dividend", "ipo",
    ];
    if YAHOO_TERMS.iter().any(|term| q.contains(term)) {
        return "yahoo".to_string();
    }

    // Ticker-like input (short, uppercase, no spaces) → yahoo
    let symbol_charset = raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '^' | '-' | '=' | ':'));
    let all_caps_like = raw.chars().all(|c| !c.is_ascii_lowercase());
    let short_alpha = raw.len() <= 5 && raw.chars().all(|c| c.is_ascii_alphabetic());
    let ticker_like = !raw.contains(' ') && symbol_charset && (all_caps_like || short_alpha);
    if ticker_like {
        return "yahoo".to_string();
    }

    // Multi-word query with FRED results → fred
    if raw.contains(' ') && !fred.is_empty() {
        return "fred".to_string();
    }

    // Default: whichever has results
    if !yahoo.is_empty() && fred.is_empty() {
        "yahoo".to_string()
    } else if yahoo.is_empty() && !fred.is_empty() {
        "fred".to_string()
    } else {
        "both".to_string()
    }
}

/// Search FRED series. Tries the live API first (if FRED_API_KEY is available),
/// falls back to the hardcoded catalog. Live search covers 800K+ series vs 60 hardcoded.
async fn search_fred_live_or_catalog(query: &str) -> Vec<SearchItem> {
    if let Ok(api_key) = crate::finance::credentials::resolve_fred_api_key() {
        if let Ok(results) = search_fred_api(query, &api_key).await {
            if !results.is_empty() {
                return results;
            }
        }
    }
    search_fred_catalog(query)
}

/// Search FRED via fred/series/search API. Preserves FRED's search ordering.
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
        return Ok(Vec::new());
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

        if id.is_empty() { continue; }

        // Build descriptive name with metadata inline
        let name = if !freq.is_empty() || !units.is_empty() {
            format!("{} [{} {} {}]", title, freq, units, sa).trim_end().to_string()
        } else {
            title
        };

        items.push(SearchItem {
            symbol: id,
            name: Some(name),
            exchange: Some("FRED".to_string()),
            asset_type: Some("MACRO".to_string()),
            score: None, // preserve FRED's native search_rank ordering, don't fake a score
        });
    }

    Ok(items)
}

/// Hardcoded FRED catalog fallback — works without API key.
fn search_fred_catalog(query: &str) -> Vec<SearchItem> {
    static FRED_CATALOG: &[(&str, &str)] = &[
        ("UMCSENT", "University of Michigan Consumer Sentiment"),
        ("MICH", "University of Michigan Inflation Expectation"),
        ("UNRATE", "Unemployment Rate"),
        ("PAYEMS", "All Employees, Total Nonfarm"),
        ("CPIAUCSL", "Consumer Price Index for All Urban Consumers"),
        ("CPILFESL", "Core CPI (Less Food and Energy)"),
        ("PCEPILFE", "Core PCE (Fed's 2% target measure)"),
        ("GDP", "Gross Domestic Product"),
        ("GDPC1", "Real Gross Domestic Product"),
        ("FEDFUNDS", "Federal Funds Effective Rate"),
        ("DFF", "Federal Funds Effective Rate (Daily)"),
        ("T10Y2Y", "10-Year Minus 2-Year Spread"),
        ("T10Y3M", "10-Year Minus 3-Month Spread"),
        ("DGS2", "2-Year Treasury Rate"),
        ("DGS10", "10-Year Treasury Rate"),
        ("DGS30", "30-Year Treasury Rate"),
        ("WALCL", "Fed Total Assets (Balance Sheet)"),
        ("WTREGEN", "Treasury General Account (TGA)"),
        ("RRPONTSYD", "Overnight Reverse Repo (RRP)"),
        ("MORTGAGE30US", "30-Year Mortgage Rate"),
        ("DCOILWTICO", "WTI Crude Oil Price"),
        ("DCOILBRENTEU", "Brent Crude Oil Price"),
        ("GOLDAMGBD228NLBM", "Gold Price (London Fix)"),
        ("VIXCLS", "VIX"),
        ("SP500", "S&P 500"),
        ("DTWEXBGS", "Trade-Weighted Dollar Index"),
        ("CSUSHPINSA", "Case-Shiller Home Price Index"),
        ("HOUST", "Housing Starts"),
        ("RSAFS", "Advance Retail Sales"),
        ("INDPRO", "Industrial Production"),
        ("M2SL", "M2 Money Supply"),
        ("ICSA", "Initial Jobless Claims"),
        ("JTSJOL", "JOLTS Job Openings"),
        ("BAMLH0A0HYM2", "High Yield Credit Spread"),
        ("BAMLC0A0CM", "Investment Grade Credit Spread"),
        ("STLFSI4", "St. Louis Financial Stress Index"),
        ("SAHMREALTIME", "Sahm Rule Recession Indicator"),
        ("USREC", "NBER Recession Indicator"),
    ];

    let q = query.to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();

    FRED_CATALOG
        .iter()
        .filter(|(id, name)| {
            let id_lower = id.to_lowercase();
            let name_lower = name.to_lowercase();
            words.iter().all(|w| id_lower.contains(w) || name_lower.contains(w))
        })
        .map(|(id, name)| SearchItem {
            symbol: id.to_string(),
            name: Some(name.to_string()),
            exchange: Some("FRED".to_string()),
            asset_type: Some("MACRO".to_string()),
            score: None,
        })
        .collect()
}
