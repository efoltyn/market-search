// Polymarket live search and market detail.
//
// Discovery rides Gamma /public-search. Its default relevance order buries
// the deep markets ("house": the 44M-volume House-control event is not on
// the first page of 966; a parlay and a breakup market are), so the same
// query also runs with sort=volume and sort=volume_24hr and the union is
// re-ranked locally with match quality + depth. Every multi-market event is
// returned as one grouped row (legs with joinable tickers), the same shape
// Kalshi event rows use.
//
// Volume basis: `volume_usd` is the market's LIFETIME volume (Gamma
// volumeNum), `volume_24h_usd` the last 24h; Kalshi rows use the same split.
// (Rows used to carry 24h volume as volume_usd, falling back to lifetime when
// 24h was absent, so one column mixed two bases.)

const LIVE_POLY_LIMIT_PER_TYPE: usize = 12;
const LIVE_POLY_EXACT_TAG_EVENT_LIMIT: usize = 8;
/// Legs shown per grouped event row; the rest are counted in legs_omitted.
const POLY_EVENT_ROW_LEGS: usize = 12;
/// Score penalty for combo/parlay legs unless the query asks for them.
const POLY_COMBO_PENALTY: i64 = 60;
const POLYMARKET_DATA_API_V2_URL: &str = "https://data-api.polymarket.com/v2";
const POLYMARKET_GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";

#[derive(Clone, serde::Deserialize)]
struct LivePolymarketSearchResp {
    #[serde(default)]
    events: Vec<LivePolymarketEvent>,
    #[serde(default)]
    tags: Vec<LivePolymarketSearchTag>,
}

#[derive(Clone, serde::Deserialize)]
struct LivePolymarketSearchTag {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Clone, serde::Deserialize)]
struct LivePolymarketEvent {
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
    volume: Option<serde_json::Value>,
    #[serde(default, rename = "volume24hr")]
    volume_24hr: Option<serde_json::Value>,
    #[serde(default, rename = "openInterest")]
    open_interest: Option<serde_json::Value>,
    #[serde(default, rename = "negRisk")]
    neg_risk: Option<bool>,
    #[serde(default)]
    tags: Vec<LivePolymarketTag>,
    #[serde(default)]
    markets: Vec<LivePolymarketMarket>,
}

#[derive(Clone, serde::Deserialize)]
struct LivePolymarketTag {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Clone, Default, serde::Deserialize)]
struct LivePolymarketMarket {
    #[serde(default)]
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
    #[serde(default)]
    volume: Option<serde_json::Value>,
    #[serde(default, rename = "volumeNum")]
    volume_num: Option<serde_json::Value>,
    #[serde(default, rename = "volume24hr")]
    volume_24hr: Option<serde_json::Value>,
    #[serde(default, rename = "liquidityNum")]
    liquidity_num: Option<serde_json::Value>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    closed: Option<bool>,
    #[serde(default, rename = "bestBid")]
    best_bid: Option<serde_json::Value>,
    #[serde(default, rename = "bestAsk")]
    best_ask: Option<serde_json::Value>,
    #[serde(default, rename = "lastTradePrice")]
    last_trade_price: Option<serde_json::Value>,
    #[serde(default, rename = "groupItemTitle")]
    group_item_title: Option<String>,
    #[serde(default, rename = "endDate")]
    end_date: Option<String>,
    #[serde(default, rename = "closedTime")]
    closed_time: Option<String>,
    #[serde(default, rename = "umaResolutionStatus")]
    uma_resolution_status: Option<String>,
    #[serde(default, rename = "oneDayPriceChange")]
    one_day_price_change: Option<serde_json::Value>,
}

/// Where a leg sits in its lifecycle. `Resolved` = UMA resolution final, so
/// outcomePrices are the payout (1/0), not a last trade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PolyLegState {
    Open,
    Closed,
    Resolved,
    /// Placeholder slot of a multi-outcome event ("Party C", "Other"):
    /// never priced, never traded.
    Placeholder,
}

impl LivePolymarketMarket {
    fn outcomes(&self) -> Vec<String> {
        live_jsonish_string_array(&self.outcomes)
    }

    fn prices(&self) -> Vec<String> {
        live_jsonish_string_array(&self.outcome_prices)
    }

    fn lifetime_volume_usd(&self) -> f64 {
        self.volume_num
            .as_ref()
            .and_then(live_parse_decimal_value)
            .or_else(|| self.volume.as_ref().and_then(live_parse_decimal_value))
            .unwrap_or(0.0)
    }

    fn volume_24h_usd(&self) -> f64 {
        self.volume_24hr
            .as_ref()
            .and_then(live_parse_decimal_value)
            .unwrap_or(0.0)
    }

    fn state(&self, event_open: bool) -> PolyLegState {
        if self.prices().is_empty() {
            return PolyLegState::Placeholder;
        }
        let closed = self.closed.unwrap_or(false);
        if !closed && self.active.unwrap_or(event_open) {
            return PolyLegState::Open;
        }
        if !closed {
            // Inactive but not closed: an unlaunched slot.
            return PolyLegState::Placeholder;
        }
        if self
            .uma_resolution_status
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("resolved"))
            && poly_resolved_outcome(&self.outcomes(), &self.prices()).is_some()
        {
            PolyLegState::Resolved
        } else {
            PolyLegState::Closed
        }
    }

    fn label(&self) -> String {
        self.group_item_title
            .as_deref()
            .map(live_normalize_whitespace)
            .filter(|s| !s.is_empty())
            .or_else(|| self.question.as_deref().map(live_normalize_whitespace))
            .unwrap_or_else(|| live_json_value_to_string(&self.id))
    }
}

/// Winning outcome of a resolved market: the one outcome priced exactly 1
/// while every other is 0. Anything else (split or voided payouts, a market
/// still trading) has no single winner and returns None. Yes/No lowercase to
/// match Kalshi's `result`.
fn poly_resolved_outcome(outcomes: &[String], prices: &[String]) -> Option<String> {
    if outcomes.len() != prices.len() || outcomes.is_empty() {
        return None;
    }
    let parsed: Vec<f64> = prices
        .iter()
        .map(|p| p.trim().parse::<f64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let winners: Vec<usize> = parsed
        .iter()
        .enumerate()
        .filter(|(_, p)| (**p - 1.0).abs() < 1e-9)
        .map(|(i, _)| i)
        .collect();
    let losers = parsed.iter().filter(|p| p.abs() < 1e-9).count();
    if winners.len() != 1 || losers != parsed.len() - 1 {
        return None;
    }
    let label = outcomes[winners[0]].trim();
    Some(if label.eq_ignore_ascii_case("yes") || label.eq_ignore_ascii_case("no") {
        label.to_ascii_lowercase()
    } else {
        label.to_string()
    })
}

/// Gamma timestamps come as "2025-09-17 21:27:38+00" or RFC3339; emit RFC3339 UTC.
fn poly_normalize_timestamp(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
    }
    let fixed = if s.ends_with("+00") { format!("{s}:00") } else { s.to_string() };
    for fmt in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%:z"] {
        if let Ok(dt) = chrono::DateTime::parse_from_str(&fixed, fmt) {
            return Some(
                dt.with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            );
        }
    }
    None
}

/// Combo/parlay detection from what Polymarket itself publishes: the
/// parlays/combos tags, "Combo" in the event title, conditional events
/// ("... pending oil above or below 80") and "A & B" joint events.
fn poly_event_is_combo(event_title: &str, tag_slugs: &[String]) -> bool {
    if tag_slugs
        .iter()
        .any(|t| t.contains("parlay") || t.contains("combo"))
    {
        return true;
    }
    let lower = event_title.to_ascii_lowercase();
    lower.contains("combo") || lower.contains(" pending ") || lower.contains(" & ")
}

/// A leg that bundles three or more named outcomes ("Will Republicans lose
/// the Alaska, Iowa, Kansas, ... and Texas Senate seats?") is a joint bet even
/// when its event carries no combo tag.
fn poly_market_title_is_combo(title: &str) -> bool {
    title.matches(", ").count() >= 2 && title.contains(" and ")
}

fn poly_query_wants_combo(query_terms: &[String]) -> bool {
    query_terms.iter().any(|t| {
        matches!(
            t.as_str(),
            "combo" | "combos" | "parlay" | "parlays" | "pending" | "sweep"
        )
    })
}

/// Which Gamma `events_status` values a search status needs.
fn poly_search_statuses(status: KalshiSearchStatus) -> &'static [&'static str] {
    match status {
        KalshiSearchStatus::Open => &["active"],
        KalshiSearchStatus::Closed | KalshiSearchStatus::Settled => &["closed"],
        KalshiSearchStatus::All => &["active", "closed"],
    }
}

fn poly_leg_matches_status(state: PolyLegState, status: KalshiSearchStatus) -> bool {
    match (status, state) {
        (_, PolyLegState::Placeholder) => false,
        (KalshiSearchStatus::Open, s) => s == PolyLegState::Open,
        (KalshiSearchStatus::Closed, s) => s == PolyLegState::Closed,
        (KalshiSearchStatus::Settled, s) => s == PolyLegState::Resolved,
        (KalshiSearchStatus::All, _) => true,
    }
}

fn poly_leg_probability(market: &LivePolymarketMarket) -> Option<f64> {
    live_probability_yes_from_outcomes(&market.outcomes(), &market.prices())
}

/// One grouped row per multi-market event, same schema as Kalshi event rows:
/// mutually exclusive (negRisk) legs sorted by probability, ladders (dates,
/// thresholds) in Polymarket's own order.
fn poly_event_group_row(
    event: &LivePolymarketEvent,
    event_ticker: &str,
    event_title: &str,
    category: &str,
    event_score: i64,
    legs: &[&LivePolymarketMarket],
    status: KalshiSearchStatus,
    event_open: bool,
) -> serde_json::Value {
    let mutually_exclusive = event.neg_risk.unwrap_or(false);
    let mut shown: Vec<(usize, &LivePolymarketMarket)> = legs.iter().copied().enumerate().collect();
    if shown.len() > POLY_EVENT_ROW_LEGS {
        shown.sort_by(|a, b| {
            b.1.lifetime_volume_usd()
                .partial_cmp(&a.1.lifetime_volume_usd())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        shown.truncate(POLY_EVENT_ROW_LEGS);
    }
    if mutually_exclusive {
        shown.sort_by(|a, b| {
            let pa = poly_leg_probability(a.1).unwrap_or(-1.0);
            let pb = poly_leg_probability(b.1).unwrap_or(-1.0);
            pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        // Ladders: date legs by close time, threshold legs (one shared
        // close) in Polymarket's own order.
        shown.sort_by(|a, b| {
            let close = |m: &LivePolymarketMarket| m.end_date.as_deref().and_then(poly_normalize_timestamp);
            close(a.1).cmp(&close(b.1)).then(a.0.cmp(&b.0))
        });
    }
    let leg_rows: Vec<serde_json::Value> = shown
        .iter()
        .map(|(_, m)| {
            let mut leg = serde_json::json!({
                "ticker": live_json_value_to_string(&m.id),
                "outcome": m.label(),
                "p": poly_leg_probability(m),
            });
            if m.state(event_open) == PolyLegState::Resolved {
                if let Some(r) = poly_resolved_outcome(&m.outcomes(), &m.prices()) {
                    leg["result"] = serde_json::json!(r);
                }
            }
            leg
        })
        .collect();
    let volume_usd = event
        .volume
        .as_ref()
        .and_then(live_parse_decimal_value)
        .unwrap_or_else(|| legs.iter().map(|m| m.lifetime_volume_usd()).sum());
    let close_times: std::collections::BTreeSet<String> = legs
        .iter()
        .filter_map(|m| m.end_date.as_deref().and_then(poly_normalize_timestamp))
        .collect();
    let mut row = serde_json::json!({
        "source": "polymarket",
        "event_ticker": event_ticker,
        "title": event_title,
        "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
        "match_score": event_score,
        "volume_usd": volume_usd,
        "legs": leg_rows,
    });
    if let Some(oi) = event.open_interest.as_ref().and_then(live_parse_decimal_value) {
        row["open_interest_usd"] = serde_json::json!(oi);
    }
    if mutually_exclusive {
        row["mutually_exclusive"] = serde_json::json!(true);
    }
    if let Some(first) = close_times.iter().next() {
        let key = if close_times.len() == 1 { "close_time" } else { "first_close_time" };
        row[key] = serde_json::json!(first);
    }
    if legs.len() > shown.len() {
        row["legs_omitted"] = serde_json::json!(legs.len() - shown.len());
    }
    if status != KalshiSearchStatus::Open {
        row["status_filter"] = serde_json::json!(status.as_str());
    }
    row
}

fn live_pick_exact_polymarket_tag(
    search_tags: &[LivePolymarketSearchTag],
    query_phrase: &str,
    query_terms: &[String],
) -> Option<String> {
    let mut best: Option<(String, i64)> = None;
    for tag in search_tags {
        let label = live_normalize_whitespace(tag.label.as_deref().unwrap_or_default());
        let slug = live_normalize_whitespace(tag.slug.as_deref().unwrap_or_default());
        let candidates = [label.as_str(), slug.as_str()];
        let mut matched = false;
        let mut score = 0i64;
        for candidate in candidates {
            if candidate.is_empty() {
                continue;
            }
            let candidate_lower = candidate.to_ascii_lowercase();
            if candidate_lower == query_phrase {
                matched = true;
                score = score.max(40);
            }
            if query_terms
                .iter()
                .any(|term| candidate_lower == term.to_ascii_lowercase())
            {
                matched = true;
                score = score.max(32);
            }
        }
        if !matched {
            continue;
        }
        let chosen = if !slug.is_empty() { slug } else { label };
        if chosen.is_empty() {
            continue;
        }
        let replace = match best.as_ref() {
            Some((best_slug, best_score)) => {
                score > *best_score || (score == *best_score && chosen < *best_slug)
            }
            None => true,
        };
        if replace {
            best = Some((chosen, score));
        }
    }
    best.map(|(slug, _)| slug)
}

fn live_polymarket_event_tags_text(event: &LivePolymarketEvent) -> String {
    event
        .tags
        .iter()
        .flat_map(|tag| [tag.label.as_deref(), tag.slug.as_deref()])
        .flatten()
        .map(live_normalize_whitespace)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `--category` on Polymarket: Gamma rarely fills `category`, so a filter
/// also matches any tag label or slug ("--category elections").
fn poly_event_matches_category(category: &str, tag_text: &str, filter: Option<&str>) -> bool {
    category_matches_filter(category, filter)
        || filter.is_some_and(|f| tag_text.to_ascii_lowercase().contains(&f.to_ascii_lowercase()))
}

async fn live_fetch_polymarket_results(
    client: &reqwest::Client,
    query: &str,
    query_phrase: &str,
    query_terms: &[String],
    opts: &CsvSearchOptions,
    profile_applied: SearchProfile,
) -> std::result::Result<
    (
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
        Vec<String>,
        Vec<serde_json::Value>,
        Option<String>,
    ),
    String,
> {
    let power_grid_query = live_query_has_power_grid_intent(query);
    let status = opts.status_filter;
    let url = "https://gamma-api.polymarket.com/public-search";

    // (events_status, sort) passes. The first is the plain relevance pass and
    // the only one that asks for matching tags.
    let mut passes: Vec<(&str, Option<&str>)> = Vec::new();
    for events_status in poly_search_statuses(status) {
        passes.push((events_status, None));
        passes.push((events_status, Some("volume")));
        if *events_status == "active" {
            passes.push((events_status, Some("volume_24hr")));
        }
    }
    let requests = passes.iter().enumerate().map(|(idx, (events_status, sort))| {
        let mut params = vec![
            ("q", query.to_string()),
            ("limit_per_type", LIVE_POLY_LIMIT_PER_TYPE.to_string()),
            ("page", "1".to_string()),
            ("events_status", events_status.to_string()),
            ("search_tags", (idx == 0).to_string()),
            ("search_profiles", "false".to_string()),
            ("optimized", "false".to_string()),
        ];
        if *events_status == "closed" {
            params.push(("keep_closed_markets", "1".to_string()));
        }
        if let Some(sort) = sort {
            params.push(("sort", sort.to_string()));
            params.push(("ascending", "false".to_string()));
        }
        async move {
            live_fetch_json::<LivePolymarketSearchResp>(
                client,
                url,
                &params,
                "polymarket public-search",
            )
            .await
        }
    });
    let results = futures::future::join_all(requests).await;

    let mut api_errors = Vec::new();
    let mut search_tags: Vec<LivePolymarketSearchTag> = Vec::new();
    // event key -> (event, best provider rank)
    let mut combined: Vec<(LivePolymarketEvent, usize, bool)> = Vec::new();
    let mut index_by_key: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut any_ok = false;
    let mut first_error: Option<String> = None;
    for (pass_idx, result) in results.into_iter().enumerate() {
        match result {
            Ok(body) => {
                any_ok = true;
                if pass_idx == 0 {
                    search_tags = body.tags;
                }
                for (rank, event) in body.events.into_iter().enumerate() {
                    let key = live_json_value_to_string(&event.id);
                    match index_by_key.get(&key) {
                        Some(&i) => combined[i].1 = combined[i].1.min(rank),
                        None => {
                            index_by_key.insert(key, combined.len());
                            combined.push((event, rank, false));
                        }
                    }
                }
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error.clone());
                }
                api_errors.push(serde_json::json!({
                    "phase": "event_discovery",
                    "source": "polymarket",
                    "pass": passes[pass_idx].1.unwrap_or("relevance"),
                    "error": error,
                }));
            }
        }
    }
    if !any_ok {
        return Err(first_error.unwrap_or_else(|| "polymarket public-search failed".to_string()));
    }

    let exact_tag_slug = live_pick_exact_polymarket_tag(&search_tags, query_phrase, query_terms);
    if let (Some(tag_slug), KalshiSearchStatus::Open) = (exact_tag_slug.as_deref(), status) {
        let tag_query = vec![
            ("tag_slug", tag_slug.to_string()),
            ("related_tags", "false".to_string()),
            ("active", "true".to_string()),
            ("closed", "false".to_string()),
            ("limit", LIVE_POLY_EXACT_TAG_EVENT_LIMIT.to_string()),
            ("order", "volume24hr".to_string()),
            ("ascending", "false".to_string()),
        ];
        let tag_url = "https://gamma-api.polymarket.com/events";
        match live_fetch_json::<Vec<LivePolymarketEvent>>(
            client,
            tag_url,
            &tag_query,
            "polymarket events by tag",
        )
        .await
        {
            Ok(events) => {
                for (rank, event) in events.into_iter().enumerate() {
                    let key = live_json_value_to_string(&event.id);
                    match index_by_key.get(&key) {
                        Some(&i) => combined[i].2 = true,
                        None => {
                            index_by_key.insert(key, combined.len());
                            combined.push((event, rank + LIVE_POLY_LIMIT_PER_TYPE, true));
                        }
                    }
                }
            }
            Err(error) => api_errors.push(serde_json::json!({
                "phase": "event_discovery",
                "source": "polymarket_tag_events",
                "tag_slug": tag_slug,
                "error": error,
            })),
        }
    }
    combined.sort_by_key(|(_, rank, _)| *rank);

    let expansion_seed_events: Vec<LivePolymarketEvent> = combined
        .iter()
        .filter(|(event, _, _)| {
            if !power_grid_query {
                return true;
            }
            let title = event.title.as_deref().unwrap_or_default();
            let category = event.category.as_deref().unwrap_or_default();
            let tag_text = live_polymarket_event_tags_text(event);
            live_market_has_power_grid_context(title, title, category, &tag_text)
        })
        .map(|(event, _, _)| event.clone())
        .collect();
    let expansion_terms = live_extract_expansion_terms(query_terms, &expansion_seed_events);
    let wants_combo = poly_query_wants_combo(query_terms);
    let now = chrono::Utc::now();
    let close_cutoff = opts.closing_within.map(|d| now + d);
    let mut events = Vec::new();
    let mut markets = Vec::new();
    let mut seen_markets = std::collections::HashSet::new();

    for (event, event_rank, exact_tag_grounded) in combined.iter() {
        let event_ticker = event
            .ticker
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| live_json_value_to_string(&event.id));
        let title = event.title.clone().unwrap_or_else(|| event_ticker.clone());
        let category = event.category.clone().unwrap_or_default();
        let tag_text = live_polymarket_event_tags_text(event);
        let tag_slugs: Vec<String> = event
            .tags
            .iter()
            .filter_map(|t| t.slug.as_deref().map(str::to_ascii_lowercase))
            .collect();
        if !poly_event_matches_category(&category, &tag_text, opts.category_filter.as_deref()) {
            continue;
        }
        let event_has_power_grid_context =
            live_market_has_power_grid_context(&title, &title, &category, &tag_text);
        if power_grid_query && !event_has_power_grid_context {
            continue;
        }
        // An event whose legs are mostly bundles ("Build Your Map": 128
        // state combinations) is a combo event even without a combo tag.
        let list_legs = event
            .markets
            .iter()
            .filter(|m| m.question.as_deref().is_some_and(poly_market_title_is_combo))
            .count();
        let combo = poly_event_is_combo(&title, &tag_slugs)
            || (event.markets.len() >= 4 && list_legs * 2 >= event.markets.len());
        let event_volume = event
            .volume
            .as_ref()
            .and_then(live_parse_decimal_value)
            .unwrap_or(0.0);
        let event_volume_24h = event
            .volume_24hr
            .as_ref()
            .and_then(live_parse_decimal_value)
            .unwrap_or(0.0);
        let event_open = event.active.unwrap_or(true) && !event.closed.unwrap_or(false);
        let mut matched_terms = Vec::new();
        let (event_title_score, event_title_terms) =
            live_scored_hits(&title, query_phrase, query_terms, 16, 10, 32);
        let (event_slug_score, event_slug_terms) = live_scored_hits(
            event.slug.as_deref().unwrap_or_default(),
            query_phrase,
            query_terms,
            8,
            5,
            8,
        );
        let (event_category_score, event_category_terms) =
            live_scored_hits(&category, query_phrase, query_terms, 5, 3, 0);
        let (event_tag_score, event_tag_terms) =
            live_scored_hits(&tag_text, query_phrase, query_terms, 4, 2, 0);
        live_extend_unique(&mut matched_terms, &event_title_terms);
        live_extend_unique(&mut matched_terms, &event_slug_terms);
        live_extend_unique(&mut matched_terms, &event_category_terms);
        live_extend_unique(&mut matched_terms, &event_tag_terms);

        let mut event_score = event_title_score
            + event_slug_score
            + event_category_score
            + event_tag_score
            + live_volume_score(event_volume)
            + live_recent_volume_score(event_volume_24h) / 2
            + (LIVE_POLY_LIMIT_PER_TYPE.saturating_sub(*event_rank) as i64) * 3;
        event_score += live_scored_hits(&tag_text, "", &expansion_terms, 2, 1, 0).0;
        if *exact_tag_grounded {
            event_score += 18;
        }
        if event_open {
            event_score += 15;
        }
        if combo && !wants_combo {
            event_score -= POLY_COMBO_PENALTY;
        }
        if profile_applied == SearchProfile::Macro {
            event_score += macro_relevance_score(
                &title,
                &category,
                &category,
                &[],
                &matched_terms,
                &opts.policy,
            );
        }

        let mut kept_legs: Vec<&LivePolymarketMarket> = Vec::new();
        for (market_rank, market) in event.markets.iter().enumerate() {
            let state = market.state(event_open);
            if !poly_leg_matches_status(state, status) {
                continue;
            }
            let close_time = market.end_date.as_deref().and_then(poly_normalize_timestamp);
            if let Some(cutoff) = close_cutoff {
                let closes_in_window = close_time
                    .as_deref()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .is_some_and(|t| t >= now && t <= cutoff);
                if state != PolyLegState::Open || !closes_in_window {
                    continue;
                }
            }
            let market_title = market.question.clone().unwrap_or_else(|| title.clone());
            if power_grid_query
                && !live_market_has_power_grid_context(&market_title, &title, &category, &tag_text)
            {
                continue;
            }
            kept_legs.push(market);

            let outcomes = market.outcomes();
            let outcome_prices = market.prices();
            let probability_yes = live_probability_yes_from_outcomes(&outcomes, &outcome_prices);
            let volume_usd = market.lifetime_volume_usd();
            let volume_24h_usd = market.volume_24h_usd();
            let liquidity_usd = market
                .liquidity_num
                .as_ref()
                .and_then(live_parse_decimal_value)
                .unwrap_or(0.0);
            let status_label = match state {
                PolyLegState::Open => "open",
                PolyLegState::Closed => "closed",
                PolyLegState::Resolved => "settled",
                PolyLegState::Placeholder => "inactive",
            };
            let base_score =
                event_score + (LIVE_POLY_LIMIT_PER_TYPE.saturating_sub(market_rank) as i64);
            let (mut match_score, matched_terms) = live_market_score(
                &market_title,
                &title,
                &category,
                &tag_text,
                query_phrase,
                query_terms,
                &expansion_terms,
                volume_usd,
                volume_24h_usd,
                liquidity_usd,
                close_time.as_deref(),
                Some(if state == PolyLegState::Open { "open" } else { "closed" }),
                probability_yes,
                base_score,
                profile_applied,
                &opts.policy,
            );
            if !combo && !wants_combo && poly_market_title_is_combo(&market_title) {
                match_score -= POLY_COMBO_PENALTY;
            }
            if state != PolyLegState::Open && status != KalshiSearchStatus::Open {
                // Closed searches: live_market_score's open bonus/penalty
                // (+25 / -200) carries no meaning when every row is closed.
                match_score += 225;
            }

            let ticker = live_json_value_to_string(&market.id);
            if !seen_markets.insert(format!("polymarket::{ticker}")) {
                continue;
            }
            let clob_token_ids = live_jsonish_string_array(&market.clob_token_ids);
            let best_bid = market.best_bid.as_ref().and_then(live_parse_decimal_value);
            let best_ask = market.best_ask.as_ref().and_then(live_parse_decimal_value);
            let mut market_json = serde_json::json!({
                "source": "polymarket",
                "ticker": ticker,
                "title": market_title,
                "event_ticker": event_ticker,
                "event_title": title,
                "yes_price": probability_yes,
                "last_trade_price": market.last_trade_price.as_ref().and_then(live_parse_decimal_value),
                "yes_bid": if state == PolyLegState::Open { best_bid } else { None },
                "yes_ask": if state == PolyLegState::Open { best_ask } else { None },
                "volume": live_volume_to_cents(volume_usd),
                "volume_usd": volume_usd,
                "volume_24h_usd": volume_24h_usd,
                "liquidity_usd": liquidity_usd,
                "close_time": close_time,
                "status": status_label,
                "probability_yes": probability_yes,
                "slug": market.slug,
                "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
                "match_score": match_score,
                "match_terms": matched_terms,
                "score_components": {
                    "provider_rank_bonus": base_score - event_score,
                    "volume_usd": volume_usd,
                },
                "outcomes": if outcomes.is_empty() { serde_json::Value::Null } else { serde_json::json!(outcomes) },
                "outcome_prices": if outcome_prices.is_empty() { serde_json::Value::Null } else { serde_json::json!(outcome_prices) },
                "clob_token_ids": if clob_token_ids.is_empty() { serde_json::Value::Null } else { serde_json::json!(clob_token_ids) },
            });
            if event.markets.len() > 1 {
                market_json["outcome"] = serde_json::json!(market.label());
            }
            if combo || poly_market_title_is_combo(&market_title) {
                market_json["combo"] = serde_json::json!(true);
            }
            if state == PolyLegState::Open {
                // Polymarket's own day-ago move, in probability points.
                if let Some(chg) = market
                    .one_day_price_change
                    .as_ref()
                    .and_then(live_parse_decimal_value)
                    .filter(|c| c.is_finite())
                {
                    market_json["chg_24h_pp"] = serde_json::json!((chg * 1000.0).round() / 10.0);
                }
            }
            if state == PolyLegState::Resolved {
                market_json["result"] =
                    serde_json::json!(poly_resolved_outcome(&outcomes, &outcome_prices));
                if let Some(at) = market.closed_time.as_deref().and_then(poly_normalize_timestamp) {
                    market_json["settled_at"] = serde_json::json!(at);
                }
            }
            attach_market_delta(&mut market_json, "polymarket", &ticker, None);
            markets.push(market_json);
        }

        if kept_legs.is_empty() {
            continue;
        }
        if kept_legs.len() > 1 {
            let mut row = poly_event_group_row(
                event,
                &event_ticker,
                &title,
                &category,
                event_score,
                &kept_legs,
                status,
                event_open,
            );
            if combo {
                row["combo"] = serde_json::json!(true);
            }
            events.push(row);
        } else {
            events.push(serde_json::json!({
                "source": "polymarket",
                "event_ticker": event_ticker,
                "title": title,
                "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
                "slug": event.slug,
                "match_score": event_score,
                "volume_usd": event_volume,
            }));
        }
    }
    events.sort_by(|a, b| {
        let score = |v: &serde_json::Value| v.get("match_score").and_then(|s| s.as_i64()).unwrap_or(0);
        score(b).cmp(&score(a))
    });

    if events.is_empty() && markets.is_empty() && !power_grid_query {
        api_errors.push(serde_json::json!({
            "phase": "event_discovery",
            "source": "polymarket",
            "error": format!("public-search returned no {} events", status.as_str()),
        }));
    }

    Ok((events, markets, expansion_terms, api_errors, exact_tag_slug))
}

// ---------------------------------------------------------------------------
// Market detail: `--market <numeric id>` (Polymarket).
// ---------------------------------------------------------------------------

/// Which finance_timeseries ticker returns which outcome's price path:
/// the bare id is outcome 0 (Yes), `<id>:NO` is outcome 1.
fn poly_timeseries_join(ticker: &str, outcomes: &[String]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let first = outcomes.first().cloned().unwrap_or_else(|| "Yes".to_string());
    let second = outcomes.get(1).cloned().unwrap_or_else(|| "No".to_string());
    map.insert(first, serde_json::json!(ticker));
    map.insert(second, serde_json::json!(format!("{ticker}:NO")));
    serde_json::Value::Object(map)
}

/// One Polymarket fill from /v2/trades, projected onto Kalshi's trade shape:
/// `yes_price` is the price of outcome 0 (Yes), `contracts` the shares, and
/// `taker` the side the taker ended up on (buying Yes or selling No = yes).
#[derive(Clone, Debug, PartialEq)]
struct PolyTrade {
    ts: i64,
    yes_price: f64,
    shares: f64,
    usd: f64,
    taker: &'static str,
    wallet: String,
}

fn poly_trade_from_json(trade: &serde_json::Value) -> Option<PolyTrade> {
    let ts = trade.get("timestamp").and_then(|v| v.as_i64())?;
    let price = trade.get("price").and_then(live_parse_decimal_value)?;
    let shares = trade.get("size").and_then(live_parse_decimal_value)?;
    let outcome_index = trade.get("outcome_index").and_then(|v| v.as_i64())?;
    let buy = match trade.get("side").and_then(|v| v.as_str())? {
        s if s.eq_ignore_ascii_case("BUY") => true,
        s if s.eq_ignore_ascii_case("SELL") => false,
        _ => return None,
    };
    let (yes_price, outcome0) = match outcome_index {
        0 => (price, true),
        1 => (1.0 - price, false),
        _ => return None, // 999 = unlabeled outcome
    };
    Some(PolyTrade {
        ts,
        yes_price,
        shares,
        usd: shares * price,
        taker: if buy == outcome0 { "yes" } else { "no" },
        wallet: trade
            .get("proxy_wallet")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .chars()
            .take(10)
            .collect(),
    })
}

/// Summary + the `limit` largest prints (by USD), listed in time order.
/// Same schema as Kalshi's trades payload plus `usd` per print.
fn poly_trades_payload(
    ticker: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    trades: &[PolyTrade],
    truncated: bool,
    min_usd: Option<f64>,
    limit: usize,
) -> serde_json::Value {
    let ts = |t: i64| {
        chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0)
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    };
    let side_sum = |side: &str, f: fn(&PolyTrade) -> f64| -> f64 {
        trades.iter().filter(|t| t.taker == side).map(f).sum()
    };
    let r2 = |x: f64| (x * 100.0).round() / 100.0;
    let mut picked: Vec<&PolyTrade> = trades.iter().collect();
    let matching = picked.len();
    if picked.len() > limit {
        picked.sort_by(|a, b| b.usd.partial_cmp(&a.usd).unwrap_or(std::cmp::Ordering::Equal));
        picked.truncate(limit);
    }
    picked.sort_by_key(|t| t.ts);
    let rows: Vec<serde_json::Value> = picked
        .iter()
        .map(|t| {
            serde_json::json!({
                "t": ts(t.ts),
                "yes_price": (t.yes_price * 10_000.0).round() / 10_000.0,
                "contracts": r2(t.shares),
                "usd": r2(t.usd),
                "taker": t.taker,
                "wallet": t.wallet,
            })
        })
        .collect();
    let first = trades.iter().map(|t| t.ts).min();
    let last = trades.iter().map(|t| t.ts).max();
    let mut out = serde_json::json!({
        "schema_version": "finance.odds.trades.v1",
        "source": "polymarket",
        "ticker": ticker,
        "start": start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "end": end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "_summary": {
            "trades": trades.len(),
            "contracts": r2(trades.iter().map(|t| t.shares).sum()),
            "usd": r2(trades.iter().map(|t| t.usd).sum()),
            "yes_taker_contracts": r2(side_sum("yes", |t| t.shares)),
            "no_taker_contracts": r2(side_sum("no", |t| t.shares)),
            "yes_taker_usd": r2(side_sum("yes", |t| t.usd)),
            "no_taker_usd": r2(side_sum("no", |t| t.usd)),
            "first": first.and_then(ts),
            "last": last.and_then(ts),
        },
        "trades": rows,
    });
    if let Some(min) = min_usd {
        out["min_trade_usd"] = serde_json::json!(min);
    }
    if matching > rows.len() {
        out["trades_note"] = serde_json::json!(format!(
            "{} of {matching} matching prints shown: the largest by USD, in time order (raise --limit or --min-trade-usd)",
            rows.len()
        ));
    }
    if truncated {
        out["window_truncated"] = serde_json::json!(format!(
            "page cap hit: prints before {} are missing; narrow --since/--until or raise --min-trade-usd",
            first.and_then(ts).unwrap_or_else(|| "?".to_string())
        ));
    }
    out
}

const POLY_TRADES_PAGE: usize = 1000;
const POLY_TRADES_MAX_PAGES: usize = 10;

/// `--market <id> --trades`: taker fills over --since/--until (default 24h).
/// /v2/trades serves a market's feed newest first and ignores start/end on
/// the condition shape, so we page back until the window start.
async fn cmd_polymarket_trades(
    market: &str,
    since: Option<&str>,
    until: Option<&str>,
    min_usd: Option<f64>,
    limit: Option<usize>,
    out_path: Option<&std::path::Path>,
) -> Result<()> {
    let market = market.trim();
    if market.is_empty() || !market.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("Polymarket --trades takes the numeric id from finance_odds markets[].ticker (got '{market}')");
    }
    let now = chrono::Utc::now();
    let end = until.map(|u| parse_trades_bound(u, true, now)).transpose()?.unwrap_or(now);
    let start = since
        .map(|s| parse_trades_bound(s, false, now))
        .transpose()?
        .unwrap_or(end - chrono::Duration::hours(24));
    if start >= end {
        anyhow::bail!("--since must be before --until");
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build polymarket client")?;
    let raw = poly_get_json(&client, &format!("{POLYMARKET_GAMMA_API_URL}/markets/{market}"), &[], "polymarket market")
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let condition_id = raw
        .get("conditionId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("polymarket market {market} has no condition id"))?
        .to_string();

    let trades_url = format!("{POLYMARKET_DATA_API_V2_URL}/trades");
    let (start_ts, end_ts) = (start.timestamp(), end.timestamp());
    let mut trades: Vec<PolyTrade> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut truncated = false;
    for page in 0..POLY_TRADES_MAX_PAGES {
        let mut params = vec![
            ("condition", condition_id.clone()),
            ("limit", POLY_TRADES_PAGE.to_string()),
        ];
        if let Some(min) = min_usd {
            params.push(("filter_type", "CASH".to_string()));
            params.push(("filter_amount", min.max(0.0).to_string()));
        }
        if let Some(c) = cursor.as_ref() {
            params.push(("cursor", c.clone()));
        }
        let body = poly_get_json(&client, &trades_url, &params, "polymarket trades")
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let rows = body.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        let mut reached_start = rows.is_empty();
        for row in &rows {
            let Some(t) = poly_trade_from_json(row) else { continue };
            if t.ts < start_ts {
                reached_start = true;
                continue;
            }
            if t.ts <= end_ts {
                trades.push(t);
            }
        }
        cursor = body
            .get("pagination")
            .and_then(|p| p.get("next_cursor"))
            .and_then(|c| c.as_str())
            .map(str::to_string);
        if reached_start || cursor.is_none() {
            break;
        }
        if page + 1 == POLY_TRADES_MAX_PAGES {
            truncated = true;
        }
    }
    trades.sort_by_key(|t| t.ts);
    let payload = poly_trades_payload(market, start, end, &trades, truncated, min_usd, limit.unwrap_or(50).max(1));
    if let Some(path) = out_path {
        std::fs::write(path, serde_json::to_string_pretty(&payload)?)?;
        println!("{}", serde_json::json!({"ok": true, "path": path.display().to_string()}));
        return Ok(());
    }
    let mut payload = payload;
    round_json_floats(&mut payload);
    println!("{}", json_to_stdout_string(&payload)?);
    Ok(())
}

async fn poly_get_json(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, String)],
    label: &str,
) -> std::result::Result<serde_json::Value, String> {
    live_fetch_json::<serde_json::Value>(client, url, query, label).await
}

async fn cmd_polymarket_market_detail(market: &str, out_path: Option<&std::path::Path>) -> Result<()> {
    let market = market.trim();
    if !market.chars().all(|c| c.is_ascii_digit()) || market.is_empty() {
        anyhow::bail!(
            "Polymarket --market takes the numeric id from finance_odds markets[].ticker (got '{market}')"
        );
    }
    let generated_at = chrono::Utc::now();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build polymarket client")?;
    let raw = poly_get_json(
        &client,
        &format!("{POLYMARKET_GAMMA_API_URL}/markets/{market}"),
        &[],
        "polymarket market",
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
    let m: LivePolymarketMarket = serde_json::from_value(raw.clone())
        .map_err(|e| anyhow::anyhow!("polymarket market parse failed: {e}"))?;
    let condition_id = raw
        .get("conditionId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // /markets/{id} omits the parent event; the list form carries it (and
    // needs closed=true to see a closed market).
    let event = match raw
        .get("events")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
    {
        Some(e) => Some(e),
        None => poly_get_json(
            &client,
            &format!("{POLYMARKET_GAMMA_API_URL}/markets"),
            &[
                ("id", market.to_string()),
                ("closed", m.closed.unwrap_or(false).to_string()),
            ],
            "polymarket market event",
        )
        .await
        .ok()
        .and_then(|v| v.as_array().and_then(|a| a.first()).cloned())
        .and_then(|row| row.get("events").and_then(|e| e.as_array()).and_then(|a| a.first()).cloned()),
    };
    let event_open = event
        .as_ref()
        .map(|e| {
            e.get("active").and_then(|v| v.as_bool()).unwrap_or(true)
                && !e.get("closed").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .unwrap_or(true);
    let state = m.state(event_open);
    let outcomes = m.outcomes();
    let prices = m.prices();
    let mut api_errors: Vec<serde_json::Value> = Vec::new();

    let mut out = serde_json::json!({
        "source": "polymarket",
        "ticker": market,
        "title": m.question.as_deref().map(live_normalize_whitespace),
        "event_ticker": event.as_ref().and_then(|e| e.get("slug").cloned()),
        "event_title": event.as_ref().and_then(|e| e.get("title").cloned()),
        "status": match state {
            PolyLegState::Open => "open",
            PolyLegState::Closed => "closed",
            PolyLegState::Resolved => "settled",
            PolyLegState::Placeholder => "inactive",
        },
        "outcomes": outcomes,
        "outcome_prices": prices,
        "probability_yes": live_probability_yes_from_outcomes(&outcomes, &prices),
        "volume_usd": m.lifetime_volume_usd(),
        "volume_24h_usd": m.volume_24h_usd(),
        "close_time": m.end_date.as_deref().and_then(poly_normalize_timestamp),
        "timeseries": poly_timeseries_join(market, &outcomes),
        "generated_at": generated_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    if let Some(label) = m.group_item_title.as_deref().filter(|s| !s.trim().is_empty()) {
        out["outcome"] = serde_json::json!(live_normalize_whitespace(label));
    }
    if state == PolyLegState::Open {
        out["yes_bid"] = serde_json::json!(m.best_bid.as_ref().and_then(live_parse_decimal_value));
        out["yes_ask"] = serde_json::json!(m.best_ask.as_ref().and_then(live_parse_decimal_value));
        out["last_trade_price"] =
            serde_json::json!(m.last_trade_price.as_ref().and_then(live_parse_decimal_value));
        out["liquidity_usd"] =
            serde_json::json!(m.liquidity_num.as_ref().and_then(live_parse_decimal_value));
        for (key, field) in [
            ("chg_1h_pp", "oneHourPriceChange"),
            ("chg_24h_pp", "oneDayPriceChange"),
            ("chg_1w_pp", "oneWeekPriceChange"),
            ("chg_1mo_pp", "oneMonthPriceChange"),
        ] {
            if let Some(chg) = raw.get(field).and_then(live_parse_decimal_value) {
                out[key] = serde_json::json!((chg * 1000.0).round() / 10.0);
            }
        }
    }
    if state == PolyLegState::Resolved {
        out["result"] = serde_json::json!(poly_resolved_outcome(&outcomes, &prices));
    }
    if let Some(at) = m.closed_time.as_deref().and_then(poly_normalize_timestamp) {
        out[if state == PolyLegState::Resolved { "settled_at" } else { "closed_at" }] =
            serde_json::json!(at);
    }
    if let Some(rules) = raw.get("description").and_then(|v| v.as_str()) {
        let rules = live_normalize_whitespace(rules);
        let clipped: String = rules.chars().take(600).collect();
        out["rules"] = serde_json::json!(if clipped.len() < rules.len() {
            format!("{clipped}...")
        } else {
            clipped
        });
    }

    // Open interest (priced gross USD, keyed on the on-chain condition id);
    // skipped once settled, when it only counts unredeemed payouts.
    if !condition_id.is_empty() {
        let oi_url = format!("{POLYMARKET_DATA_API_V2_URL}/oi");
        let oi = if state == PolyLegState::Resolved {
            Ok(serde_json::Value::Null)
        } else {
            poly_get_json(&client, &oi_url, &[("condition", condition_id.clone())], "polymarket open interest").await
        };
        match oi {
            Ok(v) => {
                if let Some(value) = v
                    .get("data")
                    .and_then(|d| d.as_array())
                    .and_then(|a| a.first())
                    .and_then(|r| r.get("value"))
                    .and_then(live_parse_decimal_value)
                {
                    out["open_interest_usd"] = serde_json::json!((value * 100.0).round() / 100.0);
                }
            }
            Err(error) => api_errors.push(serde_json::json!({"phase": "open_interest", "error": error})),
        }
    }

    // Sibling legs of a multi-outcome event, so one call shows the whole
    // distribution with joinable tickers.
    if let Some(event_id) = event
        .as_ref()
        .and_then(|e| e.get("id"))
        .map(live_json_value_to_string)
        .filter(|id| !id.is_empty())
    {
        let ev_url = format!("{POLYMARKET_GAMMA_API_URL}/events/{event_id}");
        match poly_get_json(&client, &ev_url, &[], "polymarket event").await {
            Ok(v) => {
                if let Ok(ev) = serde_json::from_value::<LivePolymarketEvent>(v) {
                    if ev.markets.len() > 1 {
                        let legs: Vec<&LivePolymarketMarket> = ev
                            .markets
                            .iter()
                            .filter(|leg| leg.state(event_open) != PolyLegState::Placeholder)
                            .collect();
                        let ev_ticker = ev.slug.clone().unwrap_or_else(|| event_id.clone());
                        let ev_title = ev.title.clone().unwrap_or_default();
                        let row = poly_event_group_row(
                            &ev,
                            &ev_ticker,
                            &ev_title,
                            "",
                            0,
                            &legs,
                            KalshiSearchStatus::All,
                            event_open,
                        );
                        for key in ["legs", "legs_omitted", "mutually_exclusive", "open_interest_usd"] {
                            if key == "open_interest_usd" && state == PolyLegState::Resolved {
                                continue;
                            }
                            if let Some(val) = row.get(key) {
                                let out_key = if key == "open_interest_usd" { "event_open_interest_usd" } else { key };
                                out[out_key] = val.clone();
                            }
                        }
                    }
                }
            }
            Err(error) => api_errors.push(serde_json::json!({"phase": "event_legs", "error": error})),
        }
    }
    if !api_errors.is_empty() {
        out["api_errors"] = serde_json::json!(api_errors);
    }
    if let Some(obj) = out.as_object_mut() {
        obj.retain(|_, v| !v.is_null());
    }

    if let Some(path) = out_path {
        std::fs::write(path, serde_json::to_string_pretty(&out)?)?;
        println!("{}", serde_json::json!({"ok": true, "path": path.display().to_string()}));
        return Ok(());
    }
    round_json_floats(&mut out);
    println!("{}", json_to_stdout_string(&out)?);
    Ok(())
}

#[cfg(test)]
mod odds_polymarket_tests {
    use super::*;

    fn leg(id: &str, label: &str, prices: &str, active: bool, closed: bool, uma: Option<&str>, vol: f64) -> LivePolymarketMarket {
        LivePolymarketMarket {
            id: serde_json::json!(id),
            question: Some(format!("Will {label} win?")),
            outcomes: serde_json::json!("[\"Yes\", \"No\"]"),
            outcome_prices: if prices.is_empty() { serde_json::Value::Null } else { serde_json::json!(prices) },
            volume_num: Some(serde_json::json!(vol)),
            volume_24hr: Some(serde_json::json!("12.5")),
            active: Some(active),
            closed: Some(closed),
            group_item_title: Some(label.to_string()),
            end_date: Some("2026-12-10T19:00:00Z".to_string()),
            uma_resolution_status: uma.map(str::to_string),
            ..Default::default()
        }
    }

    fn event(neg_risk: bool, markets: Vec<LivePolymarketMarket>) -> LivePolymarketEvent {
        serde_json::from_value::<LivePolymarketEvent>(serde_json::json!({
            "id": "770450", "slug": "fed-decision-in-december", "title": "Fed Decision in December?",
            "volume": "11242541.5", "openInterest": 2500000.0, "negRisk": neg_risk,
        }))
        .map(|mut e| {
            e.markets = markets;
            e
        })
        .unwrap()
    }

    #[test]
    fn resolved_outcome_needs_a_clean_one_zero_payout() {
        let yn = vec!["Yes".to_string(), "No".to_string()];
        assert_eq!(poly_resolved_outcome(&yn, &["1".into(), "0".into()]), Some("yes".to_string()));
        assert_eq!(poly_resolved_outcome(&yn, &["0".into(), "1".into()]), Some("no".to_string()));
        // Split/voided payouts and live prices have no single winner.
        assert_eq!(poly_resolved_outcome(&yn, &["0.5".into(), "0.5".into()]), None);
        assert_eq!(poly_resolved_outcome(&yn, &["0.995".into(), "0.005".into()]), None);
        let teams = vec!["Lakers".to_string(), "Celtics".to_string()];
        assert_eq!(poly_resolved_outcome(&teams, &["0".into(), "1".into()]), Some("Celtics".to_string()));
    }

    #[test]
    fn leg_state_separates_placeholders_closed_and_resolved() {
        assert_eq!(leg("1", "A", "[\"0.7\",\"0.3\"]", true, false, None, 10.0).state(true), PolyLegState::Open);
        assert_eq!(leg("2", "Party C", "", false, false, None, 0.0).state(true), PolyLegState::Placeholder);
        assert_eq!(leg("3", "B", "[\"0.7\",\"0.3\"]", false, false, None, 0.0).state(true), PolyLegState::Placeholder);
        assert_eq!(leg("4", "C", "[\"1\",\"0\"]", false, true, Some("resolved"), 5.0).state(false), PolyLegState::Resolved);
        // Closed but not yet resolved: last prices are not a payout.
        assert_eq!(leg("5", "D", "[\"1\",\"0\"]", false, true, Some("proposed"), 5.0).state(false), PolyLegState::Closed);
        assert!(poly_leg_matches_status(PolyLegState::Resolved, KalshiSearchStatus::Settled));
        assert!(!poly_leg_matches_status(PolyLegState::Closed, KalshiSearchStatus::Settled));
        assert!(!poly_leg_matches_status(PolyLegState::Placeholder, KalshiSearchStatus::All));
    }

    #[test]
    fn volume_is_lifetime_not_24h() {
        let m = leg("1", "A", "[\"0.7\",\"0.3\"]", true, false, None, 10_328_180.9);
        assert_eq!(m.lifetime_volume_usd(), 10_328_180.9);
        assert_eq!(m.volume_24h_usd(), 12.5);
    }

    #[test]
    fn combo_detection_uses_tags_and_titles() {
        assert!(poly_event_is_combo("Senate Combo: Ohio and Texas", &[]));
        assert!(poly_event_is_combo("Who wins the Senate pending oil above or below 80?", &[]));
        assert!(poly_event_is_combo("Dem sweep & US recession by end of 2027?", &[]));
        assert!(poly_event_is_combo("Republican trifecta", &["parlays".to_string()]));
        assert!(!poly_event_is_combo("Ohio Senate Election Winner", &["ohio-senate".to_string()]));
        assert!(poly_market_title_is_combo(
            "Will Republicans lose the Alaska, Iowa, Kansas, Maine, Michigan, Ohio, and Texas Senate seats?"
        ));
        assert!(!poly_market_title_is_combo("Will the Democrats win the Ohio Senate race in 2026?"));
        assert!(!poly_market_title_is_combo("Will Democrats sweep the Ohio Senate and Governor elections?"));
        assert!(poly_query_wants_combo(&live_query_terms("senate parlay")));
        assert!(!poly_query_wants_combo(&live_query_terms("ohio senate")));
    }

    #[test]
    fn event_group_row_sorts_exclusive_legs_by_probability() {
        let ev = event(
            true,
            vec![
                leg("3215008", "25 bps increase", "[\"0.73\",\"0.27\"]", true, false, None, 200.0),
                leg("3215009", "No change", "[\"0.25\",\"0.75\"]", true, false, None, 100.0),
                leg("3215010", "25 bps decrease", "[\"0.02\",\"0.98\"]", true, false, None, 50.0),
            ],
        );
        let legs: Vec<&LivePolymarketMarket> = ev.markets.iter().collect();
        let row = poly_event_group_row(&ev, "fed-decision-in-december", "Fed Decision in December?", "", 10, &legs, KalshiSearchStatus::Open, true);
        assert_eq!(row["mutually_exclusive"], true);
        assert_eq!(row["volume_usd"], 11242541.5);
        assert_eq!(row["open_interest_usd"], 2500000.0);
        assert_eq!(row["close_time"], "2026-12-10T19:00:00Z");
        let order: Vec<&str> = row["legs"].as_array().unwrap().iter().map(|l| l["ticker"].as_str().unwrap()).collect();
        assert_eq!(order, ["3215008", "3215009", "3215010"]);
        assert_eq!(row["legs"][1]["outcome"], "No change");
        assert!(row.get("legs_omitted").is_none());
    }

    #[test]
    fn event_group_row_marks_results_on_settled_legs() {
        let ev = event(
            true,
            vec![
                leg("1", "25 bps increase", "[\"1\",\"0\"]", false, true, Some("resolved"), 5.0),
                leg("2", "No change", "[\"0\",\"1\"]", false, true, Some("resolved"), 5.0),
            ],
        );
        let legs: Vec<&LivePolymarketMarket> = ev.markets.iter().collect();
        let row = poly_event_group_row(&ev, "e", "E", "", 0, &legs, KalshiSearchStatus::Settled, false);
        assert_eq!(row["legs"][0]["result"], "yes");
        assert_eq!(row["legs"][1]["result"], "no");
        assert_eq!(row["status_filter"], "settled");
    }

    #[test]
    fn timestamps_normalize_to_rfc3339() {
        assert_eq!(
            poly_normalize_timestamp("2025-09-17 21:27:38+00").as_deref(),
            Some("2025-09-17T21:27:38Z")
        );
        assert_eq!(
            poly_normalize_timestamp("2026-11-04T04:59:00Z").as_deref(),
            Some("2026-11-04T04:59:00Z")
        );
        assert_eq!(poly_normalize_timestamp(""), None);
    }

    #[test]
    fn timeseries_join_names_each_outcome() {
        let join = poly_timeseries_join("562793", &["Yes".to_string(), "No".to_string()]);
        assert_eq!(join["Yes"], "562793");
        assert_eq!(join["No"], "562793:NO");
        let join = poly_timeseries_join("1", &["Over".to_string(), "Under".to_string()]);
        assert_eq!(join["Under"], "1:NO");
    }

    #[test]
    fn trades_project_onto_yes_price_and_taker_side() {
        let t = |side: &str, idx: i64, price: f64| {
            poly_trade_from_json(&serde_json::json!({
                "timestamp": 1793726802, "side": side, "outcome_index": idx, "price": price,
                "size": 20000.0, "proxy_wallet": "0xabcdef0123456789",
            }))
            .unwrap()
        };
        let buy_yes = t("BUY", 0, 0.74);
        assert_eq!((buy_yes.taker, buy_yes.yes_price, buy_yes.usd), ("yes", 0.74, 14800.0));
        assert_eq!(buy_yes.wallet, "0xabcdef01");
        let buy_no = t("BUY", 1, 0.25);
        assert_eq!((buy_no.taker, buy_no.yes_price), ("no", 0.75));
        assert_eq!(t("SELL", 0, 0.74).taker, "no");
        assert_eq!(t("SELL", 1, 0.25).taker, "yes");
        // Unlabeled outcome (999) is skipped, not guessed.
        assert!(poly_trade_from_json(&serde_json::json!({
            "timestamp": 1, "side": "BUY", "outcome_index": 999, "price": 0.5, "size": 1.0
        }))
        .is_none());
    }

    #[test]
    fn trades_payload_keeps_largest_prints_in_time_order() {
        let mk = |ts: i64, usd: f64, taker: &'static str| PolyTrade {
            ts,
            yes_price: 0.5,
            shares: usd * 2.0,
            usd,
            taker,
            wallet: "0x1".to_string(),
        };
        let trades = vec![mk(100, 10.0, "yes"), mk(200, 5000.0, "no"), mk(300, 900.0, "yes")];
        let start = chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap();
        let end = chrono::DateTime::<chrono::Utc>::from_timestamp(400, 0).unwrap();
        let out = poly_trades_payload("1", start, end, &trades, false, None, 2);
        assert_eq!(out["_summary"]["trades"], 3);
        assert_eq!(out["_summary"]["yes_taker_usd"], 910.0);
        assert_eq!(out["_summary"]["no_taker_usd"], 5000.0);
        let ts: Vec<&str> = out["trades"].as_array().unwrap().iter().map(|r| r["t"].as_str().unwrap()).collect();
        assert_eq!(ts, ["1970-01-01T00:03:20Z", "1970-01-01T00:05:00Z"]);
        assert!(out["trades_note"].as_str().unwrap().starts_with("2 of 3"));
    }
}
