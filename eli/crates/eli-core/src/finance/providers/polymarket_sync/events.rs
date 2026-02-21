/// Sync events from Polymarket with rate limiting.
pub(crate) async fn sync_polymarket_events(
    limiter: &RateLimiter,
    max_pages: usize,
) -> std::result::Result<
    (
        Vec<OddsListedEvent>,
        Vec<OddsListedMarket>,
        OddsSyncCoverage,
    ),
    String,
> {
    eprintln!("[polymarket] starting sync (max_pages={})", max_pages);
    let mut all_markets = Vec::new();
    let mut offset = 0usize;
    let mut events_pages_fetched = 0usize;
    let mut events_exhausted = false;
    let mut markets_pages_fetched = 0usize;
    let mut markets_exhausted = false;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(30))
        .build()
        .map_err(|e| format!("Polymarket client init failed: {e}"))?;

    #[derive(Deserialize)]
    struct PolyMarketRowTag {
        id: serde_json::Value,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        slug: Option<String>,
    }

    #[derive(Deserialize)]
    struct PolyMarketRowEvent {
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
        tags: Option<Vec<PolyMarketRowTag>>,
    }

    #[derive(Deserialize)]
    struct PolyMarketRow {
        id: serde_json::Value,
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        active: Option<bool>,
        #[serde(default)]
        closed: Option<bool>,
        #[serde(default)]
        volume: Option<String>,
        #[serde(default, rename = "volumeNum")]
        volume_num: Option<f64>,
        #[serde(default)]
        outcomes: serde_json::Value,
        #[serde(default, rename = "outcomePrices")]
        outcome_prices: serde_json::Value,
        #[serde(default, rename = "clobTokenIds")]
        clob_token_ids: serde_json::Value,
        #[serde(default)]
        events: Option<Vec<PolyMarketRowEvent>>,
    }

    #[derive(Clone)]
    struct PolyEventMetadata {
        title: Option<String>,
        slug: Option<String>,
        category: Option<String>,
        tags: Option<Vec<OddsTag>>,
    }

    #[derive(Deserialize)]
    struct PolyEventMetaRow {
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
        tags: Option<Vec<PolyMarketRowTag>>,
    }

    let mut events_by_id: HashMap<String, OddsListedEvent> = HashMap::new();
    let mut event_meta_by_id: HashMap<String, PolyEventMetadata> = HashMap::new();
    let limit = 500usize;

    let normalize_topic = |raw: &str| -> Option<String> {
        let s = raw.trim();
        if s.is_empty() {
            return None;
        }
        let humanized = s
            .replace(['-', '_', '/'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if humanized.is_empty() {
            None
        } else {
            Some(humanized)
        }
    };

    let derive_event_category = |event: &PolyMarketRowEvent| -> Option<String> {
        if let Some(cat) = event.category.as_deref().and_then(normalize_topic) {
            return Some(cat);
        }
        event
            .tags
            .as_ref()
            .and_then(|tags| {
                tags.iter()
                    .find_map(|t| t.label.as_deref().and_then(normalize_topic))
            })
            .or_else(|| {
                event.tags.as_ref().and_then(|tags| {
                    tags.iter()
                        .find_map(|t| t.slug.as_deref().and_then(normalize_topic))
                })
            })
    };

    {
        let mut event_offset = 0usize;
        let event_pages = max_pages;
        for _ in 0..event_pages {
            let mut attempts = 0usize;
            let rows: Vec<PolyEventMetaRow> = loop {
                limiter.wait().await;
                let mut url = reqwest::Url::parse(&format!("{}/events", POLYMARKET_GAMMA_URL))
                    .map_err(|e| format!("Polymarket events url parse failed: {e}"))?;
                url.query_pairs_mut()
                    .append_pair("active", "true")
                    .append_pair("closed", "false")
                    .append_pair("limit", &limit.to_string())
                    .append_pair("offset", &event_offset.to_string());

                let resp = client
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| format!("Polymarket events metadata fetch failed: {e}"))?;

                let status = resp.status();
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts < 5 {
                    attempts += 1;
                    limiter.on_rate_limited();
                    let cooldown_secs = 5u64.saturating_mul(1u64 << ((attempts - 1) as u32));
                    sleep(TokioDuration::from_secs(cooldown_secs.min(60))).await;
                    continue;
                }

                if !status.is_success() {
                    return Err(format!(
                        "Polymarket events metadata fetch failed: http {}",
                        status
                    ));
                }

                let raw: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| format!("Polymarket events metadata parse failed: {e}"))?;
                let rows_value = if raw.is_array() {
                    raw
                } else if let Some(data) = raw.get("data") {
                    data.clone()
                } else if let Some(events) = raw.get("events") {
                    events.clone()
                } else {
                    return Err(
                        "Polymarket events metadata response missing data/events".to_string()
                    );
                };
                let rows: Vec<PolyEventMetaRow> = serde_json::from_value(rows_value)
                    .map_err(|e| format!("Polymarket events metadata decode failed: {e}"))?;
                limiter.on_success();
                break rows;
            };

            let count = rows.len();
            events_pages_fetched = events_pages_fetched.saturating_add(1);
            if count == 0 {
                events_exhausted = true;
                break;
            }

            for row in rows {
                let event_id = json_value_to_string(row.id.clone());
                if event_id.is_empty() {
                    continue;
                }
                let event_ticker_alias = row.ticker.clone().filter(|s| !s.trim().is_empty());

                let tags = row.tags.as_ref().map(|tags| {
                    tags.iter()
                        .map(|t| OddsTag {
                            id: json_value_to_string(t.id.clone()),
                            label: t.label.clone(),
                            slug: t.slug.clone(),
                        })
                        .collect::<Vec<_>>()
                });
                let category = row
                    .category
                    .as_deref()
                    .and_then(normalize_topic)
                    .or_else(|| {
                        row.tags.as_ref().and_then(|tags| {
                            tags.iter()
                                .find_map(|t| t.label.as_deref().and_then(normalize_topic))
                        })
                    })
                    .or_else(|| {
                        row.tags.as_ref().and_then(|tags| {
                            tags.iter()
                                .find_map(|t| t.slug.as_deref().and_then(normalize_topic))
                        })
                    });
                let slug = row
                    .slug
                    .or_else(|| row.ticker.clone())
                    .filter(|s| !s.trim().is_empty());
                let metadata = PolyEventMetadata {
                    title: row.title.filter(|s| !s.trim().is_empty()),
                    slug,
                    category,
                    tags,
                };

                event_meta_by_id.insert(event_id.clone(), metadata.clone());
                if let Some(alias) = event_ticker_alias {
                    event_meta_by_id.insert(alias, metadata);
                }
            }

            event_offset = event_offset.saturating_add(count);
            if count < limit {
                events_exhausted = true;
                break;
            }
        }
    }

    // Respect user-requested pagination budget for Polymarket.
    let market_pages = max_pages;
    for page in 0..market_pages {
        let mut attempts = 0usize;
        let rows: Vec<PolyMarketRow> = loop {
            limiter.wait().await;

            let mut url = reqwest::Url::parse(&format!("{}/markets", POLYMARKET_GAMMA_URL))
                .map_err(|e| format!("Polymarket url parse failed: {e}"))?;
            url.query_pairs_mut()
                .append_pair("active", "true")
                .append_pair("closed", "false")
                .append_pair("limit", &limit.to_string())
                .append_pair("offset", &offset.to_string());
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("Polymarket bulk markets fetch failed: {e}"))?;

            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts < 5 {
                attempts += 1;
                limiter.on_rate_limited();
                let cooldown_secs = 5u64.saturating_mul(1u64 << ((attempts - 1) as u32));
                sleep(TokioDuration::from_secs(cooldown_secs.min(60))).await;
                continue;
            }

            if !status.is_success() {
                return Err(format!(
                    "Polymarket bulk markets fetch failed: http {}",
                    status
                ));
            }

            let rows = resp
                .json::<Vec<PolyMarketRow>>()
                .await
                .map_err(|e| format!("Polymarket bulk markets parse failed: {e}"))?;
            limiter.on_success();
            break rows;
        };

        let count = rows.len();
        markets_pages_fetched = markets_pages_fetched.saturating_add(1);
        eprintln!(
            "[polymarket] markets page {}: {} markets (total: {})",
            page + 1,
            count,
            all_markets.len() + count
        );
        info!("Polymarket page {}: fetched {} markets", page + 1, count);
        if count == 0 {
            markets_exhausted = true;
            break;
        }

        for m in rows {
            let market_id = json_value_to_string(m.id);
            let title = m.question.unwrap_or_else(|| market_id.clone());

            let (event_id, mut event_title, mut event_slug, mut event_category, mut event_tags) = m
                .events
                .as_ref()
                .and_then(|evs| evs.first())
                .map(|e| {
                    let id = json_value_to_string(e.id.clone());
                    let title = e.title.clone().unwrap_or_else(|| id.clone());
                    let slug = e
                        .slug
                        .clone()
                        .or_else(|| e.ticker.clone())
                        .filter(|s| !s.trim().is_empty());
                    let category = derive_event_category(e);
                    let tags = e.tags.as_ref().map(|tags| {
                        tags.iter()
                            .map(|t| OddsTag {
                                id: json_value_to_string(t.id.clone()),
                                label: t.label.clone(),
                                slug: t.slug.clone(),
                            })
                            .collect::<Vec<_>>()
                    });
                    (id, title, slug, category, tags)
                })
                .unwrap_or_else(|| (String::new(), String::new(), None, None, None));

            if !event_id.is_empty() {
                if let Some(meta) = event_meta_by_id.get(&event_id) {
                    if event_title.trim().is_empty() {
                        event_title = meta.title.clone().unwrap_or_else(|| event_id.clone());
                    }
                    if event_slug.is_none() {
                        event_slug = meta.slug.clone();
                    }
                    if event_category.is_none() {
                        event_category = meta.category.clone();
                    }
                    if event_tags.is_none() {
                        event_tags = meta.tags.clone();
                    }
                }
            }

            if !event_id.is_empty() {
                if let Some(existing) = events_by_id.get_mut(&event_id) {
                    if existing.category.is_none() {
                        existing.category = event_category.clone();
                    }
                    if existing.tags.is_none() {
                        existing.tags = event_tags.clone();
                    }
                    if existing.slug.is_none() {
                        existing.slug = event_slug.clone();
                    }
                    if existing.title.trim().is_empty() && !event_title.trim().is_empty() {
                        existing.title = event_title.clone();
                    }
                } else {
                    events_by_id.insert(
                        event_id.clone(),
                        OddsListedEvent {
                            ticker: event_id.clone(),
                            title: if event_title.is_empty() {
                                event_id.clone()
                            } else {
                                event_title.clone()
                            },
                            category: event_category.clone(),
                            series_ticker: None,
                            source: Some("polymarket".to_string()),
                            event_id: Some(event_id.clone()),
                            slug: event_slug.clone(),
                            tags: event_tags.clone(),
                        },
                    );
                }
            }

            let market_category = if !event_id.is_empty() {
                events_by_id
                    .get(&event_id)
                    .and_then(|e| e.category.clone())
                    .or_else(|| event_category.clone())
            } else {
                event_category.clone()
            };

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
            let probability_yes = match (outcomes.as_ref(), outcome_prices.as_ref()) {
                (Some(o), Some(p)) => probability_yes_from_outcomes(o, p),
                _ => None,
            };

            let yes_price = probability_yes.map(|p| (p * 100.0).round() as i64);
            let volume = m.volume_num.map(|v| v.round() as i64).or_else(|| {
                m.volume
                    .as_deref()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|v| v.round() as i64)
            });
            let status = match (m.active, m.closed) {
                (Some(true), Some(false)) => Some("open".to_string()),
                (Some(false), Some(true)) => Some("closed".to_string()),
                _ => None,
            };

            let clob_ids_vec = parse_json_value_strings(&m.clob_token_ids);
            let clob_token_ids = if clob_ids_vec.is_empty() {
                None
            } else {
                Some(clob_ids_vec)
            };

            all_markets.push(OddsListedMarket {
                ticker: market_id.clone(),
                title,
                event_ticker: event_id.clone(),
                yes_price,
                volume,
                status,
                source: Some("polymarket".to_string()),
                market_id: Some(market_id.clone()),
                event_id: if event_id.is_empty() {
                    None
                } else {
                    Some(event_id)
                },
                slug: m.slug,
                outcomes,
                outcome_prices,
                clob_token_ids,
                probability_yes,
                category: market_category,
            });
        }

        offset = offset.saturating_add(count);
        if count < limit {
            markets_exhausted = true;
            break;
        }
    }

    let mut events: Vec<OddsListedEvent> = events_by_id.into_values().collect();
    events.sort_by(|a, b| a.ticker.cmp(&b.ticker));

    // Drop sports markets — they are noise for financial analysis.
    let pre_filter_markets = all_markets.len();
    let pre_filter_events = events.len();
    let is_sports = |cat: &str| -> bool {
        let c = cat.to_lowercase();
        c.contains("sport")
            || c == "nba"
            || c == "nfl"
            || c == "mlb"
            || c == "nhl"
            || c == "soccer"
            || c == "football"
            || c == "basketball"
            || c == "baseball"
            || c == "hockey"
            || c == "mma"
            || c == "ufc"
            || c == "tennis"
            || c == "golf"
            || c == "cricket"
            || c == "formula"
    };
    all_markets.retain(|m| !m.category.as_deref().map_or(false, is_sports));
    events.retain(|e| !e.category.as_deref().map_or(false, is_sports));
    let sports_markets_dropped = pre_filter_markets - all_markets.len();
    let sports_events_dropped = pre_filter_events - events.len();
    if sports_markets_dropped > 0 || sports_events_dropped > 0 {
        eprintln!(
            "[polymarket] dropped {} sports markets, {} sports events",
            sports_markets_dropped, sports_events_dropped
        );
    }

    let mut strict_fail_reasons = Vec::new();
    if !events_exhausted {
        strict_fail_reasons.push(format!(
            "events pagination not exhausted within max_pages={max_pages}"
        ));
    }
    if !markets_exhausted {
        strict_fail_reasons.push(format!(
            "markets pagination not exhausted within max_pages={max_pages}"
        ));
    }
    let coverage = OddsSyncCoverage {
        requested_max_pages: max_pages,
        events_pages_fetched,
        events_exhausted,
        markets_pages_fetched,
        markets_exhausted,
        series_backfill_calls: None,
        series_backfill_cap: None,
        series_backfill_truncated: None,
        strict_pass: strict_fail_reasons.is_empty(),
        strict_fail_reasons,
    };

    eprintln!(
        "[polymarket] sync complete: {} markets, {} events",
        all_markets.len(),
        events.len()
    );
    Ok((events, all_markets, coverage))
}
