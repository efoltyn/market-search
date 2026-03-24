/// Parse Kalshi `last_price_dollars` (e.g. "0.75") into a probability 0.0–1.0.
/// Falls back to `last_price` (cents i64, e.g. 75 → 0.75) if dollars string is absent.
fn kalshi_prob(yes_price_cents: Option<i64>, last_price_dollars: Option<&str>) -> Option<f64> {
    if let Some(s) = last_price_dollars {
        let s = s.trim();
        if !s.is_empty() {
            if let Ok(v) = s.parse::<f64>() {
                if v >= 0.0 && v <= 1.0 {
                    return Some(v);
                }
            }
        }
    }
    yes_price_cents.map(|p| p as f64 / 100.0)
}

/// Parse Kalshi volume: prefer `volume_fp` (string, e.g. "1234.00" = contracts),
/// fall back to legacy `volume` (i64 contracts). Multiply by 100 to get approximate
/// dollar volume (contracts are $0-$1 notional, but we report in cents convention).
/// As of 2026-03-12 Kalshi deprecated legacy integer fields — volume_fp is required.
fn kalshi_volume(volume_fp: Option<&str>, volume_legacy: Option<i64>) -> Option<i64> {
    if let Some(s) = volume_fp {
        let s = s.trim();
        if !s.is_empty() {
            if let Ok(v) = s.parse::<f64>() {
                return Some((v * 100.0) as i64);
            }
        }
    }
    volume_legacy.map(|v| v * 100)
}

#[derive(Clone, Deserialize)]
struct NestedEventsResp {
    #[serde(default)]
    events: Vec<NestedEventEntry>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Clone, Deserialize)]
struct NestedEventEntry {
    event_ticker: String,
    title: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    series_ticker: Option<String>,
    #[serde(default)]
    markets: Vec<NestedMarketEntry>,
}

#[derive(Clone, Deserialize)]
struct NestedMarketEntry {
    ticker: String,
    title: String,
    #[serde(default)]
    event_ticker: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "last_price", alias = "yes_price")]
    yes_price: Option<i64>,
    #[serde(default)]
    last_price_dollars: Option<String>,
    #[serde(default)]
    volume: Option<i64>,
    #[serde(default)]
    volume_fp: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    yes_sub_title: Option<String>,
}

fn kalshi_search_score(query_phrase: &str, query_terms: &[String], fields: &[(&str, i64)]) -> i64 {
    let mut score = 0i64;
    let mut term_hits = 0usize;
    let mut phrase_match = false;

    for (field, weight) in fields {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let lower = field.to_ascii_lowercase();
        if !query_phrase.is_empty() && lower.contains(query_phrase) {
            phrase_match = true;
            score += weight.saturating_mul(4);
        }
        let hits = matched_term_count(field, query_terms);
        term_hits = term_hits.saturating_add(hits);
        score += (hits as i64).saturating_mul(*weight);
    }

    if !matches_query_terms(phrase_match, term_hits, query_terms.len()) {
        return 0;
    }

    score
}

fn kalshi_search_market_display_title(
    event_title: &str,
    title: &str,
    yes_sub_title: Option<&str>,
    subtitle: Option<&str>,
) -> String {
    let title = title.trim();
    let detail = yes_sub_title.or(subtitle).unwrap_or_default().trim();
    if title.is_empty() {
        return event_title.trim().to_string();
    }
    if detail.is_empty() {
        return title.to_string();
    }
    if title
        .to_ascii_lowercase()
        .contains(&detail.to_ascii_lowercase())
    {
        return title.to_string();
    }
    format!("{title} ({detail})")
}

fn kalshi_search_status_matches(status: Option<&str>, effective_status: &str) -> bool {
    if effective_status.eq_ignore_ascii_case("any") {
        return true;
    }
    let status = status.unwrap_or_default().trim();
    if effective_status.eq_ignore_ascii_case("open") {
        return matches!(
            status.to_ascii_lowercase().as_str(),
            "" | "open" | "active" | "initialized"
        );
    }
    status.eq_ignore_ascii_case(effective_status)
}

async fn search_kalshi_nested_events(
    client: &reqwest::Client,
    search_query: &str,
    category_filter: Option<&str>,
    status: Option<&str>,
    limit: usize,
    max_pages: usize,
) -> Result<(Vec<OddsListedEvent>, Vec<OddsListedMarket>)> {
    let query_phrase = search_query.trim().to_ascii_lowercase();
    let query_terms = search_terms(&query_phrase);
    if query_terms.is_empty() && query_phrase.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut category_queries: Vec<Option<String>> = if let Some(category) = category_filter
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        vec![Some(category.to_string())]
    } else {
        Vec::new()
    };

    if category_queries.is_empty() {
        if let Ok(tags_map) = fetch_kalshi_tags_by_categories(client).await {
            category_queries = derive_kalshi_categories_for_query(&query_phrase, &tags_map)
                .into_iter()
                .map(Some)
                .collect();
        }
    }

    let mut event_queries = category_queries;
    event_queries.push(None);

    let effective_status = status
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("open")
        .to_ascii_lowercase();

    let mut events_by_ticker: HashMap<String, (OddsListedEvent, i64)> = HashMap::new();
    let mut markets_by_ticker: HashMap<String, (OddsListedMarket, i64, i64)> = HashMap::new();
    let market_target = limit.saturating_mul(3).clamp(30, 300);
    let event_target = limit.saturating_mul(2).clamp(20, 200);

    for (query_idx, category) in event_queries.into_iter().enumerate() {
        if query_idx > 0
            && category.is_none()
            && markets_by_ticker.len() >= limit.max(10)
            && events_by_ticker.len() >= limit.max(10)
        {
            break;
        }
        let mut cursor: Option<String> = None;
        for page_idx in 0..max_pages.max(1) {
            if query_idx > 0 || page_idx > 0 {
                tokio::time::sleep(StdDuration::from_millis(75)).await;
            }

            let mut query: Vec<(&str, String)> = vec![
                ("status", effective_status.clone()),
                ("limit", 200usize.to_string()),
                ("with_nested_markets", "true".to_string()),
            ];
            if let Some(ref category_value) = category {
                query.push(("category", category_value.clone()));
            }
            if let Some(ref current_cursor) = cursor {
                if !current_cursor.trim().is_empty() {
                    query.push(("cursor", current_cursor.trim().to_string()));
                }
            }

            let url = format!("{}/events", KALSHI_BASE_URL);
            let resp = match client.get(&url).query(&query).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    warn!("kalshi nested events search fetch failed: {e}");
                    break;
                }
            };
            if !resp.status().is_success() {
                warn!(
                    "kalshi nested events search failed with http {} (continuing)",
                    resp.status()
                );
                break;
            }
            let body: NestedEventsResp = match resp.json().await {
                Ok(body) => body,
                Err(e) => {
                    warn!("kalshi nested events search parse failed: {e}");
                    break;
                }
            };
            if body.events.is_empty() {
                break;
            }

            for event in body.events {
                let category_value = event.category.clone();
                let event_score = kalshi_search_score(
                    &query_phrase,
                    &query_terms,
                    &[
                        (&event.title, 12),
                        (&event.event_ticker, 8),
                        (category_value.as_deref().unwrap_or_default(), 4),
                    ],
                );
                let mut best_market_score = 0i64;

                for market in event.markets {
                    if !kalshi_search_status_matches(market.status.as_deref(), &effective_status) {
                        continue;
                    }
                    let display_title = kalshi_search_market_display_title(
                        &event.title,
                        &market.title,
                        market.yes_sub_title.as_deref(),
                        market.subtitle.as_deref(),
                    );
                    let market_score = kalshi_search_score(
                        &query_phrase,
                        &query_terms,
                        &[
                            (&display_title, 16),
                            (&event.title, 8),
                            (&event.event_ticker, 5),
                            (&market.ticker, 5),
                            (category_value.as_deref().unwrap_or_default(), 3),
                        ],
                    );
                    if market_score <= 0 {
                        continue;
                    }
                    best_market_score = best_market_score.max(market_score);

                    let market_volume = kalshi_volume(market.volume_fp.as_deref(), market.volume)
                        .unwrap_or_default();
                    let listed_market = OddsListedMarket {
                        ticker: market.ticker.clone(),
                        title: display_title,
                        event_ticker: market
                            .event_ticker
                            .clone()
                            .unwrap_or_else(|| event.event_ticker.clone()),
                        freshness: odds_freshness(None),
                        yes_price: market.yes_price,
                        volume: kalshi_volume(market.volume_fp.as_deref(), market.volume),
                        status: market.status.clone(),
                        source: Some("kalshi".to_string()),
                        market_id: None,
                        event_id: None,
                        slug: None,
                        outcomes: None,
                        outcome_prices: None,
                        clob_token_ids: None,
                        probability_yes: kalshi_prob(
                            market.yes_price,
                            market.last_price_dollars.as_deref(),
                        ),
                        category: category_value.clone(),
                    };
                    let total_score = market_score + event_score / 2;
                    match markets_by_ticker.get_mut(&market.ticker) {
                        Some(existing) => {
                            if total_score > existing.1
                                || (total_score == existing.1 && market_volume > existing.2)
                            {
                                *existing = (listed_market, total_score, market_volume);
                            }
                        }
                        None => {
                            markets_by_ticker.insert(
                                market.ticker.clone(),
                                (listed_market, total_score, market_volume),
                            );
                        }
                    }
                }

                let combined_event_score = event_score + best_market_score / 2;
                if combined_event_score <= 0 {
                    continue;
                }

                let listed_event = OddsListedEvent {
                    ticker: event.event_ticker.clone(),
                    title: event.title.clone(),
                    category: category_value,
                    series_ticker: event.series_ticker.clone(),
                    source: Some("kalshi".to_string()),
                    event_id: None,
                    slug: None,
                    tags: None,
                };
                match events_by_ticker.get_mut(&event.event_ticker) {
                    Some(existing) => {
                        if combined_event_score > existing.1 {
                            *existing = (listed_event, combined_event_score);
                        }
                    }
                    None => {
                        events_by_ticker.insert(
                            event.event_ticker.clone(),
                            (listed_event, combined_event_score),
                        );
                    }
                }
            }

            cursor = body.cursor.filter(|value| !value.trim().is_empty());
            if cursor.is_none()
                || (markets_by_ticker.len() >= market_target
                    && events_by_ticker.len() >= event_target)
            {
                break;
            }
        }

        if markets_by_ticker.len() >= market_target && events_by_ticker.len() >= event_target {
            break;
        }
    }

    let mut events: Vec<(OddsListedEvent, i64)> = events_by_ticker.into_values().collect();
    events.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.ticker.cmp(&b.0.ticker)));
    let mut markets: Vec<(OddsListedMarket, i64, i64)> = markets_by_ticker.into_values().collect();
    markets.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.ticker.cmp(&b.0.ticker))
    });

    Ok((
        events
            .into_iter()
            .take(limit)
            .map(|(event, _)| event)
            .collect(),
        markets
            .into_iter()
            .take(limit)
            .map(|(market, _, _)| market)
            .collect(),
    ))
}

pub(crate) async fn fetch_odds_kalshi(req: OddsRequest) -> Result<OddsResponse> {
    let client = &*crate::finance::shared_client::GENERAL;

    // Handle list_series mode: return available series (optionally filtered)
    if req.list_series {
        #[derive(Deserialize)]
        struct SeriesListResp {
            series: Vec<RawSeriesEntry>,
            #[serde(default)]
            cursor: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawSeriesEntry {
            ticker: String,
            title: String,
            category: Option<String>,
            frequency: Option<String>,
        }

        let limit = req.limit.unwrap_or(200).clamp(1, 1000);
        let max_pages = req.max_pages.unwrap_or(1).max(1);
        let mut page = 0usize;
        let mut page_cursor = req.cursor.clone();
        let mut all_series: Vec<RawSeriesEntry> = Vec::new();
        while page < max_pages {
            let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
            if let Some(ref c) = page_cursor {
                if !c.trim().is_empty() {
                    query.push(("cursor", c.trim().to_string()));
                }
            }
            let url = format!("{}/series", KALSHI_BASE_URL);
            let resp = client
                .get(&url)
                .query(&query)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi series list failed: {e}")))?;

            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "kalshi series list failed: http {}",
                    resp.status()
                )));
            }

            let body: SeriesListResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi series list parse failed: {e}")))?;
            let count = body.series.len();
            all_series.extend(body.series);
            page += 1;
            page_cursor = body.cursor.filter(|c| !c.trim().is_empty());
            if count == 0 || page_cursor.is_none() {
                break;
            }
        }

        // Filter by category and/or search term
        let category_filter = req.category.as_deref().map(|s| s.trim().to_lowercase());
        let search_filter = req
            .search
            .as_deref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        let search_terms_filter = search_filter
            .as_deref()
            .map(search_terms)
            .unwrap_or_default();
        let filtered: Vec<OddsSeries> = all_series
            .into_iter()
            .filter(|s| {
                if let Some(ref cat) = category_filter {
                    if let Some(ref sc) = s.category {
                        if !sc.to_lowercase().contains(cat) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                if let Some(ref search) = search_filter {
                    let title_match = s.title.to_lowercase().contains(search);
                    let ticker_match = s.ticker.to_lowercase().contains(search);
                    let term_hits = matched_term_count(&s.title, &search_terms_filter)
                        + matched_term_count(&s.ticker, &search_terms_filter);
                    let term_match = matches_query_terms(
                        title_match || ticker_match,
                        term_hits,
                        search_terms_filter.len(),
                    );
                    if !term_match {
                        return false;
                    }
                }
                true
            })
            .map(|s| OddsSeries {
                ticker: s.ticker,
                title: s.title,
                category: s.category,
                frequency: s.frequency,
            })
            .collect();

        let generated_at = Utc::now();
        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at,
            schema_version: "finance.odds.v2".to_string(),
            freshness_summary: odds_response_freshness_summary(generated_at, &[], None),
            applied_policy: AppliedPolicy::default(),
            decision_trace: vec![],
            run_meta: odds_run_meta(0, 0, 0, filtered.len()),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor: None,
            available_series: Some(filtered),
            available_events: None,
            available_markets: None,
            available_tags: None,
            analytics: None,
            sources: None,
            field_semantics: default_odds_field_semantics(),
        });
    }

    // Handle list_events mode: return open events
    if req.list_events {
        #[derive(Deserialize)]
        struct EventsListResp {
            events: Vec<RawEventEntry>,
            #[serde(default)]
            cursor: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawEventEntry {
            event_ticker: String,
            title: String,
            category: Option<String>,
            series_ticker: Option<String>,
        }

        let search_filter = req
            .search
            .as_deref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        let search_terms_filter = search_filter
            .as_deref()
            .map(search_terms)
            .unwrap_or_default();
        let mut limit = req.limit.unwrap_or(100);
        if search_filter.is_some() && req.limit.is_none() {
            limit = 200;
        }
        if limit < 1 {
            limit = 1;
        } else if limit > 200 {
            limit = 200;
        }
        let max_pages = match req.max_pages {
            Some(n) => n.max(1),
            None => {
                if search_filter.is_some() {
                    let target = 500usize;
                    (target + limit - 1) / limit
                } else {
                    1
                }
            }
        };
        let mut page = 0usize;
        let mut page_cursor = req.cursor.clone();
        let mut cursor: Option<String> = None;
        let status = req
            .status
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let search_mode = search_filter.is_some()
            && req
                .series_ticker
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            && req
                .cursor
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty();

        if search_mode {
            if let Some(ref search) = search_filter {
                let search_pages = req.max_pages.unwrap_or(5).max(1);
                let (events, _) = search_kalshi_nested_events(
                    &client,
                    search,
                    req.category.as_deref(),
                    status.as_deref(),
                    limit,
                    search_pages,
                )
                .await?;
                if !events.is_empty() {
                    let generated_at = Utc::now();
                    return Ok(OddsResponse {
                        base_url: KALSHI_BASE_URL.to_string(),
                        generated_at,
                        schema_version: "finance.odds.v2".to_string(),
                        freshness_summary: odds_response_freshness_summary(generated_at, &[], None),
                        applied_policy: AppliedPolicy::default(),
                        decision_trace: vec!["kalshi_search_mode=nested_events".to_string()],
                        run_meta: odds_run_meta(events.len(), 0, events.len(), 0),
                        series: None,
                        events: vec![],
                        markets: vec![],
                        orderbook: None,
                        cursor: None,
                        available_series: None,
                        available_events: Some(events),
                        available_markets: None,
                        available_tags: None,
                        analytics: None,
                        sources: None,
                        field_semantics: default_odds_field_semantics(),
                    });
                }
            }
        }

        let mut filtered: Vec<OddsListedEvent> = Vec::new();
        while page < max_pages {
            // Pace pages to avoid 429 rate limits (skip delay on first page).
            // 200ms between pages keeps us well under Kalshi's rate ceiling.
            if page > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            // Early exit: if we've already found plenty of matching events,
            // stop paginating to save API budget and latency.
            if search_filter.is_some() && filtered.len() >= 30 {
                break;
            }
            let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
            if let Some(ref st) = status {
                query.push(("status", st.clone()));
            }
            if let Some(ref cat) = req.category {
                let cat = cat.trim();
                if !cat.is_empty() {
                    query.push(("category", cat.to_string()));
                }
            }
            if let Some(ref st) = req.series_ticker {
                let st = st.trim();
                if !st.is_empty() {
                    query.push(("series_ticker", st.to_string()));
                }
            }
            if let Some(ref c) = page_cursor {
                if !c.trim().is_empty() {
                    query.push(("cursor", c.trim().to_string()));
                }
            }

            let url = format!("{}/events", KALSHI_BASE_URL);
            let start_time = std::time::Instant::now();
            let resp = client
                .get(&url)
                .query(&query)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi events list failed: {e}")))?;

            let status_code = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| Error::Provider(format!("kalshi events read failed: {e}")))?;
            info!(
                target: "eli.finance.odds",
                url = %url,
                status = %status_code,
                bytes = body.len(),
                elapsed_ms = start_time.elapsed().as_millis(),
                "kalshi events list"
            );

            if !status_code.is_success() {
                return Err(Error::Provider(format!(
                    "kalshi events list failed: http {}",
                    status_code
                )));
            }

            let body: EventsListResp = match serde_json::from_str(&body) {
                Ok(parsed) => parsed,
                Err(e) => {
                    let debug_path = write_debug_payload("odds_events", &url, &body);
                    warn!(
                        target: "eli.finance.odds",
                        url = %url,
                        error = %e,
                        debug_path = debug_path.as_deref().unwrap_or(""),
                        "kalshi events parse failed"
                    );
                    return Err(Error::Provider(format!(
                        "kalshi events list parse failed: {e}"
                    )));
                }
            };

            for e in body.events {
                if let Some(ref search) = search_filter {
                    let title_match = e.title.to_lowercase().contains(search);
                    let ticker_match = e.event_ticker.to_lowercase().contains(search);
                    let term_hits = matched_term_count(&e.title, &search_terms_filter)
                        + matched_term_count(&e.event_ticker, &search_terms_filter);
                    let term_match = matches_query_terms(
                        title_match || ticker_match,
                        term_hits,
                        search_terms_filter.len(),
                    );
                    if !term_match {
                        continue;
                    }
                }
                filtered.push(OddsListedEvent {
                    ticker: e.event_ticker,
                    title: e.title,
                    category: e.category,
                    series_ticker: e.series_ticker,
                    source: Some("kalshi".to_string()),
                    event_id: None,
                    slug: None,
                    tags: None,
                });
            }

            page += 1;
            page_cursor = body.cursor.clone();
            cursor = body.cursor;
            if page_cursor.as_deref().unwrap_or("").is_empty() {
                break;
            }
        }

        let generated_at = Utc::now();
        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at,
            schema_version: "finance.odds.v2".to_string(),
            freshness_summary: odds_response_freshness_summary(generated_at, &[], None),
            applied_policy: AppliedPolicy::default(),
            decision_trace: vec![],
            run_meta: odds_run_meta(filtered.len(), 0, filtered.len(), 0),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor,
            available_series: None,
            available_events: Some(filtered),
            available_markets: None,
            available_tags: None,
            analytics: None,
            sources: None,
            field_semantics: default_odds_field_semantics(),
        });
    }

    // Handle list_markets mode: return open markets
    if req.list_markets {
        #[derive(Deserialize)]
        struct MarketsListResp {
            markets: Vec<RawMarketEntry>,
            #[serde(default)]
            cursor: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawMarketEntry {
            ticker: String,
            title: String,
            event_ticker: String,
            #[serde(default, rename = "last_price", alias = "yes_price")]
            yes_price: Option<i64>,
            #[serde(default)]
            last_price_dollars: Option<String>,
            #[serde(default)]
            volume: Option<i64>,
            #[serde(default)]
            volume_fp: Option<String>,
            #[serde(default)]
            status: Option<String>,
        }
        #[derive(Deserialize)]
        struct EventsListResp {
            events: Vec<RawEventEntry>,
            #[serde(default)]
            cursor: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawEventEntry {
            event_ticker: String,
            title: String,
            #[serde(default)]
            category: Option<String>,
        }

        let search_filter = req.search.as_deref().map(|s| s.trim().to_lowercase());
        let search_terms_filter = search_filter
            .as_deref()
            .map(search_terms)
            .unwrap_or_default();
        let limit = req.limit.unwrap_or(100);
        let max_pages = req.max_pages.unwrap_or(1).max(1);
        let mut page = 0usize;
        let mut page_cursor = req.cursor.clone();
        let mut cursor: Option<String> = None;
        let status = req
            .status
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let mut filtered: Vec<OddsListedMarket> = Vec::new();

        let search_mode = search_filter.is_some()
            && req
                .series_ticker
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            && req
                .event_ticker
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            && req
                .cursor
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty();

        if search_mode {
            if let Some(ref search) = search_filter {
                let search_pages = req.max_pages.unwrap_or(5).max(1);
                let (_, nested_markets) = search_kalshi_nested_events(
                    &client,
                    search,
                    req.category.as_deref(),
                    status.as_deref(),
                    limit,
                    search_pages,
                )
                .await?;
                if !nested_markets.is_empty() {
                    let analytics = build_odds_analytics_from_listed(&nested_markets);
                    let generated_at = Utc::now();
                    return Ok(OddsResponse {
                        base_url: KALSHI_BASE_URL.to_string(),
                        generated_at,
                        schema_version: "finance.odds.v2".to_string(),
                        freshness_summary: odds_response_freshness_summary(
                            generated_at,
                            &[],
                            Some(&nested_markets),
                        ),
                        applied_policy: AppliedPolicy::default(),
                        decision_trace: vec!["kalshi_search_mode=nested_events".to_string()],
                        run_meta: odds_run_meta(0, 0, 0, nested_markets.len()),
                        series: None,
                        events: vec![],
                        markets: vec![],
                        orderbook: None,
                        cursor: None,
                        available_series: None,
                        available_events: None,
                        available_markets: Some(nested_markets),
                        available_tags: None,
                        analytics,
                        sources: None,
                        field_semantics: default_odds_field_semantics(),
                    });
                }
            }

            let mut category_hints: Vec<String> = Vec::new();
            if let Some(search_query) = search_filter.as_deref() {
                if let Ok(tags_map) = fetch_kalshi_tags_by_categories(&client).await {
                    category_hints = derive_kalshi_categories_for_query(search_query, &tags_map);
                }
            }

            let effective_status = status
                .clone()
                .unwrap_or_else(|| "open".to_string())
                .to_ascii_lowercase();
            let event_limit = 100usize;
            let event_pages = max_pages.min(3).max(1);
            let mut event_queries: Vec<Option<String>> = if category_hints.is_empty() {
                vec![None]
            } else {
                category_hints.into_iter().map(Some).collect()
            };

            // Keep one generic query path in the mix for terms that aren't in category/tag map.
            if event_queries
                .iter()
                .all(|c| c.as_deref().map(str::trim).unwrap_or("").len() > 0)
            {
                event_queries.push(None);
            }

            let mut candidate_events: Vec<String> = Vec::new();
            let mut candidate_events_seen: HashSet<String> = HashSet::new();
            let candidate_cap = (limit.saturating_mul(max_pages)).clamp(50, 300);

            for category in event_queries {
                let mut event_cursor: Option<String> = None;
                for _ in 0..event_pages {
                    let mut query: Vec<(&str, String)> = vec![
                        ("status", effective_status.clone()),
                        ("limit", event_limit.to_string()),
                    ];
                    if let Some(ref cat) = category {
                        if !cat.trim().is_empty() {
                            query.push(("category", cat.trim().to_string()));
                        }
                    }
                    if let Some(ref c) = event_cursor {
                        if !c.trim().is_empty() {
                            query.push(("cursor", c.trim().to_string()));
                        }
                    }

                    let url = format!("{}/events", KALSHI_BASE_URL);
                    let resp = match client.get(&url).query(&query).send().await {
                        Ok(resp) => resp,
                        Err(e) => {
                            warn!("kalshi events prefilter request failed: {e}");
                            break;
                        }
                    };
                    if !resp.status().is_success() {
                        warn!(
                            "kalshi events prefilter failed with http {} (continuing)",
                            resp.status()
                        );
                        break;
                    }
                    let body: EventsListResp = match resp.json().await {
                        Ok(body) => body,
                        Err(e) => {
                            warn!("kalshi events prefilter parse failed: {e}");
                            break;
                        }
                    };

                    for event in body.events {
                        if let Some(ref search) = search_filter {
                            let title_match = event.title.to_ascii_lowercase().contains(search);
                            let ticker_match =
                                event.event_ticker.to_ascii_lowercase().contains(search);
                            let category_match = event
                                .category
                                .as_deref()
                                .map(|c| c.to_ascii_lowercase().contains(search))
                                .unwrap_or(false);
                            let term_hits = matched_term_count(&event.title, &search_terms_filter)
                                + matched_term_count(&event.event_ticker, &search_terms_filter)
                                + event
                                    .category
                                    .as_deref()
                                    .map(|c| matched_term_count(c, &search_terms_filter))
                                    .unwrap_or(0);
                            let term_match = matches_query_terms(
                                title_match || ticker_match || category_match,
                                term_hits,
                                search_terms_filter.len(),
                            );
                            if !term_match {
                                continue;
                            }
                        }
                        if candidate_events_seen.insert(event.event_ticker.clone()) {
                            candidate_events.push(event.event_ticker);
                            if candidate_events.len() >= candidate_cap {
                                break;
                            }
                        }
                    }

                    event_cursor = body.cursor.filter(|c| !c.trim().is_empty());
                    if event_cursor.is_none() || candidate_events.len() >= candidate_cap {
                        break;
                    }
                }
                if candidate_events.len() >= candidate_cap {
                    break;
                }
            }

            if !candidate_events.is_empty() {
                let mut seen_markets: HashSet<String> = HashSet::new();
                for event_ticker in candidate_events {
                    let mut local_cursor: Option<String> = None;
                    for _ in 0..max_pages {
                        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
                        if !effective_status.eq_ignore_ascii_case("any") {
                            query.push(("status", effective_status.clone()));
                        }
                        query.push(("event_ticker", event_ticker.clone()));
                        if let Some(ref c) = local_cursor {
                            if !c.trim().is_empty() {
                                query.push(("cursor", c.trim().to_string()));
                            }
                        }

                        let url = format!("{}/markets", KALSHI_BASE_URL);
                        let resp = match client.get(&url).query(&query).send().await {
                            Ok(resp) => resp,
                            Err(e) => {
                                warn!("kalshi markets targeted request failed: {e}");
                                break;
                            }
                        };
                        if !resp.status().is_success() {
                            warn!(
                                "kalshi markets targeted fetch failed with http {} (continuing)",
                                resp.status()
                            );
                            break;
                        }
                        let body: MarketsListResp = match resp.json().await {
                            Ok(body) => body,
                            Err(e) => {
                                warn!("kalshi markets targeted parse failed: {e}");
                                break;
                            }
                        };

                        for m in body.markets {
                            if let Some(ref search) = search_filter {
                                let title_match = m.title.to_ascii_lowercase().contains(search);
                                let ticker_match = m.ticker.to_ascii_lowercase().contains(search);
                                let term_hits = matched_term_count(&m.title, &search_terms_filter)
                                    + matched_term_count(&m.ticker, &search_terms_filter);
                                let term_match = matches_query_terms(
                                    title_match || ticker_match,
                                    term_hits,
                                    search_terms_filter.len(),
                                );
                                if !term_match {
                                    continue;
                                }
                            }
                            if seen_markets.insert(m.ticker.clone()) {
                                let prob =
                                    kalshi_prob(m.yes_price, m.last_price_dollars.as_deref());
                                filtered.push(OddsListedMarket {
                                    ticker: m.ticker,
                                    title: m.title,
                                    event_ticker: m.event_ticker,
                                    freshness: odds_freshness(None),
                                    yes_price: m.yes_price,
                                    volume: kalshi_volume(m.volume_fp.as_deref(), m.volume),
                                    status: m.status,
                                    source: Some("kalshi".to_string()),
                                    market_id: None,
                                    event_id: None,
                                    slug: None,
                                    outcomes: None,
                                    outcome_prices: None,
                                    clob_token_ids: None,
                                    probability_yes: prob,
                                    category: None,
                                });
                            }
                        }

                        local_cursor = body.cursor.filter(|c| !c.trim().is_empty());
                        if local_cursor.is_none() {
                            break;
                        }
                    }
                }

                if !filtered.is_empty() {
                    let analytics = build_odds_analytics_from_listed(&filtered);
                    let generated_at = Utc::now();
                    return Ok(OddsResponse {
                        base_url: KALSHI_BASE_URL.to_string(),
                        generated_at,
                        schema_version: "finance.odds.v2".to_string(),
                        freshness_summary: odds_response_freshness_summary(
                            generated_at,
                            &[],
                            Some(&filtered),
                        ),
                        applied_policy: AppliedPolicy::default(),
                        decision_trace: vec![],
                        run_meta: odds_run_meta(0, 0, 0, filtered.len()),
                        series: None,
                        events: vec![],
                        markets: vec![],
                        orderbook: None,
                        cursor: None,
                        available_series: None,
                        available_events: None,
                        available_markets: Some(filtered),
                        available_tags: None,
                        analytics,
                        sources: None,
                        field_semantics: default_odds_field_semantics(),
                    });
                }
            }
        }

        // Kalshi supports filtering markets by `series_ticker` directly on `/markets`,
        // so we avoid expanding series -> events -> markets (slower and more likely to 429).
        while page < max_pages {
            let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
            let effective_status = status.clone().unwrap_or_else(|| "open".to_string());
            if !effective_status.eq_ignore_ascii_case("any") {
                query.push(("status", effective_status));
            }
            if let Some(ref series) = req.series_ticker {
                let series = series.trim();
                if !series.is_empty() {
                    query.push(("series_ticker", series.to_string()));
                }
            }
            if let Some(ref event) = req.event_ticker {
                let event = event.trim();
                if !event.is_empty() {
                    query.push(("event_ticker", event.to_string()));
                }
            }
            if let Some(ref c) = page_cursor {
                if !c.trim().is_empty() {
                    query.push(("cursor", c.trim().to_string()));
                }
            }

            let url = format!("{}/markets", KALSHI_BASE_URL);
            let start_time = std::time::Instant::now();
            let resp = client
                .get(&url)
                .query(&query)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi markets list failed: {e}")))?;

            let status_code = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| Error::Provider(format!("kalshi markets read failed: {e}")))?;
            info!(
                target: "eli.finance.odds",
                url = %url,
                status = %status_code,
                bytes = body.len(),
                elapsed_ms = start_time.elapsed().as_millis(),
                "kalshi markets list"
            );

            if !status_code.is_success() {
                return Err(Error::Provider(format!(
                    "kalshi markets list failed: http {}",
                    status_code
                )));
            }

            let body: MarketsListResp = match serde_json::from_str(&body) {
                Ok(parsed) => parsed,
                Err(e) => {
                    let debug_path = write_debug_payload("odds_markets", &url, &body);
                    warn!(
                        target: "eli.finance.odds",
                        url = %url,
                        error = %e,
                        debug_path = debug_path.as_deref().unwrap_or(""),
                        "kalshi markets parse failed"
                    );
                    return Err(Error::Provider(format!(
                        "kalshi markets list parse failed: {e}"
                    )));
                }
            };

            for m in body.markets {
                if let Some(ref search) = search_filter {
                    let title_match = m.title.to_lowercase().contains(search);
                    let ticker_match = m.ticker.to_lowercase().contains(search);
                    let term_hits = matched_term_count(&m.title, &search_terms_filter)
                        + matched_term_count(&m.ticker, &search_terms_filter);
                    let term_match = matches_query_terms(
                        title_match || ticker_match,
                        term_hits,
                        search_terms_filter.len(),
                    );
                    if !term_match {
                        continue;
                    }
                }
                filtered.push(OddsListedMarket {
                    ticker: m.ticker,
                    title: m.title,
                    event_ticker: m.event_ticker,
                    freshness: odds_freshness(None),
                    yes_price: m.yes_price,
                    volume: kalshi_volume(m.volume_fp.as_deref(), m.volume),
                    status: m.status,
                    source: Some("kalshi".to_string()),
                    market_id: None,
                    event_id: None,
                    slug: None,
                    outcomes: None,
                    outcome_prices: None,
                    clob_token_ids: None,
                    probability_yes: kalshi_prob(m.yes_price, m.last_price_dollars.as_deref()),
                    category: None,
                });
            }

            page += 1;
            page_cursor = body.cursor.clone();
            cursor = body.cursor;
            if page_cursor.as_deref().unwrap_or("").is_empty() {
                break;
            }
        }

        let analytics = build_odds_analytics_from_listed(&filtered);
        let generated_at = Utc::now();
        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at,
            schema_version: "finance.odds.v2".to_string(),
            freshness_summary: odds_response_freshness_summary(generated_at, &[], Some(&filtered)),
            applied_policy: AppliedPolicy::default(),
            decision_trace: vec![],
            run_meta: odds_run_meta(0, 0, 0, filtered.len()),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor,
            available_series: None,
            available_events: None,
            available_markets: Some(filtered),
            available_tags: None,
            analytics,
            sources: None,
            field_semantics: default_odds_field_semantics(),
        });
    }

    if req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(Error::InvalidInput(
            "use --list-series, --list-events, --list-markets, or provide series/event/market ticker".to_string(),
        ));
    }

    if req.include_orderbook && req.market_ticker.as_deref().unwrap_or("").trim().is_empty() {
        return Err(Error::InvalidInput(
            "market_ticker is required when include_orderbook is true".to_string(),
        ));
    }

    let mut series: Option<OddsSeries> = None;
    let mut events: Vec<OddsEvent> = Vec::new();
    let mut markets: Vec<OddsMarket> = Vec::new();
    let mut cursor: Option<String> = None;

    if let Some(raw) = req.series_ticker.as_deref() {
        let ticker = raw.trim();
        if !ticker.is_empty() {
            #[derive(Deserialize)]
            struct SeriesResp {
                series: RawSeries,
            }
            #[derive(Deserialize)]
            struct RawSeries {
                ticker: String,
                title: String,
                category: Option<String>,
                frequency: Option<String>,
            }

            let url = format!("{}/series/{}", KALSHI_BASE_URL, ticker);
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi series fetch failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "kalshi series fetch failed: http {}",
                    resp.status()
                )));
            }
            let body: SeriesResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi series parse failed: {e}")))?;

            series = Some(OddsSeries {
                ticker: body.series.ticker,
                title: body.series.title,
                category: body.series.category,
                frequency: body.series.frequency,
            });

            #[derive(Deserialize)]
            struct MarketsResp {
                markets: Vec<RawMarket>,
                #[serde(default)]
                cursor: Option<String>,
            }
            #[derive(Deserialize)]
            struct RawMarket {
                ticker: String,
                title: String,
                event_ticker: String,
                #[serde(default)]
                status: Option<String>,
                #[serde(default, rename = "last_price", alias = "yes_price")]
                yes_price: Option<i64>,
                #[serde(default)]
                last_price_dollars: Option<String>,
                #[serde(default)]
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
                #[serde(default)]
                volume_fp: Option<String>,
            }

            // If an event_ticker is also provided, list markets by event instead of series.
            if req.event_ticker.as_deref().unwrap_or("").trim().is_empty() {
                let mut page = 0usize;
                let max_pages = req.max_pages.unwrap_or(1).max(1);
                let mut page_cursor = req.cursor.clone();

                while page < max_pages {
                    let mut query: Vec<(&str, String)> =
                        vec![("series_ticker", ticker.to_string())];
                    let status = req
                        .status
                        .as_ref()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "open".to_string());
                    query.push(("status", status));
                    if req.status.is_none() {
                        query.push(("events_status", "open".to_string()));
                    }
                    if let Some(limit) = req.limit {
                        query.push(("limit", limit.to_string()));
                    }
                    if let Some(c) = page_cursor.as_ref() {
                        if !c.trim().is_empty() {
                            query.push(("cursor", c.to_string()));
                        }
                    }

                    let url = format!("{}/markets", KALSHI_BASE_URL);
                    let resp = client.get(url).query(&query).send().await.map_err(|e| {
                        Error::Provider(format!("kalshi markets fetch failed: {e}"))
                    })?;
                    if !resp.status().is_success() {
                        return Err(Error::Provider(format!(
                            "kalshi markets fetch failed: http {}",
                            resp.status()
                        )));
                    }
                    let body: MarketsResp = resp.json().await.map_err(|e| {
                        Error::Provider(format!("kalshi markets parse failed: {e}"))
                    })?;

                    for m in body.markets {
                        markets.push(OddsMarket {
                            ticker: m.ticker,
                            title: m.title,
                            event_ticker: m.event_ticker,
                            freshness: odds_freshness(None),
                            status: m.status,
                            yes_price: m.yes_price,
                            yes_bid: m.yes_bid,
                            yes_ask: m.yes_ask,
                            volume: kalshi_volume(m.volume_fp.as_deref(), m.volume),
                            source: Some("kalshi".to_string()),
                            market_id: None,
                            event_id: None,
                            slug: None,
                            outcomes: None,
                            outcome_prices: None,
                            clob_token_ids: None,
                            probability_yes: kalshi_prob(
                                m.yes_price,
                                m.last_price_dollars.as_deref(),
                            ),
                            outcome_best_bids: None,
                            outcome_best_asks: None,
                            orderbook_timestamp: None,
                        });
                    }

                    page += 1;
                    page_cursor = body.cursor.clone();
                    cursor = body.cursor;
                    if page_cursor.as_deref().unwrap_or("").is_empty() {
                        break;
                    }
                }
            }
        }
    }

    if let Some(raw) = req.event_ticker.as_deref() {
        let ticker = raw.trim();
        if !ticker.is_empty() {
            #[derive(Deserialize)]
            struct EventResp {
                event: RawEvent,
            }
            #[derive(Deserialize)]
            struct RawEvent {
                #[serde(rename = "event_ticker")]
                ticker: String,
                title: String,
                category: Option<String>,
            }

            let url = format!("{}/events/{}", KALSHI_BASE_URL, ticker);
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi event fetch failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "kalshi event fetch failed: http {}",
                    resp.status()
                )));
            }
            let body: EventResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi event parse failed: {e}")))?;

            events.push(OddsEvent {
                ticker: body.event.ticker,
                title: body.event.title,
                category: body.event.category,
                source: Some("kalshi".to_string()),
                event_id: None,
                slug: None,
                tags: None,
            });

            // List markets for this event (unless series listing already covered, or user didn't ask for any listing).
            #[derive(Deserialize)]
            struct MarketsResp {
                markets: Vec<RawMarket>,
                #[serde(default)]
                cursor: Option<String>,
            }
            #[derive(Deserialize)]
            struct RawMarket {
                ticker: String,
                title: String,
                event_ticker: String,
                #[serde(default)]
                status: Option<String>,
                #[serde(default, rename = "last_price", alias = "yes_price")]
                yes_price: Option<i64>,
                #[serde(default)]
                last_price_dollars: Option<String>,
                #[serde(default)]
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
                #[serde(default)]
                volume_fp: Option<String>,
            }

            let mut page = 0usize;
            let max_pages = req.max_pages.unwrap_or(1).max(1);
            let mut page_cursor = req.cursor.clone();

            while page < max_pages {
                let mut query: Vec<(&str, String)> = vec![("event_ticker", ticker.to_string())];
                let status = req
                    .status
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "open".to_string());
                query.push(("status", status));
                if req.status.is_none() {
                    query.push(("events_status", "open".to_string()));
                }
                if let Some(limit) = req.limit {
                    query.push(("limit", limit.to_string()));
                }
                if let Some(c) = page_cursor.as_ref() {
                    if !c.trim().is_empty() {
                        query.push(("cursor", c.to_string()));
                    }
                }

                let url = format!("{}/markets", KALSHI_BASE_URL);
                let resp =
                    client.get(url).query(&query).send().await.map_err(|e| {
                        Error::Provider(format!("kalshi markets fetch failed: {e}"))
                    })?;
                if !resp.status().is_success() {
                    return Err(Error::Provider(format!(
                        "kalshi markets fetch failed: http {}",
                        resp.status()
                    )));
                }
                let body: MarketsResp = resp
                    .json()
                    .await
                    .map_err(|e| Error::Provider(format!("kalshi markets parse failed: {e}")))?;

                for m in body.markets {
                    markets.push(OddsMarket {
                        ticker: m.ticker,
                        title: m.title,
                        event_ticker: m.event_ticker,
                        freshness: odds_freshness(None),
                        status: m.status,
                        yes_price: m.yes_price,
                        yes_bid: m.yes_bid,
                        yes_ask: m.yes_ask,
                        volume: kalshi_volume(m.volume_fp.as_deref(), m.volume),
                        source: Some("kalshi".to_string()),
                        market_id: None,
                        event_id: None,
                        slug: None,
                        outcomes: None,
                        outcome_prices: None,
                        clob_token_ids: None,
                        probability_yes: kalshi_prob(m.yes_price, m.last_price_dollars.as_deref()),
                        outcome_best_bids: None,
                        outcome_best_asks: None,
                        orderbook_timestamp: None,
                    });
                }

                page += 1;
                page_cursor = body.cursor.clone();
                cursor = body.cursor;
                if page_cursor.as_deref().unwrap_or("").is_empty() {
                    break;
                }
            }
        }
    }

    if let Some(raw) = req.market_ticker.as_deref() {
        let ticker = raw.trim();
        if !ticker.is_empty() {
            #[derive(Deserialize)]
            struct MarketResp {
                market: RawMarket,
            }
            #[derive(Deserialize)]
            struct RawMarket {
                ticker: String,
                title: String,
                event_ticker: String,
                #[serde(default)]
                status: Option<String>,
                #[serde(default, rename = "last_price", alias = "yes_price")]
                yes_price: Option<i64>,
                #[serde(default)]
                last_price_dollars: Option<String>,
                #[serde(default)]
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
                #[serde(default)]
                volume_fp: Option<String>,
            }

            let url = format!("{}/markets/{}", KALSHI_BASE_URL, ticker);
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi market fetch failed: {e}")))?;
            if !resp.status().is_success() {
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    if let Some(candidates) = load_kalshi_market_candidates_from_cache(ticker) {
                        let generated_at = Utc::now();
                        return Ok(OddsResponse {
                            base_url: KALSHI_BASE_URL.to_string(),
                            generated_at,
                            schema_version: "finance.odds.v2".to_string(),
                            freshness_summary: odds_response_freshness_summary(
                                generated_at,
                                &[],
                                Some(&candidates),
                            ),
                            applied_policy: AppliedPolicy::default(),
                            decision_trace: vec![format!(
                                "market_lookup_fallback=cache_candidates:{}",
                                ticker
                            )],
                            run_meta: odds_run_meta(0, 0, 0, candidates.len()),
                            series: None,
                            events: Vec::new(),
                            markets: Vec::new(),
                            orderbook: None,
                            cursor: None,
                            available_series: None,
                            available_events: None,
                            available_markets: Some(candidates),
                            available_tags: None,
                            analytics: None,
                            sources: None,
                            field_semantics: default_odds_field_semantics(),
                        });
                    }
                }
                return Err(Error::Provider(format!(
                    "kalshi market fetch failed: http {}",
                    resp.status()
                )));
            }
            let body: MarketResp = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("kalshi market parse failed: {e}")))?;

            let m = body.market;
            if !markets.iter().any(|existing| existing.ticker == m.ticker) {
                markets.push(OddsMarket {
                    ticker: m.ticker,
                    title: m.title,
                    event_ticker: m.event_ticker,
                    freshness: odds_freshness(None),
                    status: m.status,
                    yes_price: m.yes_price,
                    yes_bid: m.yes_bid,
                    yes_ask: m.yes_ask,
                    volume: kalshi_volume(m.volume_fp.as_deref(), m.volume),
                    source: Some("kalshi".to_string()),
                    market_id: None,
                    event_id: None,
                    slug: None,
                    outcomes: None,
                    outcome_prices: None,
                    clob_token_ids: None,
                    probability_yes: kalshi_prob(m.yes_price, m.last_price_dollars.as_deref()),
                    outcome_best_bids: None,
                    outcome_best_asks: None,
                    orderbook_timestamp: None,
                });
            }
        }
    }

    let mut orderbook: Option<OddsOrderbook> = None;
    if req.include_orderbook {
        if let Some(raw) = req.market_ticker.as_deref() {
            let ticker = raw.trim();
            if !ticker.is_empty() {
                #[derive(Deserialize)]
                struct OrderbookResp {
                    orderbook: RawOrderbook,
                }
                #[derive(Deserialize)]
                struct RawOrderbook {
                    #[serde(default)]
                    yes: Option<Vec<[i64; 2]>>,
                    #[serde(default)]
                    no: Option<Vec<[i64; 2]>>,
                }

                let url = format!("{}/markets/{}/orderbook", KALSHI_BASE_URL, ticker);
                let resp =
                    client.get(url).send().await.map_err(|e| {
                        Error::Provider(format!("kalshi orderbook fetch failed: {e}"))
                    })?;
                if !resp.status().is_success() {
                    return Err(Error::Provider(format!(
                        "kalshi orderbook fetch failed: http {}",
                        resp.status()
                    )));
                }
                let body: OrderbookResp = resp
                    .json()
                    .await
                    .map_err(|e| Error::Provider(format!("kalshi orderbook parse failed: {e}")))?;

                let depth = req.orderbook_depth.unwrap_or(5).max(1);
                let yes = body
                    .orderbook
                    .yes
                    .unwrap_or_default()
                    .into_iter()
                    .take(depth)
                    .map(|pair| OddsOrderLevel {
                        price: pair[0],
                        quantity: pair[1],
                    })
                    .collect::<Vec<_>>();
                let no = body
                    .orderbook
                    .no
                    .unwrap_or_default()
                    .into_iter()
                    .take(depth)
                    .map(|pair| OddsOrderLevel {
                        price: pair[0],
                        quantity: pair[1],
                    })
                    .collect::<Vec<_>>();

                orderbook = Some(OddsOrderbook {
                    market_ticker: ticker.to_string(),
                    yes,
                    no,
                });
            }
        }
    }

    let analytics = build_odds_analytics(&markets);
    let generated_at = Utc::now();
    Ok(OddsResponse {
        base_url: KALSHI_BASE_URL.to_string(),
        generated_at,
        schema_version: "finance.odds.v2".to_string(),
        freshness_summary: odds_response_freshness_summary(generated_at, &markets, None),
        applied_policy: AppliedPolicy::default(),
        decision_trace: vec![],
        run_meta: odds_run_meta(events.len(), markets.len(), 0, 0),
        series,
        events,
        markets,
        orderbook,
        cursor,
        available_series: None,
        available_events: None,
        available_markets: None,
        available_tags: None,
        analytics,
        sources: None,
        field_semantics: default_odds_field_semantics(),
    })
}

fn load_kalshi_market_candidates_from_cache(
    requested_ticker: &str,
) -> Option<Vec<OddsListedMarket>> {
    #[derive(serde::Deserialize)]
    struct RawCacheRow {
        source: String,
        ticker: String,
        title: String,
        event_ticker: String,
        yes_price: String,
        volume: String,
        status: String,
        probability: String,
    }

    let event_prefix = requested_ticker.split('-').next()?.trim();
    if event_prefix.is_empty() {
        return None;
    }
    let cache_dir = directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));
    let csv_path = cache_dir.join("all_markets.csv");
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(csv_path)
        .ok()?;
    let mut out = Vec::new();
    for row in rdr.deserialize::<RawCacheRow>() {
        let Ok(row) = row else { continue };
        if !row.source.trim().eq_ignore_ascii_case("kalshi") {
            continue;
        }
        if row.event_ticker.trim() != event_prefix {
            continue;
        }
        let yes_price = row.yes_price.trim().parse::<i64>().ok();
        let volume = row.volume.trim().parse::<f64>().ok().map(|v| v as i64);
        let probability_yes = row
            .probability
            .trim()
            .parse::<f64>()
            .ok()
            .or_else(|| yes_price.map(|p| p as f64 / 100.0));
        out.push(OddsListedMarket {
            ticker: row.ticker,
            title: row.title,
            event_ticker: row.event_ticker,
            freshness: odds_freshness(None),
            category: None,
            yes_price,
            volume,
            status: if row.status.trim().is_empty() {
                None
            } else {
                Some(row.status)
            },
            source: Some("kalshi".to_string()),
            market_id: None,
            event_id: None,
            slug: None,
            outcomes: None,
            outcome_prices: None,
            clob_token_ids: None,
            probability_yes,
        });
    }
    out.sort_by(|a, b| {
        b.volume
            .unwrap_or(0)
            .cmp(&a.volume.unwrap_or(0))
            .then_with(|| a.ticker.cmp(&b.ticker))
    });
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kalshi_open_search_treats_active_as_live() {
        assert!(kalshi_search_status_matches(Some("active"), "open"));
        assert!(kalshi_search_status_matches(Some("initialized"), "open"));
        assert!(kalshi_search_status_matches(Some("open"), "open"));
        assert!(!kalshi_search_status_matches(Some("closed"), "open"));
    }

    #[test]
    fn kalshi_market_display_title_includes_detail_once() {
        let rendered = kalshi_search_market_display_title(
            "Will there be a recession in 2026?",
            "Will there be a recession in 2026?",
            Some("Starts"),
            None,
        );
        assert_eq!(rendered, "Will there be a recession in 2026? (Starts)");

        let unchanged = kalshi_search_market_display_title(
            "",
            "Will oil settle above $90? (Above $90)",
            Some("Above $90"),
            None,
        );
        assert_eq!(unchanged, "Will oil settle above $90? (Above $90)");
    }
}
