use super::super::timeseries::fetch::write_debug_payload;
use super::super::*;

pub(crate) fn json_value_to_string(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_json_array_strings(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<serde_json::Value>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(json_value_to_string)
        .collect()
}

pub(crate) fn parse_json_value_strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Array(arr) => arr.iter().cloned().map(json_value_to_string).collect(),
        serde_json::Value::String(s) => parse_json_array_strings(s),
        serde_json::Value::Null => Vec::new(),
        other => vec![json_value_to_string(other.clone())],
    }
}

fn parse_probability(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok()
}

fn search_terms(raw: &str) -> Vec<String> {
    raw.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn contains_any_term(haystack: &str, terms: &[String]) -> bool {
    let lower = haystack.to_ascii_lowercase();
    terms.iter().any(|term| lower.contains(term))
}

fn matched_term_count(haystack: &str, terms: &[String]) -> usize {
    let lower = haystack.to_ascii_lowercase();
    terms
        .iter()
        .filter(|term| lower.contains(term.as_str()))
        .count()
}

fn matches_query_terms(phrase_match: bool, term_hits: usize, total_terms: usize) -> bool {
    if phrase_match {
        return true;
    }
    if total_terms >= 2 {
        term_hits >= 2
    } else {
        term_hits >= 1
    }
}

fn query_mentions_explicit_year(query: &str) -> bool {
    query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|token| token.parse::<i32>().ok())
        .any(|year| (1900..=2100).contains(&year))
}

fn extract_year_tokens(text: &str) -> Vec<i32> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|token| token.parse::<i32>().ok())
        .filter(|year| (1900..=2100).contains(year))
        .collect()
}

fn is_probably_stale_open_market(
    title: &str,
    status: Option<&str>,
    query: Option<&str>,
    current_year: i32,
) -> bool {
    let is_open = status
        .map(|s| s.trim().eq_ignore_ascii_case("open"))
        .unwrap_or(true);
    if !is_open {
        return false;
    }
    if let Some(q) = query {
        if query_mentions_explicit_year(q) {
            return false;
        }
    }
    extract_year_tokens(title)
        .into_iter()
        .any(|year| year <= current_year - 1)
}

fn text_relevance_score(text: &str, terms: &[String]) -> i32 {
    if terms.is_empty() {
        return 0;
    }
    let lower = text.to_ascii_lowercase();
    let mut score = 0i32;
    for term in terms {
        if lower == *term {
            score += 6;
        } else if lower.contains(term) {
            score += 3;
        }
        if lower.starts_with(term) {
            score += 2;
        }
    }
    score
}

fn score_listed_event(e: &OddsListedEvent, terms: &[String]) -> i32 {
    let mut score = 0i32;
    score += text_relevance_score(&e.title, terms) * 4;
    score += text_relevance_score(&e.ticker, terms) * 3;
    if let Some(slug) = e.slug.as_deref() {
        score += text_relevance_score(slug, terms) * 2;
    }
    if let Some(category) = e.category.as_deref() {
        score += text_relevance_score(category, terms);
    }
    score
}

fn score_listed_market(m: &OddsListedMarket, terms: &[String]) -> i32 {
    let mut score = 0i32;
    score += text_relevance_score(&m.title, terms) * 4;
    score += text_relevance_score(&m.ticker, terms) * 3;
    score += text_relevance_score(&m.event_ticker, terms) * 2;
    if let Some(slug) = m.slug.as_deref() {
        score += text_relevance_score(slug, terms) * 2;
    }
    if let Some(category) = m.category.as_deref() {
        score += text_relevance_score(category, terms);
    }
    if let Some(status) = m.status.as_deref() {
        if status.eq_ignore_ascii_case("open") {
            score += 4;
        }
    }
    if let Some(volume) = m.volume {
        if volume > 0 {
            score += ((volume as f64).log10().floor() as i32).max(1);
        }
    }
    score
}

fn score_market(m: &OddsMarket, terms: &[String]) -> i32 {
    let mut score = 0i32;
    score += text_relevance_score(&m.title, terms) * 4;
    score += text_relevance_score(&m.ticker, terms) * 3;
    score += text_relevance_score(&m.event_ticker, terms) * 2;
    if let Some(slug) = m.slug.as_deref() {
        score += text_relevance_score(slug, terms) * 2;
    }
    if let Some(status) = m.status.as_deref() {
        if status.eq_ignore_ascii_case("open") {
            score += 4;
        }
    }
    if let Some(volume) = m.volume {
        if volume > 0 {
            score += ((volume as f64).log10().floor() as i32).max(1);
        }
    }
    score
}

async fn fetch_kalshi_tags_by_categories(
    client: &reqwest::Client,
) -> Result<HashMap<String, Vec<String>>> {
    #[derive(Deserialize)]
    struct TagsByCategoriesResp {
        #[serde(default)]
        tags_by_categories: HashMap<String, Option<Vec<String>>>,
    }

    let url = format!("{}/search/tags_by_categories", KALSHI_BASE_URL);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("kalshi tags_by_categories fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "kalshi tags_by_categories fetch failed: http {}",
            resp.status()
        )));
    }

    let body: TagsByCategoriesResp = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("kalshi tags_by_categories parse failed: {e}")))?;

    Ok(body
        .tags_by_categories
        .into_iter()
        .map(|(category, tags)| (category, tags.unwrap_or_default()))
        .collect())
}

fn derive_kalshi_categories_for_query(
    query: &str,
    tags_by_categories: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let query_lc = query.trim().to_ascii_lowercase();
    if query_lc.is_empty() {
        return Vec::new();
    }
    let terms = search_terms(&query_lc);

    let mut scored: Vec<(String, i32)> = tags_by_categories
        .iter()
        .filter_map(|(category, tags)| {
            let mut score = 0i32;
            if category.to_ascii_lowercase().contains(&query_lc) {
                score += 4;
            }
            if tags
                .iter()
                .any(|tag| tag.to_ascii_lowercase().contains(&query_lc))
            {
                score += 3;
            }
            if contains_any_term(category, &terms) {
                score += 2;
            }
            if tags.iter().any(|tag| contains_any_term(tag, &terms)) {
                score += 1;
            }
            (score > 0).then_some((category.clone(), score))
        })
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored
        .into_iter()
        .take(6)
        .map(|(category, _)| category)
        .collect()
}

pub(crate) fn probability_yes_from_outcomes(outcomes: &[String], prices: &[String]) -> Option<f64> {
    let mut idx = None;
    for (i, o) in outcomes.iter().enumerate() {
        if o.trim().eq_ignore_ascii_case("yes") {
            idx = Some(i);
            break;
        }
    }
    let i = idx?;
    prices.get(i).and_then(|p| parse_probability(p))
}

fn build_odds_analytics(markets: &[OddsMarket]) -> Option<OddsAnalytics> {
    if markets.is_empty() {
        return None;
    }

    let markets_total = markets.len();
    let mut open_markets = 0usize;
    let mut active_markets = 0usize;
    let mut initialized_markets = 0usize;
    let mut markets_with_volume = 0usize;
    let mut volume_sum: i64 = 0;
    let mut prob_count = 0usize;
    let mut prob_sum = 0.0f64;
    let mut spread_count = 0usize;
    let mut spread_sum = 0.0f64;

    for m in markets {
        if let Some(status) = m.status.as_deref() {
            let status = status.to_ascii_lowercase();
            if status == "open" {
                open_markets += 1;
            }
            if status == "active" {
                active_markets += 1;
            }
            if status == "initialized" {
                initialized_markets += 1;
            }
        }

        if let Some(v) = m.volume {
            if v > 0 {
                markets_with_volume += 1;
                volume_sum = volume_sum.saturating_add(v);
            }
        }

        if let Some(p) = m.probability_yes {
            prob_count += 1;
            prob_sum += p;
        }

        if let (Some(ask), Some(bid)) = (m.yes_ask, m.yes_bid) {
            if ask >= bid {
                spread_count += 1;
                spread_sum += (ask - bid) as f64;
            }
        }
    }

    Some(OddsAnalytics {
        markets_total,
        open_markets,
        active_markets,
        initialized_markets,
        markets_with_volume,
        total_volume: Some(volume_sum),
        average_probability_yes: (prob_count > 0).then_some(prob_sum / prob_count as f64),
        average_spread_cents: (spread_count > 0).then_some(spread_sum / spread_count as f64),
    })
}

pub(crate) fn build_odds_analytics_from_listed(
    markets: &[OddsListedMarket],
) -> Option<OddsAnalytics> {
    if markets.is_empty() {
        return None;
    }
    let converted: Vec<OddsMarket> = markets
        .iter()
        .map(|m| OddsMarket {
            ticker: m.ticker.clone(),
            title: m.title.clone(),
            event_ticker: m.event_ticker.clone(),
            status: m.status.clone(),
            yes_price: m.yes_price,
            yes_bid: None,
            yes_ask: None,
            volume: m.volume,
            source: m.source.clone(),
            market_id: m.market_id.clone(),
            event_id: m.event_id.clone(),
            slug: m.slug.clone(),
            outcomes: m.outcomes.clone(),
            outcome_prices: m.outcome_prices.clone(),
            clob_token_ids: m.clob_token_ids.clone(),
            probability_yes: m.probability_yes,
            outcome_best_bids: None,
            outcome_best_asks: None,
            orderbook_timestamp: None,
        })
        .collect();
    build_odds_analytics(&converted)
}

#[derive(Deserialize)]
struct PolyBookLevel {
    price: String,
    size: String,
}

#[derive(Deserialize)]
struct PolyBookMessage {
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    asset_id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    bids: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    asks: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    buys: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    sells: Option<Vec<PolyBookLevel>>,
}

struct PolyBookSnapshot {
    best_bid: Option<String>,
    best_ask: Option<String>,
    timestamp: Option<String>,
}

async fn fetch_polymarket_books_ws(
    token_ids: &[String],
    timeout_ms: u64,
) -> Result<std::collections::HashMap<String, PolyBookSnapshot>> {
    use tokio_tungstenite::tungstenite::Message;
    let mut out: std::collections::HashMap<String, PolyBookSnapshot> =
        std::collections::HashMap::new();
    if token_ids.is_empty() {
        return Ok(out);
    }

    let (mut ws, _) = connect_async("wss://ws-subscriptions-clob.polymarket.com/ws/market")
        .await
        .map_err(|e| Error::Provider(format!("polymarket ws connect failed: {e}")))?;

    let subscribe = serde_json::json!({
        "type": "market",
        "assets_ids": token_ids,
    });
    ws.send(Message::Text(subscribe.to_string()))
        .await
        .map_err(|e| Error::Provider(format!("polymarket ws subscribe failed: {e}")))?;

    let deadline = tokio::time::Instant::now() + TokioDuration::from_millis(timeout_ms.max(1));
    while out.len() < token_ids.len() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        let next = tokio::time::timeout(remaining, ws.next()).await;
        let Some(msg) = next.ok().and_then(|v| v.transpose().ok()).flatten() else {
            break;
        };
        if let Message::Text(text) = msg {
            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg: PolyBookMessage = match serde_json::from_value(parsed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg.event_type != "book" {
                continue;
            }
            let Some(asset_id) = msg.asset_id.clone() else {
                continue;
            };
            let bids = msg.bids.or(msg.buys).unwrap_or_default();
            let asks = msg.asks.or(msg.sells).unwrap_or_default();
            let best_bid = bids.first().map(|b| b.price.clone());
            let best_ask = asks.first().map(|a| a.price.clone());
            out.insert(
                asset_id,
                PolyBookSnapshot {
                    best_bid,
                    best_ask,
                    timestamp: msg.timestamp.clone(),
                },
            );
        }
    }

    let _ = ws.close(None).await;
    Ok(out)
}

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

pub(crate) async fn fetch_odds_polymarket(req: &OddsRequest) -> Result<OddsResponse> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(30))
        .connect_timeout(StdDuration::from_secs(10))
        .build()
        .map_err(|e| Error::Provider(format!("odds client init failed: {e}")))?;

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

        return Ok(OddsResponse {
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            generated_at: Utc::now(),
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
                                yes_price: None,
                                volume: None,
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
        return Ok(OddsResponse {
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            generated_at: Utc::now(),
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

                return Ok(OddsResponse {
                    base_url: POLYMARKET_GAMMA_URL.to_string(),
                    generated_at: Utc::now(),
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
                        yes_price: None,
                        volume: None,
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
                        status: status.clone(),
                        yes_price: None,
                        yes_bid: None,
                        yes_ask: None,
                        volume: None,
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
    Ok(OddsResponse {
        base_url: POLYMARKET_GAMMA_URL.to_string(),
        generated_at: Utc::now(),
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

fn postprocess_odds_response(mut resp: OddsResponse, req: &OddsRequest) -> OddsResponse {
    let search = req
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(search_raw) = search else {
        return resp;
    };
    let terms = search_terms(search_raw);
    if terms.is_empty() {
        return resp;
    }

    use chrono::Datelike;
    let current_year = Utc::now().year();

    if let Some(events) = resp.available_events.as_mut() {
        events.sort_by(|a, b| {
            score_listed_event(b, &terms)
                .cmp(&score_listed_event(a, &terms))
                .then_with(|| a.title.cmp(&b.title))
        });
    }

    if let Some(markets) = resp.available_markets.as_mut() {
        markets.retain(|m| {
            !is_probably_stale_open_market(
                &m.title,
                m.status.as_deref(),
                Some(search_raw),
                current_year,
            )
        });
        markets.sort_by(|a, b| {
            score_listed_market(b, &terms)
                .cmp(&score_listed_market(a, &terms))
                .then_with(|| a.title.cmp(&b.title))
        });
    }

    if !resp.markets.is_empty() {
        resp.markets.retain(|m| {
            !is_probably_stale_open_market(
                &m.title,
                m.status.as_deref(),
                Some(search_raw),
                current_year,
            )
        });
        resp.markets.sort_by(|a, b| {
            score_market(b, &terms)
                .cmp(&score_market(a, &terms))
                .then_with(|| a.title.cmp(&b.title))
        });
    }

    if resp.available_markets.is_some() {
        resp.analytics = resp
            .available_markets
            .as_ref()
            .and_then(|m| build_odds_analytics_from_listed(m));
    } else if !resp.markets.is_empty() {
        resp.analytics = build_odds_analytics(&resp.markets);
    }

    resp
}

pub async fn fetch_odds(req: OddsRequest) -> Result<OddsResponse> {
    let mut provider = req
        .provider
        .as_deref()
        .unwrap_or("kalshi")
        .trim()
        .to_ascii_lowercase();

    if req.list_tags {
        if provider == "kalshi" {
            return Err(Error::InvalidInput(
                "list_tags is only supported for polymarket (use --provider polymarket or auto)"
                    .to_string(),
            ));
        }
        if provider == "auto" {
            provider = "polymarket".to_string();
        }
    }

    if req.disable_kalshi {
        if req.list_series {
            return Err(Error::InvalidInput(
                "list_series requires kalshi, but kalshi is disabled".to_string(),
            ));
        }
        if provider == "kalshi" || provider == "auto" {
            provider = "polymarket".to_string();
        }
    }

    if req.list_series {
        if provider == "polymarket" {
            return Err(Error::InvalidInput(
                "list_series is only supported for kalshi (use --provider kalshi or omit --provider)".to_string(),
            ));
        }
        let mut resp = fetch_odds_kalshi(req.clone()).await?;
        resp.sources = Some(vec![OddsSourceInfo {
            source: "kalshi".to_string(),
            base_url: KALSHI_BASE_URL.to_string(),
            ok: true,
            error: None,
        }]);
        return Ok(postprocess_odds_response(resp, &req));
    }

    if !req.list_events
        && !req.list_markets
        && !req.list_tags
        && req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(Error::InvalidInput(
            "use --list-events, --list-markets, or provide series/event/market ticker".to_string(),
        ));
    }

    if req.include_orderbook && req.market_ticker.as_deref().unwrap_or("").trim().is_empty() {
        return Err(Error::InvalidInput(
            "market_ticker is required when include_orderbook is true".to_string(),
        ));
    }

    if provider == "polymarket" {
        let mut poly = fetch_odds_polymarket(&req).await?;
        poly.sources = Some(vec![OddsSourceInfo {
            source: "polymarket".to_string(),
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            ok: true,
            error: None,
        }]);
        return Ok(postprocess_odds_response(poly, &req));
    }

    if provider == "auto" {
        let mut sources = Vec::new();
        let kalshi_result = fetch_odds_kalshi(req.clone()).await;
        match kalshi_result {
            Ok(mut kalshi) => {
                sources.push(OddsSourceInfo {
                    source: "kalshi".to_string(),
                    base_url: KALSHI_BASE_URL.to_string(),
                    ok: true,
                    error: None,
                });
                let has_events = kalshi
                    .available_events
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
                    || !kalshi.events.is_empty();
                let has_markets = kalshi
                    .available_markets
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
                    || !kalshi.markets.is_empty();
                let has_series = kalshi.series.is_some();

                let found = if req.list_events {
                    has_events
                } else if req.list_markets {
                    has_markets
                } else if req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
                    && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
                    && req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
                {
                    has_events || has_markets
                } else if !req.market_ticker.as_deref().unwrap_or("").trim().is_empty() {
                    has_markets || kalshi.orderbook.is_some()
                } else if !req.event_ticker.as_deref().unwrap_or("").trim().is_empty() {
                    has_events || has_markets
                } else {
                    has_series || has_markets
                };

                if found {
                    kalshi.sources = Some(sources);
                    return Ok(postprocess_odds_response(kalshi, &req));
                }

                let mut poly = fetch_odds_polymarket(&req).await?;
                sources.push(OddsSourceInfo {
                    source: "polymarket".to_string(),
                    base_url: POLYMARKET_GAMMA_URL.to_string(),
                    ok: true,
                    error: None,
                });
                poly.sources = Some(sources);
                return Ok(postprocess_odds_response(poly, &req));
            }
            Err(e) => {
                let msg = e.to_string();
                sources.push(OddsSourceInfo {
                    source: "kalshi".to_string(),
                    base_url: KALSHI_BASE_URL.to_string(),
                    ok: false,
                    error: Some(msg),
                });
            }
        }

        let mut poly = fetch_odds_polymarket(&req).await?;
        sources.push(OddsSourceInfo {
            source: "polymarket".to_string(),
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            ok: true,
            error: None,
        });
        poly.sources = Some(sources);
        return Ok(postprocess_odds_response(poly, &req));
    }

    let mut kalshi = fetch_odds_kalshi(req.clone()).await?;
    kalshi.sources = Some(vec![OddsSourceInfo {
        source: "kalshi".to_string(),
        base_url: KALSHI_BASE_URL.to_string(),
        ok: true,
        error: None,
    }]);
    Ok(postprocess_odds_response(kalshi, &req))
}
