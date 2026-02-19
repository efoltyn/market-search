async fn cmd_finance_odds(args: FinanceOddsArgs) -> Result<()> {
    if let Some(action) = args.action {
        match action {
            FinanceOddsAction::Sync(sync_args) => return cmd_finance_sync(sync_args).await,
            FinanceOddsAction::Where(where_args) => return cmd_finance_odds_where(where_args),
        }
    }

    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    // When --search is provided alone (no --list-events/--list-markets/ticker),
    // search the local CSV cache from `eli finance sync` instead of hitting the API.
    let has_search = args.search.is_some();
    let has_list_or_ticker = args.list_events
        || args.list_markets
        || args.list_series
        || args.list_tags
        || args.series.is_some()
        || args.event.is_some()
        || args.market.is_some();

    if has_search && !has_list_or_ticker {
        // Check if local CSV cache exists
        let cache_dir = directories::ProjectDirs::from("", "", "eli")
            .map(|d| d.cache_dir().join("odds"))
            .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));
        let csv_path = cache_dir.join("all_markets.csv");

        if csv_path.exists() && !args.live {
            // CSV exists — search locally (instant, no API calls)
            return cmd_finance_odds_search_csv(
                args.search.as_deref().unwrap_or(""),
                args.limit,
                args.country.as_deref(),
                args.min_volume,
                args.top,
                args.explain,
            );
        }

        if csv_path.exists() && args.live {
            // CSV exists + --live: search CSV for tickers, then fetch fresh prices
            return cmd_finance_odds_search_live(
                args.search.as_deref().unwrap_or(""),
                &csv_path,
                args.limit,
                args.country.as_deref(),
                args.min_volume,
                args.top,
                args.explain,
            )
            .await;
        }

        // No CSV — fall back to live API search (Kalshi events → markets)
        eprintln!(
            "no local CSV cache; falling back to live API search for {:?}",
            args.search.as_deref().unwrap_or("")
        );
        return cmd_finance_odds_search_live_no_csv(
            args.search.as_deref().unwrap_or(""),
            args.limit,
            args.top,
        )
        .await;
    }

    let provider = args
        .provider
        .as_ref()
        .map(|s| s.trim().to_ascii_lowercase());
    let provider = match provider {
        None => None,
        Some(p) if p.is_empty() => None,
        Some(p) => match p.as_str() {
            "kalshi" | "polymarket" | "auto" => Some(p),
            other => anyhow::bail!(
                "unsupported --provider '{other}' (supported: kalshi, polymarket, auto)"
            ),
        },
    };

    let req = eli_core::finance::OddsRequest {
        provider,
        disable_kalshi: false,
        series_ticker: args.series,
        event_ticker: args.event,
        market_ticker: args.market,
        status: args.status,
        limit: args.limit,
        cursor: args.cursor,
        max_pages: args.max_pages,
        include_orderbook: args.orderbook,
        orderbook_depth: args.depth,
        list_series: args.list_series,
        list_events: args.list_events,
        list_markets: args.list_markets,
        list_tags: args.list_tags,
        category: args.category,
        search: args.search,
    };

    let resp = eli_core::finance::fetch_odds(req.clone())
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch odds")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.odds",
            &[format!(
                "provider={}",
                args.provider.clone().unwrap_or_default()
            )],
        )?;
        let prediction_markets_path = prediction_markets_path_for_output(&wr.out_path);
        update_prediction_markets(&prediction_markets_path, &req, &resp, Some(&wr.out_path))
            .context("update prediction markets")?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"prediction_markets_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&prediction_markets_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string())
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

/// Search the local prediction market CSV cache (from `eli finance sync`).
/// Returns matching markets as JSON, sorted by volume descending.
fn cmd_finance_odds_search_csv(
    query: &str,
    limit: Option<usize>,
    country: Option<&str>,
    min_volume_usd: Option<f64>,
    top: Option<usize>,
    explain: bool,
) -> Result<()> {
    #[derive(Deserialize)]
    struct OddsCsvRow {
        source: String,
        ticker: String,
        title: String,
        event_ticker: String,
        yes_price: String,
        volume: String,
        status: String,
        probability: String,
        category: String,
        topic: String,
    }

    fn contains_keyword(haystack: &str, keyword: &str) -> bool {
        if keyword.contains(' ') || keyword.contains('.') {
            return haystack.contains(keyword);
        }
        haystack
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|tok| tok == keyword)
    }

    fn find_us_hints(row: &OddsCsvRow) -> Vec<String> {
        let text = format!(
            "{} {} {} {} {}",
            row.title, row.event_ticker, row.category, row.topic, row.ticker
        )
        .to_ascii_lowercase();
        let keywords = [
            "us",
            "u.s.",
            "united states",
            "american",
            "nfp",
            "nonfarm payrolls",
            "fomc",
            "federal reserve",
            "cpi",
            "pce",
            "gdpnow",
        ];
        keywords
            .iter()
            .filter(|k| contains_keyword(&text, k))
            .map(|k| k.to_string())
            .collect()
    }

    fn compile_term_patterns(terms: &[String]) -> Vec<(String, regex::Regex)> {
        terms
            .iter()
            .filter_map(|t| {
                regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t)))
                    .ok()
                    .map(|re| (t.clone(), re))
            })
            .collect()
    }

    fn compute_match_terms(text: &str, term_patterns: &[(String, regex::Regex)]) -> Vec<String> {
        term_patterns
            .iter()
            .filter_map(|(term, re)| re.is_match(text).then_some(term.clone()))
            .collect()
    }

    fn has_phrase_match(text: &str, phrase_pattern: &Option<regex::Regex>) -> bool {
        phrase_pattern.as_ref().is_some_and(|re| re.is_match(text))
    }

    fn compute_match_score(row: &OddsCsvRow, query: &str, matched_terms: &[String], volume_usd: f64) -> i64 {
        let q = query.to_ascii_lowercase();
        let title = row.title.to_ascii_lowercase();
        let ticker = row.ticker.to_ascii_lowercase();
        let event = row.event_ticker.to_ascii_lowercase();
        let category = row.category.to_ascii_lowercase();
        let topic = row.topic.to_ascii_lowercase();

        let mut score = 0.0f64;
        if !q.is_empty() && title.contains(&q) {
            score += 30.0;
        }
        for t in matched_terms {
            if title.contains(t) {
                score += 10.0;
            }
            if ticker.contains(t) || event.contains(t) {
                score += 6.0;
            }
            if category.contains(t) || topic.contains(t) {
                score += 4.0;
            }
        }
        score += (matched_terms.len() as f64) * 8.0;
        score += (volume_usd.max(0.0) + 1.0).log10() * 3.0;
        score.round() as i64
    }

    fn explain_reasons(
        row: &OddsCsvRow,
        query: &str,
        matched_terms: &[String],
        volume_usd: f64,
    ) -> Vec<String> {
        let mut reasons = Vec::new();
        let query_l = query.to_ascii_lowercase();
        let title_l = row.title.to_ascii_lowercase();
        if !query_l.is_empty() && title_l.contains(&query_l) {
            reasons.push("title contains full query".to_string());
        }
        if !matched_terms.is_empty() {
            reasons.push(format!("matched terms: {}", matched_terms.join(", ")));
        }
        reasons.push(format!("volume_usd={volume_usd:.2}"));
        reasons.truncate(3);
        reasons
    }

    let cache_dir = directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));

    let csv_path = cache_dir.join("all_markets.csv");
    if !csv_path.exists() {
        anyhow::bail!(
            "no local prediction market cache found at {}. Run `eli finance sync` first.",
            csv_path.display()
        );
    }

    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let query_lower = query.to_ascii_lowercase();
    let terms: Vec<String> = query_lower
        .split_whitespace()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if terms.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let term_patterns = compile_term_patterns(&terms);
    let phrase_pattern = {
        let q = query.trim();
        if q.is_empty() {
            None
        } else {
            regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(q))).ok()
        }
    };

    let country_normalized = country.map(|c| c.trim().to_ascii_uppercase());
    if let Some(ref c) = country_normalized {
        if c != "US" {
            anyhow::bail!("unsupported --country '{c}' (v1 supports: US)");
        }
    }

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(&csv_path)
        .with_context(|| format!("open {}", csv_path.display()))?;

    let mut matches: Vec<serde_json::Value> = Vec::new();
    let mut loose_matches: Vec<serde_json::Value> = Vec::new();
    let mut semantic_query_expanded = false;
    let federal_reserve_policy_query = query_lower.contains("federal reserve");
    let fed_context_terms = [
        "fed",
        "federal reserve",
        "fomc",
        "fed funds",
        "interest rate",
    ];
    let policy_action_terms = [
        "rate", "cut", "cuts", "hike", "hikes", "hold", "decrease", "increase", "bps",
        "basis point", "decision", "meeting",
    ];
    for row in rdr.deserialize::<OddsCsvRow>() {
        let row = match row {
            Ok(r) => r,
            Err(_) => continue,
        };
        let searchable = format!(
            "{} {} {} {} {} {}",
            row.source, row.ticker, row.title, row.event_ticker, row.category, row.topic
        );
        let searchable_lower = searchable.to_ascii_lowercase();
        let mut matched_terms = compute_match_terms(&searchable, &term_patterns);
        let mut expanded_match = false;
        let has_fed_context = fed_context_terms.iter().any(|t| searchable_lower.contains(t));
        let has_policy_action = policy_action_terms.iter().any(|t| searchable_lower.contains(t));
        let policy_like = has_fed_context && has_policy_action;
        if matched_terms.is_empty() {
            if federal_reserve_policy_query && policy_like {
                matched_terms.push("fed_policy_expanded".to_string());
                semantic_query_expanded = true;
                expanded_match = true;
            } else {
                continue;
            }
        }
        if terms.len() >= 2 {
            let matched_unique: std::collections::BTreeSet<String> =
                matched_terms.iter().cloned().collect();
            if matched_unique.len() < 2
                && !has_phrase_match(&searchable, &phrase_pattern)
                && !expanded_match
            {
                continue;
            }
        }

        let country_hints = find_us_hints(&row);
        if country_normalized.as_deref() == Some("US") && country_hints.is_empty() {
            continue;
        }

        let vol_cents: f64 = row.volume.trim().parse().unwrap_or(0.0);
        let volume_usd = vol_cents / 100.0;
        if let Some(min_usd) = min_volume_usd {
            if volume_usd < min_usd {
                continue;
            }
        }

        let yes_price: f64 = row.yes_price.trim().parse().unwrap_or(0.0);
        let prob: f64 = row.probability.trim().parse().unwrap_or(0.0);
        let phrase_boost = if has_phrase_match(&searchable, &phrase_pattern) {
            7
        } else {
            0
        };
        let match_score =
            compute_match_score(&row, query, &matched_terms, volume_usd) + phrase_boost;
        let mut row_json = serde_json::json!({
            "source": row.source,
            "ticker": row.ticker,
            "title": row.title,
            "event_ticker": row.event_ticker,
            "yes_price": yes_price,
            "volume": vol_cents,
            "volume_usd": volume_usd,
            "status": row.status,
            "probability": prob,
            "category": row.category,
            "topic": row.topic,
            "match_score": match_score,
            "match_terms": matched_terms,
            "country_hints": country_hints,
        });
        if explain {
            let matched_terms_vec: Vec<String> = row_json["match_terms"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            row_json["explain"] = serde_json::json!({
                "reasons": explain_reasons(&row, query, &matched_terms_vec, volume_usd)
            });
        }
        if federal_reserve_policy_query && !policy_like {
            loose_matches.push(row_json);
            continue;
        }
        matches.push(row_json);
    }

    let mut semantic_filter_relaxed = false;
    if federal_reserve_policy_query && matches.is_empty() && !loose_matches.is_empty() {
        semantic_filter_relaxed = true;
        matches = loose_matches;
    }

    // Keep breadth by default; rank by relevance first, then liquidity.
    matches.sort_by(|a, b| {
        let sa = a["match_score"].as_i64().unwrap_or(0);
        let sb = b["match_score"].as_i64().unwrap_or(0);
        sb.cmp(&sa).then_with(|| {
            let va = a["volume_usd"].as_f64().unwrap_or(0.0);
            let vb = b["volume_usd"].as_f64().unwrap_or(0.0);
            vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    let total_matches = matches.len();
    let final_limit = top.or(limit).unwrap_or(25);
    matches.truncate(final_limit);

    let resp = serde_json::json!({
        "query": query,
        "source": "local_csv_cache",
        "csv_path": csv_path.display().to_string(),
        "total_matches": total_matches,
        "returned_matches": matches.len(),
        "limit": final_limit,
        "country": country_normalized,
        "min_volume_usd": min_volume_usd,
        "top": top,
        "semantic_filter_relaxed": semantic_filter_relaxed,
        "semantic_query_expanded": semantic_query_expanded,
        "markets": matches,
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize search results")?;
    println!("{json}");
    Ok(())
}

/// No CSV available — fall back to live API: search Kalshi events, then fetch markets
/// for matched events. Also queries Polymarket. Returns combined results.
async fn cmd_finance_odds_search_live_no_csv(
    query: &str,
    limit: Option<usize>,
    top: Option<usize>,
) -> Result<()> {
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }

    // Step 1: Search Kalshi events
    let kalshi_events_req = eli_core::finance::OddsRequest {
        provider: Some("kalshi".to_string()),
        disable_kalshi: false,
        series_ticker: None,
        event_ticker: None,
        market_ticker: None,
        status: Some("open".to_string()),
        limit: Some(200),
        cursor: None,
        max_pages: Some(3),
        include_orderbook: false,
        orderbook_depth: None,
        list_series: false,
        list_events: true,
        list_markets: false,
        list_tags: false,
        category: None,
        search: Some(query.to_string()),
    };

    // Step 2: Search Polymarket events
    let poly_events_req = eli_core::finance::OddsRequest {
        provider: Some("polymarket".to_string()),
        ..kalshi_events_req.clone()
    };

    let (kalshi_resp, poly_resp) = tokio::join!(
        eli_core::finance::fetch_odds(kalshi_events_req),
        eli_core::finance::fetch_odds(poly_events_req),
    );

    let mut all_events: Vec<serde_json::Value> = Vec::new();

    // Collect Kalshi events
    if let Ok(resp) = &kalshi_resp {
        if let Some(events) = &resp.available_events {
            for e in events {
                all_events.push(serde_json::json!({
                    "source": "kalshi",
                    "event_ticker": e.ticker,
                    "title": e.title,
                    "category": e.category,
                    "series_ticker": e.series_ticker,
                }));
            }
        }
    }

    // Collect Polymarket events
    if let Ok(resp) = &poly_resp {
        if let Some(events) = &resp.available_events {
            for e in events {
                all_events.push(serde_json::json!({
                    "source": "polymarket",
                    "event_ticker": e.ticker,
                    "title": e.title,
                    "category": e.category,
                    "slug": e.slug,
                }));
            }
        }
    }

    // Step 3: For the top events, fetch their markets with live prices
    let final_limit = top.or(limit).unwrap_or(10);
    all_events.truncate(final_limit.min(15)); // cap at 15 events to avoid rate limits

    let mut live_markets: Vec<serde_json::Value> = Vec::new();

    for event_json in &all_events {
        let source = event_json["source"].as_str().unwrap_or("");
        let event_ticker = event_json["event_ticker"].as_str().unwrap_or("");
        if event_ticker.is_empty() {
            continue;
        }

        let market_req = eli_core::finance::OddsRequest {
            provider: Some(source.to_string()),
            disable_kalshi: source == "polymarket",
            series_ticker: None,
            event_ticker: Some(event_ticker.to_string()),
            market_ticker: None,
            status: None,
            limit: None,
            cursor: None,
            max_pages: None,
            include_orderbook: false,
            orderbook_depth: None,
            list_series: false,
            list_events: false,
            list_markets: false,
            list_tags: false,
            category: None,
            search: None,
        };

        if let Ok(resp) = eli_core::finance::fetch_odds(market_req).await {
            for m in &resp.markets {
                live_markets.push(serde_json::json!({
                    "source": source,
                    "ticker": m.ticker,
                    "title": m.title,
                    "event_ticker": m.event_ticker,
                    "yes_price": m.yes_price,
                    "yes_bid": m.yes_bid,
                    "yes_ask": m.yes_ask,
                    "volume": m.volume,
                    "volume_usd": m.volume.map(|v| v as f64 / 100.0),
                    "status": m.status,
                    "probability": m.probability_yes,
                }));
            }
        }
    }

    let resp = serde_json::json!({
        "query": query,
        "source": "live_api",
        "note": "no local CSV cache; results fetched from live Kalshi + Polymarket APIs",
        "events_found": all_events.len(),
        "events": all_events,
        "markets": live_markets,
        "total_markets": live_markets.len(),
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize live search results")?;
    println!("{json}");
    Ok(())
}

/// CSV exists + --live: search CSV for event tickers, then fetch fresh prices from API.
async fn cmd_finance_odds_search_live(
    query: &str,
    csv_path: &std::path::Path,
    limit: Option<usize>,
    country: Option<&str>,
    min_volume_usd: Option<f64>,
    top: Option<usize>,
    explain: bool,
) -> Result<()> {
    // First, run the normal CSV search to find matching events
    // We read the CSV and extract unique event_tickers + source
    let query_trimmed = query.trim();
    if query_trimmed.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let query_lower = query_trimmed.to_ascii_lowercase();
    let terms: Vec<String> = query_lower
        .split_whitespace()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if terms.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }

    // Build regex patterns for matching
    let term_patterns: Vec<(String, regex::Regex)> = terms
        .iter()
        .filter_map(|t| {
            regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t)))
                .ok()
                .map(|re| (t.clone(), re))
        })
        .collect();

    #[derive(serde::Deserialize)]
    struct OddsCsvRow {
        source: String,
        ticker: String,
        title: String,
        event_ticker: String,
        yes_price: String,
        volume: String,
        status: String,
        probability: String,
        category: String,
        topic: String,
    }

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(csv_path)
        .with_context(|| format!("open {}", csv_path.display()))?;

    // Find unique event_tickers that match the query
    let mut event_map: std::collections::BTreeMap<String, (String, String, f64)> =
        std::collections::BTreeMap::new(); // event_ticker -> (source, category, total_volume)

    for row in rdr.deserialize::<OddsCsvRow>() {
        let row = match row {
            Ok(r) => r,
            Err(_) => continue,
        };
        let searchable = format!(
            "{} {} {} {} {} {}",
            row.source, row.ticker, row.title, row.event_ticker, row.category, row.topic
        );
        let matched: Vec<String> = term_patterns
            .iter()
            .filter_map(|(term, re)| re.is_match(&searchable).then_some(term.clone()))
            .collect();
        if matched.is_empty() {
            continue;
        }
        if terms.len() >= 2 {
            let unique: std::collections::BTreeSet<&String> = matched.iter().collect();
            if unique.len() < 2 {
                continue;
            }
        }

        // Apply country filter
        if let Some("US") = country {
            let text = format!(
                "{} {} {} {}",
                row.title, row.event_ticker, row.category, row.ticker
            )
            .to_ascii_lowercase();
            let us_terms = [
                "us", "u.s.", "united states", "american", "fomc", "federal reserve", "cpi", "pce",
            ];
            if !us_terms.iter().any(|t| text.contains(t)) {
                continue;
            }
        }

        let vol_cents: f64 = row.volume.trim().parse().unwrap_or(0.0);
        let volume_usd = vol_cents / 100.0;
        if let Some(min_usd) = min_volume_usd {
            if volume_usd < min_usd {
                continue;
            }
        }

        let entry = event_map
            .entry(row.event_ticker.clone())
            .or_insert_with(|| (row.source.clone(), row.category.clone(), 0.0));
        entry.2 += volume_usd;
    }

    if event_map.is_empty() {
        let resp = serde_json::json!({
            "query": query_trimmed,
            "source": "live_api_via_csv",
            "note": "no matching events found in CSV cache",
            "events_found": 0,
            "events": [],
            "markets": [],
            "total_markets": 0,
        });
        let json = serde_json::to_string_pretty(&resp).context("serialize")?;
        println!("{json}");
        return Ok(());
    }

    // Sort events by total volume, take top N
    let mut events_sorted: Vec<(String, String, String, f64)> = event_map
        .into_iter()
        .map(|(ticker, (source, cat, vol))| (ticker, source, cat, vol))
        .collect();
    events_sorted.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    let final_limit = top.or(limit).unwrap_or(10).min(15);
    events_sorted.truncate(final_limit);

    // Fetch fresh prices for each event
    let mut all_events: Vec<serde_json::Value> = Vec::new();
    let mut all_markets: Vec<serde_json::Value> = Vec::new();

    for (event_ticker, source, category, csv_volume) in &events_sorted {
        let market_req = eli_core::finance::OddsRequest {
            provider: Some(source.clone()),
            disable_kalshi: source == "polymarket",
            series_ticker: None,
            event_ticker: Some(event_ticker.clone()),
            market_ticker: None,
            status: None,
            limit: None,
            cursor: None,
            max_pages: None,
            include_orderbook: false,
            orderbook_depth: None,
            list_series: false,
            list_events: false,
            list_markets: false,
            list_tags: false,
            category: None,
            search: None,
        };

        match eli_core::finance::fetch_odds(market_req).await {
            Ok(resp) => {
                let event_title = resp
                    .events
                    .first()
                    .map(|e| e.title.clone())
                    .unwrap_or_default();
                let mut event_markets: Vec<serde_json::Value> = Vec::new();
                for m in &resp.markets {
                    let mkt_json = serde_json::json!({
                        "source": source,
                        "ticker": m.ticker,
                        "title": m.title,
                        "event_ticker": m.event_ticker,
                        "yes_price": m.yes_price,
                        "yes_bid": m.yes_bid,
                        "yes_ask": m.yes_ask,
                        "volume": m.volume,
                        "volume_usd": m.volume.map(|v| v as f64 / 100.0),
                        "status": m.status,
                        "probability": m.probability_yes,
                    });
                    all_markets.push(mkt_json.clone());
                    event_markets.push(mkt_json);
                }
                all_events.push(serde_json::json!({
                    "event_ticker": event_ticker,
                    "title": event_title,
                    "source": source,
                    "category": category,
                    "csv_volume_usd": csv_volume,
                    "markets": event_markets,
                }));
            }
            Err(e) => {
                all_events.push(serde_json::json!({
                    "event_ticker": event_ticker,
                    "source": source,
                    "category": category,
                    "csv_volume_usd": csv_volume,
                    "error": format!("{e}"),
                    "markets": [],
                }));
            }
        }
    }

    let resp = serde_json::json!({
        "query": query_trimmed,
        "source": "live_api_via_csv",
        "note": "CSV used for discovery, live API used for fresh prices",
        "events_found": all_events.len(),
        "events": all_events,
        "total_markets": all_markets.len(),
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize live search results")?;
    println!("{json}");
    Ok(())
}

fn cmd_finance_odds_where(args: FinanceOddsWhereArgs) -> Result<()> {
    #[derive(Serialize)]
    struct OddsIdLanguageKalshi {
        /// Kalshi identifiers are human-readable tickers.
        ///
        /// - event_ticker: groups a set of related markets (e.g. `KXFED-26MAR`)
        /// - market_ticker: a specific market within the event (e.g. `KXFED-26MAR-T3.50`)
        event_ticker_example: String,
        market_ticker_example: String,
    }

    #[derive(Serialize)]
    struct OddsIdLanguagePolymarket {
        /// Polymarket markets have numeric IDs.
        ///
        /// - market_id: numeric market id (e.g. `609655`)
        /// - event_id: numeric event id (sometimes also exposed as a slug/ticker in search results)
        market_id_example: String,
        event_id_example: String,
        event_slug_example: String,
    }

    #[derive(Serialize)]
    struct OddsIdLanguage {
        kalshi: OddsIdLanguageKalshi,
        polymarket: OddsIdLanguagePolymarket,
    }

    #[derive(Serialize)]
    struct OddsWhereResponse {
        cache_dir: String,
        kalshi_csv_path: String,
        polymarket_csv_path: String,
        merged_csv_path: String,
        csv_schema: Vec<&'static str>,
        id_language: OddsIdLanguage,
    }

    let cache_dir = args.cache_dir.unwrap_or_else(|| {
        directories::ProjectDirs::from("", "", "eli")
            .map(|d| d.cache_dir().join("odds"))
            .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"))
    });

    let resp = OddsWhereResponse {
        cache_dir: cache_dir.display().to_string(),
        kalshi_csv_path: cache_dir.join("kalshi_markets.csv").display().to_string(),
        polymarket_csv_path: cache_dir
            .join("polymarket_markets.csv")
            .display()
            .to_string(),
        merged_csv_path: cache_dir.join("all_markets.csv").display().to_string(),
        csv_schema: vec![
            "source",
            "ticker",
            "title",
            "event_ticker",
            "yes_price",
            "volume",
            "status",
            "probability",
            "category",
            "topic",
        ],
        id_language: OddsIdLanguage {
            kalshi: OddsIdLanguageKalshi {
                event_ticker_example: "KXFED-26MAR".to_string(),
                market_ticker_example: "KXFED-26MAR-T3.50".to_string(),
            },
            polymarket: OddsIdLanguagePolymarket {
                market_id_example: "609655".to_string(),
                event_id_example: "48802".to_string(),
                event_slug_example: "us-recession-by-end-of-2026".to_string(),
            },
        },
    };

    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

