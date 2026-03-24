pub(crate) async fn fetch_odds_polymarket(req: &OddsRequest) -> Result<OddsResponse> {
    let client = &*crate::finance::shared_client::GENERAL;

    #[derive(Deserialize, Clone)]
    struct PolyTag {
        id: serde_json::Value,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        slug: Option<String>,
    }

    #[derive(Deserialize)]
    struct PolyMarket {
        id: serde_json::Value,
        #[serde(default)]
        question: Option<String>,
        #[serde(default, rename = "clobTokenIds")]
        clob_token_ids: serde_json::Value,
        #[serde(default)]
        outcomes: serde_json::Value,
        #[serde(default, rename = "outcomePrices")]
        outcome_prices: serde_json::Value,
        #[serde(default, rename = "volumeNum")]
        volume_num: Option<f64>,
        #[serde(default)]
        volume: Option<serde_json::Value>,
        #[serde(default, rename = "bestBid")]
        best_bid: Option<f64>,
        #[serde(default, rename = "bestAsk")]
        best_ask: Option<f64>,
        #[serde(default, rename = "lastTradePrice")]
        last_trade_price: Option<f64>,
    }

    #[derive(Deserialize)]
    struct PolyEvent {
        id: serde_json::Value,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        active: Option<bool>,
        #[serde(default)]
        closed: Option<bool>,
        #[serde(default)]
        tags: Option<Vec<PolyTag>>,
        #[serde(default)]
        markets: Option<Vec<PolyMarket>>,
    }

    #[derive(Deserialize)]
    struct PolySearchMarket {
        id: serde_json::Value,
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        outcomes: serde_json::Value,
        #[serde(default, rename = "outcomePrices")]
        outcome_prices: serde_json::Value,
        #[serde(default, rename = "clobTokenIds")]
        clob_token_ids: serde_json::Value,
        #[serde(default, rename = "volumeNum")]
        volume_num: Option<f64>,
        #[serde(default)]
        volume: Option<serde_json::Value>,
    }

    #[derive(Deserialize)]
    struct PolySearchEvent {
        id: serde_json::Value,
        #[serde(default)]
        ticker: Option<String>,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        category: Option<String>,
        #[serde(default)]
        active: Option<bool>,
        #[serde(default)]
        closed: Option<bool>,
        #[serde(default)]
        tags: Option<Vec<PolyTag>>,
        #[serde(default)]
        markets: Option<Vec<PolySearchMarket>>,
    }

    #[derive(Deserialize)]
    struct PolySearchPagination {
        #[serde(default, rename = "hasMore")]
        has_more: Option<bool>,
    }

    #[derive(Deserialize)]
    struct PolySearchResp {
        #[serde(default)]
        events: Option<Vec<PolySearchEvent>>,
        #[serde(default)]
        pagination: Option<PolySearchPagination>,
    }

    /// Parse Polymarket volume to cents (i64). Prefers `volume_num` (f64 USD),
    /// falls back to parsing `volume` JSON value as string/number.
    fn poly_volume_cents(volume_num: Option<f64>, volume: &Option<serde_json::Value>) -> Option<i64> {
        if let Some(v) = volume_num {
            if v > 0.0 {
                return Some((v * 100.0) as i64);
            }
        }
        match volume {
            Some(serde_json::Value::Number(n)) => n.as_f64().filter(|v| *v > 0.0).map(|v| (v * 100.0) as i64),
            Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok().filter(|v| *v > 0.0).map(|v| (v * 100.0) as i64),
            _ => None,
        }
    }

    let search_filter = req
        .search
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

    if req.list_tags {
        let limit = req.limit.unwrap_or(100).max(1);
        let max_pages = match req.max_pages {
            Some(n) => n.max(1),
            None => {
                let target = 500usize;
                (target + limit - 1) / limit
            }
        };
        let mut offset = req
            .cursor
            .as_deref()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut page = 0usize;
        let mut has_more = false;
        let mut tags_out: Vec<OddsTag> = Vec::new();
        while page < max_pages {
            let url = format!(
                "{}/tags?limit={}&offset={}",
                POLYMARKET_GAMMA_URL, limit, offset
            );
            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| Error::Provider(format!("polymarket tags list failed: {e}")))?;

            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "polymarket tags list failed: http {}",
                    resp.status()
                )));
            }

            let raw: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| Error::Provider(format!("polymarket tags list parse failed: {e}")))?;

            let tags_value = if raw.is_array() {
                raw
            } else if let Some(data) = raw.get("data") {
                data.clone()
            } else if let Some(tags) = raw.get("tags") {
                tags.clone()
            } else if let Some(error) = raw.get("error").or_else(|| raw.get("message")) {
                return Err(Error::Provider(format!(
                    "polymarket tags list error: {}",
                    error
                )));
            } else {
                return Err(Error::Provider(
                    "polymarket tags list unexpected response".to_string(),
                ));
            };

            let page_tags: Vec<PolyTag> = serde_json::from_value(tags_value)
                .map_err(|e| Error::Provider(format!("polymarket tags list parse failed: {e}")))?;
            let raw_len = page_tags.len();
            tags_out.extend(page_tags.into_iter().map(|t| OddsTag {
                id: json_value_to_string(t.id),
                label: t.label,
                slug: t.slug,
            }));

            page += 1;
            if raw_len < limit {
                has_more = false;
                break;
            }
            offset = offset.saturating_add(limit);
            has_more = true;
        }

        let generated_at = Utc::now();
        return Ok(OddsResponse {
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            generated_at,
            schema_version: "finance.odds.v2".to_string(),
            freshness_summary: odds_response_freshness_summary(generated_at, &[], None),
            applied_policy: AppliedPolicy::default(),
            decision_trace: vec![],
            run_meta: odds_run_meta(0, 0, 0, 0),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor: if has_more {
                Some(offset.to_string())
            } else {
                None
            },
            available_series: None,
            available_events: None,
            available_markets: None,
            available_tags: Some(tags_out),
            analytics: None,
            sources: None,
            field_semantics: default_odds_field_semantics(),
        });
    }

    if search_filter.is_some()
        && (req.list_events || req.list_markets)
        && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
    {
        let search_raw = req.search.as_deref().unwrap_or("").trim();
        let limit = req.limit.unwrap_or(100).max(1);
        let max_pages = match req.max_pages {
            Some(n) => n.max(1),
            None => {
                let target = 500usize;
                (target + limit - 1) / limit
            }
        };
        let mut page = req
            .cursor
            .as_deref()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1);
        if page < 1 {
            page = 1;
        }
        let status = req
            .status
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "all".to_string());

        let mut listed_events: Vec<OddsListedEvent> = Vec::new();
        let mut listed_markets: Vec<OddsListedMarket> = Vec::new();
        let mut pages_done = 0usize;
        let mut has_more = true;
        let mut next_cursor: Option<String> = None;

        while pages_done < max_pages && has_more {
            let query: Vec<(&str, String)> = vec![
                ("q", search_raw.to_string()),
                ("limit_per_type", limit.to_string()),
                ("page", page.to_string()),
                ("events_status", status.clone()),
                ("search_tags", "false".to_string()),
                ("search_profiles", "false".to_string()),
                // `optimized=true` omits market IDs, which breaks downstream drill-down (`--market`).
                ("optimized", "false".to_string()),
            ];

            let url = format!("{}/public-search", POLYMARKET_GAMMA_URL);
            let resp =
                client.get(&url).query(&query).send().await.map_err(|e| {
                    Error::Provider(format!("polymarket public-search failed: {e}"))
                })?;

            if !resp.status().is_success() {
                return Err(Error::Provider(format!(
                    "polymarket public-search failed: http {}",
                    resp.status()
                )));
            }

            let body: PolySearchResp = resp.json().await.map_err(|e| {
                Error::Provider(format!("polymarket public-search parse failed: {e}"))
            })?;

            let events = body.events.unwrap_or_default();
            for event in events {
                let event_id = event
                    .ticker
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| json_value_to_string(event.id.clone()));
                let title = event.title.clone().unwrap_or_else(|| event_id.clone());
                let tags = event.tags.clone().map(|t| {
                    t.into_iter()
                        .map(|tag| OddsTag {
                            id: json_value_to_string(tag.id),
                            label: tag.label,
                            slug: tag.slug,
                        })
                        .collect::<Vec<_>>()
                });

                if req.list_events {
                    listed_events.push(OddsListedEvent {
                        ticker: event_id.clone(),
                        title: title.clone(),
                        category: event.category.clone(),
                        series_ticker: None,
                        source: Some("polymarket".to_string()),
                        event_id: Some(event_id.clone()),
                        slug: event.slug.clone(),
                        tags: tags.clone(),
                    });
                }

                let status = match (event.active, event.closed) {
                    (Some(true), Some(false)) => Some("open".to_string()),
                    (Some(false), Some(true)) => Some("closed".to_string()),
                    _ => None,
                };

                if req.list_markets {
                    if let Some(markets) = event.markets {
                        for m in markets {
                            let market_id = json_value_to_string(m.id);
                            let title = m.question.unwrap_or_else(|| market_id.clone());
                            let outcomes_vec = parse_json_value_strings(&m.outcomes);
                            let outcome_prices_vec = parse_json_value_strings(&m.outcome_prices);
                            let outcomes = if outcomes_vec.is_empty() {
                                None
                            } else {
                                Some(outcomes_vec)
                            };
                            let outcome_prices = if outcome_prices_vec.is_empty() {
                                None
                            } else {
                                Some(outcome_prices_vec)
                            };
                            let clob_token_ids_vec = parse_json_value_strings(&m.clob_token_ids);
                            let clob_token_ids = if clob_token_ids_vec.is_empty() {
                                None
                            } else {
                                Some(clob_token_ids_vec)
                            };
                            let probability_yes = match (outcomes.as_ref(), outcome_prices.as_ref())
                            {
                                (Some(o), Some(p)) => probability_yes_from_outcomes(o, p),
                                _ => None,
                            };

                            listed_markets.push(OddsListedMarket {
                                ticker: market_id.clone(),
                                title: title.clone(),
                                event_ticker: event_id.clone(),
                                freshness: odds_freshness(None),
                                yes_price: None,
                                volume: poly_volume_cents(m.volume_num, &m.volume),
                                status: status.clone(),
                                source: Some("polymarket".to_string()),
                                market_id: Some(market_id.clone()),
                                event_id: Some(event_id.clone()),
                                slug: m.slug.clone(),
                                outcomes: outcomes.clone(),
                                outcome_prices: outcome_prices.clone(),
                                clob_token_ids: clob_token_ids.clone(),
                                probability_yes,
                                category: None,
                            });
                        }
                    }
                }
            }

            has_more = body.pagination.and_then(|p| p.has_more).unwrap_or(false);
            pages_done += 1;
            if has_more {
                page += 1;
                next_cursor = Some(page.to_string());
            } else {
                next_cursor = None;
            }
        }

        let analytics = req
            .list_markets
            .then(|| build_odds_analytics_from_listed(&listed_markets))
            .flatten();
        let generated_at = Utc::now();
        return Ok(OddsResponse {
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            generated_at,
            schema_version: "finance.odds.v2".to_string(),
            freshness_summary: odds_response_freshness_summary(
                generated_at,
                &[],
                req.list_markets.then_some(listed_markets.as_slice()),
            ),
            applied_policy: AppliedPolicy::default(),
            decision_trace: vec![],
            run_meta: odds_run_meta(
                if req.list_events { listed_events.len() } else { 0 },
                0,
                if req.list_events { listed_events.len() } else { 0 },
                if req.list_markets { listed_markets.len() } else { 0 },
            ),
            series: None,
            events: vec![],
            markets: vec![],
            orderbook: None,
            cursor: next_cursor,
            available_series: None,
            available_events: if req.list_events {
                Some(listed_events)
            } else {
                None
            },
            available_markets: if req.list_markets {
                Some(listed_markets)
            } else {
                None
            },
            available_tags: None,
            analytics,
            sources: None,
            field_semantics: default_odds_field_semantics(),
        });
    }

    // Direct Polymarket event lookup by ID or slug — avoids scanning all events.
    if let Some(ref event_raw) = req.event_ticker {
        let event_trimmed = event_raw.trim();
        if !event_trimmed.is_empty() {
            let query_param = if event_trimmed.chars().all(|c| c.is_ascii_digit()) {
                ("id", event_trimmed.to_string())
            } else {
                ("slug", event_trimmed.to_string())
            };
            let url = format!("{}/events", POLYMARKET_GAMMA_URL);
            let resp = client
                .get(&url)
                .query(&[query_param])
                .send()
                .await
                .map_err(|e| Error::Provider(format!("polymarket event lookup failed: {e}")))?;

            if resp.status().is_success() {
                let raw: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| Error::Provider(format!("polymarket event lookup parse: {e}")))?;

                let events_arr = if raw.is_array() {
                    raw
                } else {
                    serde_json::json!([raw])
                };

                let mut odds_events: Vec<OddsEvent> = Vec::new();
                let mut odds_markets: Vec<OddsMarket> = Vec::new();

                if let Some(events_list) = events_arr.as_array() {
                    for ev in events_list {
                        let event_id = ev
                            .get("id")
                            .map(|v| json_value_to_string(v.clone()))
                            .unwrap_or_default();
                        let title = ev
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let slug = ev.get("slug").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let category = ev.get("category").and_then(|v| v.as_str()).map(|s| s.to_string());

                        odds_events.push(OddsEvent {
                            ticker: event_id.clone(),
                            title: title.clone(),
                            category: category.clone(),
                            source: Some("polymarket".to_string()),
                            event_id: Some(event_id.clone()),
                            slug: slug.clone(),
                            tags: None,
                        });

                        if let Some(markets) = ev.get("markets").and_then(|v| v.as_array()) {
                            for m in markets {
                                let market_id = m
                                    .get("id")
                                    .map(|v| json_value_to_string(v.clone()))
                                    .unwrap_or_default();
                                let question = m
                                    .get("question")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&title)
                                    .to_string();
                                let outcomes_raw =
                                    m.get("outcomes").cloned().unwrap_or(serde_json::Value::Null);
                                let outcome_prices_raw = m
                                    .get("outcomePrices")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                let outcomes_vec = parse_json_value_strings(&outcomes_raw);
                                let outcome_prices_vec =
                                    parse_json_value_strings(&outcome_prices_raw);
                                let probability_yes = probability_yes_from_outcomes(
                                    &outcomes_vec,
                                    &outcome_prices_vec,
                                );
                                let volume = m
                                    .get("volume")
                                    .or_else(|| m.get("volumeNum"))
                                    .and_then(|v| {
                                        v.as_f64()
                                            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
                                    })
                                    .map(|v| (v * 100.0) as i64);
                                let clob_token_ids_raw = m
                                    .get("clobTokenIds")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                let clob_token_ids_vec =
                                    parse_json_value_strings(&clob_token_ids_raw);

                                odds_markets.push(OddsMarket {
                                    ticker: market_id.clone(),
                                    title: question,
                                    event_ticker: event_id.clone(),
                                    freshness: odds_freshness(None),
                                    status: match (
                                        m.get("active").and_then(|v| v.as_bool()),
                                        m.get("closed").and_then(|v| v.as_bool()),
                                    ) {
                                        (Some(true), Some(false)) => Some("open".to_string()),
                                        (Some(false), Some(true)) => Some("closed".to_string()),
                                        _ => None,
                                    },
                                    yes_price: probability_yes.map(|p| (p * 100.0) as i64),
                                    yes_bid: None,
                                    yes_ask: None,
                                    volume,
                                    source: Some("polymarket".to_string()),
                                    market_id: Some(market_id),
                                    event_id: Some(event_id.clone()),
                                    slug: slug.clone(),
                                    outcomes: if outcomes_vec.is_empty() {
                                        None
                                    } else {
                                        Some(outcomes_vec)
                                    },
                                    outcome_prices: if outcome_prices_vec.is_empty() {
                                        None
                                    } else {
                                        Some(outcome_prices_vec)
                                    },
                                    clob_token_ids: if clob_token_ids_vec.is_empty() {
                                        None
                                    } else {
                                        Some(clob_token_ids_vec)
                                    },
                                    probability_yes,
                                    outcome_best_bids: None,
                                    outcome_best_asks: None,
                                    orderbook_timestamp: None,
                                });
                            }
                        }
                    }
                }

                let active_count = odds_markets
                    .iter()
                    .filter(|m| m.status.as_deref() == Some("open"))
                    .count();
                let total_vol: i64 = odds_markets.iter().filter_map(|m| m.volume).sum();
                let avg_prob = if odds_markets.is_empty() {
                    None
                } else {
                    Some(
                        odds_markets
                            .iter()
                            .filter_map(|m| m.probability_yes)
                            .sum::<f64>()
                            / odds_markets.len() as f64,
                    )
                };

                let generated_at = Utc::now();
                return Ok(OddsResponse {
                    base_url: POLYMARKET_GAMMA_URL.to_string(),
                    generated_at,
                    schema_version: "finance.odds.v2".to_string(),
                    freshness_summary: odds_response_freshness_summary(
                        generated_at,
                        &odds_markets,
                        None,
                    ),
                    applied_policy: AppliedPolicy::default(),
                    decision_trace: vec![],
                    run_meta: odds_run_meta(odds_events.len(), odds_markets.len(), 0, 0),
                    series: None,
                    events: odds_events,
                    markets: odds_markets.clone(),
                    orderbook: None,
                    cursor: None,
                    available_series: None,
                    available_events: None,
                    available_markets: None,
                    available_tags: None,
                    analytics: Some(OddsAnalytics {
                        markets_total: odds_markets.len(),
                        open_markets: active_count,
                        active_markets: active_count,
                        initialized_markets: 0,
                        markets_with_volume: odds_markets
                            .iter()
                            .filter(|m| m.volume.unwrap_or(0) > 0)
                            .count(),
                        total_volume: Some(total_vol),
                        average_probability_yes: avg_prob,
                        average_spread_cents: None,
                    }),
                    sources: None,
                    field_semantics: default_odds_field_semantics(),
                });
            }
        }
    }

    let limit = req.limit.unwrap_or(100).max(1);
    let max_pages = match req.max_pages {
        Some(n) => n.max(1),
        None => {
            let has_search = search_filter.is_some();
            if has_search {
                let target = 500usize;
                (target + limit - 1) / limit
            } else {
                1
            }
        }
    };
    let mut offset = req
        .cursor
        .as_deref()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let event_filter = req
        .event_ticker
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

    let mut events: Vec<PolyEvent> = Vec::new();
    let mut page = 0usize;
    while page < max_pages {
        let url = format!(
            "{}/events?active=true&closed=false&limit={}&offset={}",
            POLYMARKET_GAMMA_URL, limit, offset
        );
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("polymarket events list failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "polymarket events list failed: http {}",
                resp.status()
            )));
        }

        let raw: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("polymarket events list parse failed: {e}")))?;

        let events_value = if raw.is_array() {
            raw
        } else if let Some(data) = raw.get("data") {
            data.clone()
        } else if let Some(events_json) = raw.get("events") {
            events_json.clone()
        } else if let Some(error) = raw.get("error").or_else(|| raw.get("message")) {
            return Err(Error::Provider(format!(
                "polymarket events list error: {}",
                error
            )));
        } else {
            return Err(Error::Provider(
                "polymarket events list unexpected response".to_string(),
            ));
        };

        let mut page_events: Vec<PolyEvent> = serde_json::from_value(events_value)
            .map_err(|e| Error::Provider(format!("polymarket events list parse failed: {e}")))?;
        let raw_len = page_events.len();

        if let Some(ref search) = search_filter {
            page_events = page_events
                .into_iter()
                .filter(|e| {
                    let title = e.title.as_deref().unwrap_or("").to_lowercase();
                    let slug = e.slug.as_deref().unwrap_or("").to_lowercase();
                    title.contains(search) || slug.contains(search)
                })
                .collect();
        }

        if let Some(ref filter) = event_filter {
            page_events = page_events
                .into_iter()
                .filter(|e| {
                    let id = json_value_to_string(e.id.clone()).to_lowercase();
                    let slug = e.slug.as_deref().unwrap_or("").to_lowercase();
                    id == *filter || slug == *filter
                })
                .collect();
        }

        events.extend(page_events);

        page += 1;
        if raw_len < limit {
            break;
        }
        offset = offset.saturating_add(limit);
    }

    let mut odds_events: Vec<OddsEvent> = Vec::new();
    let mut odds_markets: Vec<OddsMarket> = Vec::new();
    let mut listed_events: Vec<OddsListedEvent> = Vec::new();
    let mut listed_markets: Vec<OddsListedMarket> = Vec::new();

    let list_events_only = req.list_events;
    let list_markets_only = req.list_markets;

    for event in events {
        let event_id = json_value_to_string(event.id);
        let title = event.title.unwrap_or_else(|| event_id.clone());
        let slug = event.slug.clone();
        let tags = event.tags.map(|t| {
            t.into_iter()
                .map(|tag| OddsTag {
                    id: json_value_to_string(tag.id),
                    label: tag.label,
                    slug: tag.slug,
                })
                .collect::<Vec<_>>()
        });

        if list_events_only {
            listed_events.push(OddsListedEvent {
                ticker: event_id.clone(),
                title: title.clone(),
                category: None,
                series_ticker: None,
                source: Some("polymarket".to_string()),
                event_id: Some(event_id.clone()),
                slug: slug.clone(),
                tags: tags.clone(),
            });
        } else {
            odds_events.push(OddsEvent {
                ticker: event_id.clone(),
                title: title.clone(),
                category: None,
                source: Some("polymarket".to_string()),
                event_id: Some(event_id.clone()),
                slug: slug.clone(),
                tags: tags.clone(),
            });
        }

        let status = match (event.active, event.closed) {
            (Some(true), Some(false)) => Some("open".to_string()),
            (Some(false), Some(true)) => Some("closed".to_string()),
            _ => None,
        };

        if let Some(markets) = event.markets {
            for m in markets {
                let market_volume = poly_volume_cents(m.volume_num, &m.volume);
                let market_id = json_value_to_string(m.id);
                let title = m.question.unwrap_or_else(|| market_id.clone());
                let outcomes_vec = parse_json_value_strings(&m.outcomes);
                let outcome_prices_vec = parse_json_value_strings(&m.outcome_prices);
                let outcomes = if outcomes_vec.is_empty() {
                    None
                } else {
                    Some(outcomes_vec)
                };
                let outcome_prices = if outcome_prices_vec.is_empty() {
                    None
                } else {
                    Some(outcome_prices_vec)
                };
                let clob_token_ids_vec = parse_json_value_strings(&m.clob_token_ids);
                let clob_token_ids = if clob_token_ids_vec.is_empty() {
                    None
                } else {
                    Some(clob_token_ids_vec)
                };
                let probability_yes = match (outcomes.as_ref(), outcome_prices.as_ref()) {
                    (Some(o), Some(p)) => probability_yes_from_outcomes(o, p),
                    _ => None,
                };

                if list_markets_only {
                    listed_markets.push(OddsListedMarket {
                        ticker: market_id.clone(),
                        title: title.clone(),
                        event_ticker: event_id.clone(),
                        freshness: odds_freshness(None),
                        yes_price: None,
                        volume: market_volume,
                        status: status.clone(),
                        source: Some("polymarket".to_string()),
                        market_id: Some(market_id.clone()),
                        event_id: Some(event_id.clone()),
                        slug: None,
                        outcomes: outcomes.clone(),
                        outcome_prices: outcome_prices.clone(),
                        clob_token_ids: clob_token_ids.clone(),
                        probability_yes,
                        category: None,
                    });
                } else {
                    odds_markets.push(OddsMarket {
                        ticker: market_id.clone(),
                        title: title.clone(),
                        event_ticker: event_id.clone(),
                        freshness: odds_freshness(None),
                        status: status.clone(),
                        yes_price: None,
                        yes_bid: None,
                        yes_ask: None,
                        volume: market_volume,
                        source: Some("polymarket".to_string()),
                        market_id: Some(market_id.clone()),
                        event_id: Some(event_id.clone()),
                        slug: None,
                        outcomes: outcomes.clone(),
                        outcome_prices: outcome_prices.clone(),
                        clob_token_ids: clob_token_ids.clone(),
                        probability_yes,
                        outcome_best_bids: None,
                        outcome_best_asks: None,
                        orderbook_timestamp: None,
                    });
                }
            }
        }
    }

    if let Some(ref market_filter) = req.market_ticker {
        let filter = market_filter.trim().to_lowercase();
        if !filter.is_empty() {
            odds_markets.retain(|m| {
                let id = m.market_id.as_deref().unwrap_or(&m.ticker).to_lowercase();
                id == filter
            });
            listed_markets.retain(|m| {
                let id = m.market_id.as_deref().unwrap_or(&m.ticker).to_lowercase();
                id == filter
            });
        }
    }

    if req.include_orderbook {
        let mut token_ids = Vec::new();
        for market in &odds_markets {
            if let Some(tokens) = market.clob_token_ids.as_ref() {
                for token in tokens {
                    if !token_ids.contains(token) {
                        token_ids.push(token.clone());
                    }
                }
            }
        }

        if !token_ids.is_empty() {
            let books = fetch_polymarket_books_ws(&token_ids, 3000).await?;
            for market in &mut odds_markets {
                if let Some(tokens) = market.clob_token_ids.as_ref() {
                    let mut bids = Vec::new();
                    let mut asks = Vec::new();
                    let mut timestamp = None;
                    for token in tokens {
                        if let Some(book) = books.get(token) {
                            bids.push(book.best_bid.clone().unwrap_or_default());
                            asks.push(book.best_ask.clone().unwrap_or_default());
                            if timestamp.is_none() {
                                timestamp = book.timestamp.clone();
                            }
                        } else {
                            bids.push(String::new());
                            asks.push(String::new());
                        }
                    }
                    market.outcome_best_bids = Some(bids.clone());
                    market.outcome_best_asks = Some(asks.clone());
                    market.orderbook_timestamp = timestamp;

                    if let (Some(outcomes), Some(bids), Some(asks)) = (
                        market.outcomes.as_ref(),
                        market.outcome_best_bids.as_ref(),
                        market.outcome_best_asks.as_ref(),
                    ) {
                        if let Some(idx) = outcomes
                            .iter()
                            .position(|o| o.trim().eq_ignore_ascii_case("yes"))
                        {
                            let bid = bids.get(idx).and_then(|v| parse_probability(v));
                            let ask = asks.get(idx).and_then(|v| parse_probability(v));
                            if let (Some(b), Some(a)) = (bid, ask) {
                                market.probability_yes = Some((b + a) / 2.0);
                            } else if let Some(b) = bid {
                                market.probability_yes = Some(b);
                            } else if let Some(a) = ask {
                                market.probability_yes = Some(a);
                            }
                        }
                    }
                }
            }
        }
    }

    let analytics = build_odds_analytics(&odds_markets);
    let generated_at = Utc::now();
    Ok(OddsResponse {
        base_url: POLYMARKET_GAMMA_URL.to_string(),
        generated_at,
        schema_version: "finance.odds.v2".to_string(),
        freshness_summary: odds_response_freshness_summary(
            generated_at,
            &odds_markets,
            list_markets_only.then_some(listed_markets.as_slice()),
        ),
        applied_policy: AppliedPolicy::default(),
        decision_trace: vec![],
        run_meta: odds_run_meta(
            odds_events.len(),
            odds_markets.len(),
            if list_events_only { listed_events.len() } else { 0 },
            if list_markets_only { listed_markets.len() } else { 0 },
        ),
        series: None,
        events: odds_events,
        markets: odds_markets,
        orderbook: None,
        cursor: None,
        available_series: None,
        available_events: if list_events_only {
            Some(listed_events)
        } else {
            None
        },
        available_markets: if list_markets_only {
            Some(listed_markets)
        } else {
            None
        },
        available_tags: None,
        analytics,
        sources: None,
        field_semantics: default_odds_field_semantics(),
    })
}
