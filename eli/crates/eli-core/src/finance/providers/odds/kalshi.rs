pub(crate) async fn fetch_odds_kalshi(req: OddsRequest) -> Result<OddsResponse> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(30))
        .connect_timeout(StdDuration::from_secs(10))
        .build()
        .map_err(|e| Error::Provider(format!("odds client init failed: {e}")))?;

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

        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at: Utc::now(),
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
            .map(|s| s.to_string())
            .unwrap_or_else(|| "open".to_string());

        let mut filtered: Vec<OddsListedEvent> = Vec::new();
        while page < max_pages {
            let mut query: Vec<(&str, String)> =
                vec![("status", status.clone()), ("limit", limit.to_string())];
            if let Some(ref cat) = req.category {
                let cat = cat.trim();
                if !cat.is_empty() {
                    query.push(("category", cat.to_string()));
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

        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at: Utc::now(),
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
            volume: Option<i64>,
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
                                filtered.push(OddsListedMarket {
                                    ticker: m.ticker,
                                    title: m.title,
                                    event_ticker: m.event_ticker,
                                    yes_price: m.yes_price,
                                    volume: m.volume,
                                    status: m.status,
                                    source: Some("kalshi".to_string()),
                                    market_id: None,
                                    event_id: None,
                                    slug: None,
                                    outcomes: None,
                                    outcome_prices: None,
                                    clob_token_ids: None,
                                    probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
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
                    return Ok(OddsResponse {
                        base_url: KALSHI_BASE_URL.to_string(),
                        generated_at: Utc::now(),
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
                    yes_price: m.yes_price,
                    volume: m.volume,
                    status: m.status,
                    source: Some("kalshi".to_string()),
                    market_id: None,
                    event_id: None,
                    slug: None,
                    outcomes: None,
                    outcome_prices: None,
                    clob_token_ids: None,
                    probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
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
        return Ok(OddsResponse {
            base_url: KALSHI_BASE_URL.to_string(),
            generated_at: Utc::now(),
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
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
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
                            status: m.status,
                            yes_price: m.yes_price,
                            yes_bid: m.yes_bid,
                            yes_ask: m.yes_ask,
                            volume: m.volume,
                            source: Some("kalshi".to_string()),
                            market_id: None,
                            event_id: None,
                            slug: None,
                            outcomes: None,
                            outcome_prices: None,
                            clob_token_ids: None,
                            probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
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
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
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
                        status: m.status,
                        yes_price: m.yes_price,
                        yes_bid: m.yes_bid,
                        yes_ask: m.yes_ask,
                        volume: m.volume,
                        source: Some("kalshi".to_string()),
                        market_id: None,
                        event_id: None,
                        slug: None,
                        outcomes: None,
                        outcome_prices: None,
                        clob_token_ids: None,
                        probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
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
                yes_bid: Option<i64>,
                #[serde(default)]
                yes_ask: Option<i64>,
                #[serde(default)]
                volume: Option<i64>,
            }

            let url = format!("{}/markets/{}", KALSHI_BASE_URL, ticker);
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("kalshi market fetch failed: {e}")))?;
            if !resp.status().is_success() {
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
                    status: m.status,
                    yes_price: m.yes_price,
                    yes_bid: m.yes_bid,
                    yes_ask: m.yes_ask,
                    volume: m.volume,
                    source: Some("kalshi".to_string()),
                    market_id: None,
                    event_id: None,
                    slug: None,
                    outcomes: None,
                    outcome_prices: None,
                    clob_token_ids: None,
                    probability_yes: m.yes_price.map(|p| p as f64 / 100.0),
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
    Ok(OddsResponse {
        base_url: KALSHI_BASE_URL.to_string(),
        generated_at: Utc::now(),
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

