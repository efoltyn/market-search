/// Sync events/markets from Polymarket Gamma API.
///
/// Strategy:
///   1. Fetch events metadata sequentially (offset paging) for category/tag context.
///   2. Fetch markets pages in parallel by offset.
///   3. Merge event metadata into markets and dedupe by stable market id.
///   4. Drop sports rows unless include_sports=true.
pub(crate) async fn sync_polymarket_events(
    limiter: &RateLimiter,
    max_pages: Option<usize>,
    include_sports: bool,
) -> std::result::Result<
    (
        Vec<OddsListedEvent>,
        Vec<OddsListedMarket>,
        OddsSyncCoverage,
    ),
    String,
> {
    eprintln!(
        "[polymarket] starting sync (max_pages={}, include_sports={})",
        max_pages
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unbounded".to_string()),
        include_sports
    );

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(30))
        .build()
        .map_err(|e| format!("Polymarket client init failed: {e}"))?;

    let limit = 500usize;
    let mut events_by_id: HashMap<String, OddsListedEvent> = HashMap::new();
    let mut event_meta_by_id: HashMap<String, PolyEventMetadata> = HashMap::new();

    let mut events_pages_fetched = 0usize;
    let mut events_exhausted = false;
    let mut events_requests = 0usize;
    let mut markets_pages_fetched = 0usize;
    let mut markets_exhausted = false;
    let mut markets_requests = 0usize;
    let mut retry_count_429 = 0usize;
    let mut retry_count_5xx = 0usize;

    let page_cap = max_pages.unwrap_or(usize::MAX);
    let mut page = 0usize;
    while page < page_cap {
        let offset = page.saturating_mul(limit);
        let result = fetch_polymarket_event_page(&client, limiter, limit, offset).await?;
        events_pages_fetched = events_pages_fetched.saturating_add(1);
        events_requests = events_requests.saturating_add(result.attempts);
        retry_count_429 = retry_count_429.saturating_add(result.retry_429);
        retry_count_5xx = retry_count_5xx.saturating_add(result.retry_5xx);

        let count = result.rows.len();
        if page == 0 || (page + 1) % 10 == 0 || count == 0 {
            eprintln!(
                "[polymarket] events page {}: {} rows",
                page + 1,
                count
            );
        }

        if count == 0 {
            events_exhausted = true;
            break;
        }

        for row in result.rows {
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

        if count < limit {
            events_exhausted = true;
            break;
        }

        page = page.saturating_add(1);
    }

    let worker_count = max_pages.unwrap_or(12).clamp(1, 12);
    let mut market_pages: Vec<MarketPageResult> = Vec::new();
    let mut next_market_page = 0usize;
    let mut first_short_page: Option<usize> = None;

    while next_market_page < page_cap {
        if first_short_page.is_some() {
            break;
        }

        let remaining = if page_cap == usize::MAX {
            worker_count
        } else {
            (page_cap - next_market_page).min(worker_count)
        };
        if remaining == 0 {
            break;
        }
        let end_page = next_market_page.saturating_add(remaining);

        let mut batch_results = futures::stream::iter(next_market_page..end_page)
            .map(|page| {
                let client = client.clone();
                async move {
                    let offset = page.saturating_mul(limit);
                    let result =
                        fetch_polymarket_market_page(&client, limiter, page, limit, offset)
                            .await
                            .map_err(|e| format!("markets page {} failed: {e}", page + 1))?;
                    Ok::<MarketPageResult, String>(result)
                }
            })
            .buffer_unordered(worker_count)
            .collect::<Vec<_>>()
            .await;

        let mut batch_pages: Vec<MarketPageResult> = Vec::new();
        for result in batch_results.drain(..) {
            match result {
                Ok(page) => {
                    markets_pages_fetched = markets_pages_fetched.saturating_add(1);
                    markets_requests = markets_requests.saturating_add(page.attempts);
                    retry_count_429 = retry_count_429.saturating_add(page.retry_429);
                    retry_count_5xx = retry_count_5xx.saturating_add(page.retry_5xx);
                    batch_pages.push(page);
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(short_page) = batch_pages
            .iter()
            .filter(|p| p.rows.len() < limit)
            .map(|p| p.page)
            .min()
        {
            first_short_page = Some(short_page);
            markets_exhausted = true;
        }
        market_pages.extend(batch_pages.into_iter());
        next_market_page = end_page;
    }

    market_pages.sort_by_key(|p| p.page);

    let process_page_cap = first_short_page.unwrap_or_else(|| {
        if page_cap == usize::MAX {
            market_pages.last().map(|p| p.page).unwrap_or(0)
        } else {
            page_cap.saturating_sub(1)
        }
    });

    let mut all_markets: Vec<OddsListedMarket> = Vec::new();
    let mut seen_market_ids: HashSet<String> = HashSet::new();

    for page in market_pages.into_iter() {
        if page.page > process_page_cap {
            continue;
        }

        let count = page.rows.len();
        eprintln!(
            "[polymarket] markets page {}: {} rows",
            page.page + 1,
            count
        );

        for m in page.rows {
            let market_id = json_value_to_string(m.id);
            let dedupe_key = if market_id.is_empty() {
                m.slug.clone().unwrap_or_default()
            } else {
                market_id.clone()
            };
            if dedupe_key.is_empty() || !seen_market_ids.insert(dedupe_key) {
                continue;
            }

            let title = m.question.clone().unwrap_or_else(|| market_id.clone());

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

            if !include_sports && event_category.as_deref().map_or(false, is_sports_topic) {
                continue;
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
            }
            .or_else(|| {
                m.last_trade_price
                    .as_ref()
                    .and_then(json_value_to_f64)
                    .map(|p| p.clamp(0.0, 1.0))
            })
            .or_else(|| {
                midpoint_prob(
                    m.best_bid.as_ref().and_then(json_value_to_f64),
                    m.best_ask.as_ref().and_then(json_value_to_f64),
                )
            });

            let yes_price = probability_yes.map(|p| (p * 100.0).round() as i64);
            let volume = m.volume_num.map(|v| v.round() as i64).or_else(|| {
                m.volume
                    .as_ref()
                    .and_then(json_value_to_f64)
                    .map(|v| v.round() as i64)
            });

            all_markets.push(OddsListedMarket {
                ticker: market_id.clone(),
                title,
                event_ticker: event_id.clone(),
                freshness: Freshness::new(
                    Utc::now(),
                    Utc::now(),
                    FreshnessState::Unknown,
                    FreshnessOrigin::TransportReceived,
                    FreshnessQuality::Estimated,
                ),
                yes_price,
                volume,
                status: normalize_polymarket_status(m.active, m.closed),
                source: Some("polymarket".to_string()),
                market_id: Some(market_id),
                event_id: if event_id.is_empty() {
                    None
                } else {
                    Some(event_id)
                },
                slug: m.slug,
                outcomes,
                outcome_prices,
                clob_token_ids: {
                    let ids = parse_json_value_strings(&m.clob_token_ids);
                    if ids.is_empty() {
                        None
                    } else {
                        Some(ids)
                    }
                },
                probability_yes,
                category: market_category,
            });
        }
    }

    let mut events: Vec<OddsListedEvent> = events_by_id.into_values().collect();
    events.sort_by(|a, b| a.ticker.cmp(&b.ticker));
    all_markets.sort_by(|a, b| a.ticker.cmp(&b.ticker));

    if !include_sports {
        let before_events = events.len();
        let before_markets = all_markets.len();
        events.retain(|e| !e.category.as_deref().map_or(false, is_sports_topic));
        all_markets.retain(|m| !m.category.as_deref().map_or(false, is_sports_topic));
        let dropped_events = before_events.saturating_sub(events.len());
        let dropped_markets = before_markets.saturating_sub(all_markets.len());
        if dropped_events > 0 || dropped_markets > 0 {
            eprintln!(
                "[polymarket] dropped {} sports markets, {} sports events",
                dropped_markets, dropped_events
            );
        }
    }

    let mut strict_fail_reasons = Vec::new();
    if !events_exhausted {
        if let Some(cap) = max_pages {
            strict_fail_reasons.push(format!(
                "events pagination not exhausted within max_pages={cap}"
            ));
        } else {
            strict_fail_reasons.push("events pagination not exhausted".to_string());
        }
    }
    if !markets_exhausted {
        if let Some(cap) = max_pages {
            strict_fail_reasons.push(format!(
                "markets pagination not exhausted within max_pages={cap}"
            ));
        } else {
            strict_fail_reasons.push("markets pagination not exhausted".to_string());
        }
    }

    let coverage = OddsSyncCoverage {
        requested_max_pages: max_pages,
        events_pages_fetched,
        events_exhausted,
        markets_pages_fetched,
        markets_exhausted,
        events_requests,
        markets_requests,
        retry_count_429,
        retry_count_5xx,
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
    volume: Option<serde_json::Value>,
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
    #[serde(default, rename = "bestBid")]
    best_bid: Option<serde_json::Value>,
    #[serde(default, rename = "bestAsk")]
    best_ask: Option<serde_json::Value>,
    #[serde(default, rename = "lastTradePrice")]
    last_trade_price: Option<serde_json::Value>,
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

struct EventPageResult {
    rows: Vec<PolyEventMetaRow>,
    attempts: usize,
    retry_429: usize,
    retry_5xx: usize,
}

struct MarketPageResult {
    page: usize,
    rows: Vec<PolyMarketRow>,
    attempts: usize,
    retry_429: usize,
    retry_5xx: usize,
}

async fn fetch_polymarket_event_page(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    limit: usize,
    offset: usize,
) -> std::result::Result<EventPageResult, String> {
    let mut attempts = 0usize;
    let mut retry_429 = 0usize;
    let mut retry_5xx = 0usize;

    loop {
        limiter.wait().await;
        attempts = attempts.saturating_add(1);

        let mut url = reqwest::Url::parse(&format!("{}/events", POLYMARKET_GAMMA_URL))
            .map_err(|e| format!("events url parse failed: {e}"))?;
        url.query_pairs_mut()
            .append_pair("active", "true")
            .append_pair("closed", "false")
            .append_pair("limit", &limit.to_string())
            .append_pair("offset", &offset.to_string());

        let resp = match client.get(url).send().await {
            Ok(resp) => resp,
            Err(err) => {
                if attempts < 6 {
                    retry_5xx = retry_5xx.saturating_add(1);
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempts)).await;
                    continue;
                }
                return Err(format!("events fetch failed: {err}"));
            }
        };
        let status = resp.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts < 6 {
            retry_429 = retry_429.saturating_add(1);
            limiter.on_rate_limited();
            sleep(retry_delay(status, resp.headers(), attempts)).await;
            continue;
        }
        if status.is_server_error() && attempts < 6 {
            retry_5xx = retry_5xx.saturating_add(1);
            limiter.on_rate_limited();
            sleep(retry_delay(status, resp.headers(), attempts)).await;
            continue;
        }
        if !status.is_success() {
            return Err(format!("events fetch failed: http {status}"));
        }

        let body = match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                if attempts < 6 {
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempts)).await;
                    continue;
                }
                return Err(format!("events body read failed: {e}"));
            }
        };

        let raw: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(e) => {
                if attempts < 6 {
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempts)).await;
                    continue;
                }
                let preview = String::from_utf8_lossy(&body)
                    .chars()
                    .take(240)
                    .collect::<String>();
                return Err(format!("events parse failed: {e}; body_prefix={preview:?}"));
            }
        };

        let rows_value = if raw.is_array() {
            raw
        } else if let Some(data) = raw.get("data") {
            data.clone()
        } else if let Some(events) = raw.get("events") {
            events.clone()
        } else {
            return Err("events metadata response missing data/events".to_string());
        };
        let rows: Vec<PolyEventMetaRow> = match serde_json::from_value(rows_value) {
            Ok(rows) => rows,
            Err(e) => {
                if attempts < 6 {
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempts)).await;
                    continue;
                }
                return Err(format!("events decode failed: {e}"));
            }
        };
        limiter.on_success();

        return Ok(EventPageResult {
            rows,
            attempts,
            retry_429,
            retry_5xx,
        });
    }
}

async fn fetch_polymarket_market_page(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    page: usize,
    limit: usize,
    offset: usize,
) -> std::result::Result<MarketPageResult, String> {
    let mut attempts = 0usize;
    let mut retry_429 = 0usize;
    let mut retry_5xx = 0usize;

    loop {
        limiter.wait().await;
        attempts = attempts.saturating_add(1);

        let mut url = reqwest::Url::parse(&format!("{}/markets", POLYMARKET_GAMMA_URL))
            .map_err(|e| format!("markets url parse failed: {e}"))?;
        url.query_pairs_mut()
            .append_pair("active", "true")
            .append_pair("closed", "false")
            .append_pair("limit", &limit.to_string())
            .append_pair("offset", &offset.to_string());

        let resp = match client.get(url).send().await {
            Ok(resp) => resp,
            Err(err) => {
                if attempts < 6 {
                    retry_5xx = retry_5xx.saturating_add(1);
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempts)).await;
                    continue;
                }
                return Err(format!("markets fetch failed: {err}"));
            }
        };
        let status = resp.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts < 6 {
            retry_429 = retry_429.saturating_add(1);
            limiter.on_rate_limited();
            sleep(retry_delay(status, resp.headers(), attempts)).await;
            continue;
        }
        if status.is_server_error() && attempts < 6 {
            retry_5xx = retry_5xx.saturating_add(1);
            limiter.on_rate_limited();
            sleep(retry_delay(status, resp.headers(), attempts)).await;
            continue;
        }
        if !status.is_success() {
            return Err(format!("markets fetch failed: http {status}"));
        }

        let body = match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                if attempts < 6 {
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempts)).await;
                    continue;
                }
                return Err(format!("markets body read failed: {e}"));
            }
        };

        let raw: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(e) => {
                if attempts < 6 {
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempts)).await;
                    continue;
                }
                let preview = String::from_utf8_lossy(&body)
                    .chars()
                    .take(240)
                    .collect::<String>();
                return Err(format!("markets parse failed: {e}; body_prefix={preview:?}"));
            }
        };

        let rows_value = if raw.is_array() {
            raw
        } else if let Some(data) = raw.get("data") {
            data.clone()
        } else if let Some(markets) = raw.get("markets") {
            markets.clone()
        } else {
            return Err("markets response missing data/markets".to_string());
        };

        let rows: Vec<PolyMarketRow> = match serde_json::from_value(rows_value) {
            Ok(rows) => rows,
            Err(e) => {
                if attempts < 6 {
                    limiter.on_rate_limited();
                    sleep(backoff_for_attempt(attempts)).await;
                    continue;
                }
                return Err(format!("markets decode failed: {e}"));
            }
        };
        limiter.on_success();

        return Ok(MarketPageResult {
            page,
            rows,
            attempts,
            retry_429,
            retry_5xx,
        });
    }
}

fn normalize_topic(raw: &str) -> Option<String> {
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
}

fn json_value_to_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn derive_event_category(event: &PolyMarketRowEvent) -> Option<String> {
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
}

fn is_sports_topic(cat: &str) -> bool {
    let c = cat.to_ascii_lowercase();
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
}

fn normalize_polymarket_status(active: Option<bool>, closed: Option<bool>) -> Option<String> {
    if closed.unwrap_or(false) {
        return Some("closed".to_string());
    }
    if active.unwrap_or(false) {
        return Some("open".to_string());
    }
    match (active, closed) {
        (Some(false), Some(false)) => Some("inactive".to_string()),
        _ => None,
    }
}

fn midpoint_prob(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(((x + y) / 2.0).clamp(0.0, 1.0)),
        (Some(x), None) | (None, Some(x)) => Some(x.clamp(0.0, 1.0)),
        _ => None,
    }
}

fn backoff_for_attempt(attempt: usize) -> TokioDuration {
    let seconds = 2u64.saturating_mul(1u64 << ((attempt.saturating_sub(1)) as u32));
    TokioDuration::from_secs(seconds.min(30))
}

fn retry_delay(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    attempt: usize,
) -> TokioDuration {
    let base = backoff_for_attempt(attempt);
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return base;
    }
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| TokioDuration::from_secs(secs.min(60)));
    retry_after.map_or(base, |d| d.max(base))
}
