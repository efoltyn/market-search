// Kalshi live search.
//
// Discovery rides Kalshi's own search index (the public v1 endpoint behind
// kalshi.com search: relevance-ranked EVENTS with every leg nested), so a
// query finds live markets that were never synced locally (the local FTS
// catalog held ~5k Kalshi rows, mostly BTC strikes, and "senate" found 0).
// Prices, volume, open interest and settlement fields then come from the v2
// /events endpoint (one call for every candidate event): the v1 index counts
// volume on a different basis, so its numbers are used for ranking only.
// Legs settled before Kalshi's historical cutoff are not nested in v2 events;
// they come from /historical/markets.

const KALSHI_V1_SEARCH_URL: &str = "https://api.elections.kalshi.com/v1/search/series";
const KALSHI_V2_BASE_URL: &str = "https://api.elections.kalshi.com/trade-api/v2";
/// v1 page size. Relevance alone buries deep markets ("oil": the 10M-volume
/// WTI yearly max sits at rank ~25 behind 15-minute and state gas markets),
/// so take a wide page and re-rank with depth before hydrating.
const KALSHI_SEARCH_PAGE_SIZE: usize = 40;
/// Candidate events hydrated through v2 /events (one request).
const KALSHI_SEARCH_HYDRATE_EVENTS: usize = 15;
/// Settled events whose legs need /historical/markets (one request each).
const KALSHI_SEARCH_HISTORICAL_EVENTS: usize = 6;
/// Legs shown per grouped event row; the rest are counted in legs_omitted.
const KALSHI_EVENT_ROW_LEGS: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KalshiSearchStatus {
    Open,
    Closed,
    Settled,
    All,
}

impl KalshiSearchStatus {
    fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(raw) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::Open);
        };
        match raw.to_ascii_lowercase().as_str() {
            "open" | "active" | "live" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "settled" | "resolved" | "finalized" | "determined" => Ok(Self::Settled),
            "all" | "any" => Ok(Self::All),
            other => anyhow::bail!(
                "unsupported --status '{other}' for search (supported: open, closed, settled, all)"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Settled => "settled",
            Self::All => "all",
        }
    }

    /// Status value the v1 search index accepts (None = every status).
    fn v1_param(self) -> Option<&'static str> {
        match self {
            Self::Open => Some("open"),
            Self::Closed => Some("closed"),
            Self::Settled => Some("settled"),
            Self::All => None,
        }
    }

    /// Does a v2 market status belong to this filter? v2 statuses: initialized
    /// (unopened), active, inactive (paused), closed, determined, finalized.
    fn leg_matches(self, status: Option<&str>) -> bool {
        let s = status.unwrap_or_default().trim().to_ascii_lowercase();
        match self {
            Self::Open => matches!(s.as_str(), "active" | "open"),
            Self::Closed => s == "closed",
            Self::Settled => matches!(s.as_str(), "determined" | "finalized" | "settled"),
            Self::All => true,
        }
    }
}

/// Parse a "closing soon" window: 30m, 6h, 2d, 1w.
fn parse_closing_within(raw: &str) -> Result<chrono::Duration> {
    let raw = raw.trim().to_ascii_lowercase();
    let split = raw
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| anyhow::anyhow!("--closing-within needs a unit: 30m, 6h, 2d, 1w"))?;
    let (num, unit) = raw.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("--closing-within '{raw}': expected e.g. 30m, 6h, 2d, 1w"))?;
    if n <= 0 {
        anyhow::bail!("--closing-within must be positive");
    }
    match unit {
        "m" | "min" => Ok(chrono::Duration::minutes(n)),
        "h" => Ok(chrono::Duration::hours(n)),
        "d" => Ok(chrono::Duration::days(n)),
        "w" => Ok(chrono::Duration::weeks(n)),
        other => anyhow::bail!("--closing-within unit '{other}' (use m, h, d, w)"),
    }
}

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiV1SearchResp {
    #[serde(default)]
    current_page: Vec<KalshiV1Event>,
}

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiV1Event {
    event_ticker: String,
    #[serde(default)]
    event_title: Option<String>,
    #[serde(default)]
    event_subtitle: Option<String>,
    #[serde(default)]
    series_title: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    total_volume: Option<f64>,
    #[serde(default)]
    product_metadata: Option<serde_json::Value>,
}

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiV2EventsResp {
    #[serde(default)]
    events: Vec<KalshiV2Event>,
}

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiV2Event {
    event_ticker: String,
    #[serde(default)]
    series_ticker: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    sub_title: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    mutually_exclusive: Option<bool>,
    #[serde(default)]
    markets: Vec<KalshiV2Market>,
}

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiV2MarketsResp {
    #[serde(default)]
    markets: Vec<KalshiV2Market>,
}

#[derive(Clone, Default, serde::Deserialize)]
struct KalshiV2Market {
    ticker: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    close_time: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    yes_sub_title: Option<String>,
    #[serde(default)]
    last_price_dollars: Option<String>,
    #[serde(default)]
    previous_price_dollars: Option<String>,
    #[serde(default)]
    yes_bid_dollars: Option<String>,
    #[serde(default)]
    yes_ask_dollars: Option<String>,
    #[serde(default)]
    volume_fp: Option<String>,
    #[serde(default)]
    volume_24h_fp: Option<String>,
    #[serde(default)]
    open_interest_fp: Option<String>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    settlement_ts: Option<String>,
    #[serde(default)]
    settlement_value_dollars: Option<String>,
}

impl KalshiV2Market {
    fn volume_usd(&self) -> f64 {
        live_parse_decimal(self.volume_fp.as_deref()).unwrap_or(0.0)
    }

    /// Two-sided mid while the book is live; last trade otherwise. Settled
    /// legs report the settlement value (1 or 0 for binaries).
    fn probability_yes(&self, settled: bool) -> Option<f64> {
        if settled {
            if let Some(v) = live_parse_decimal(self.settlement_value_dollars.as_deref()) {
                return Some(v.clamp(0.0, 1.0));
            }
            match self.result.as_deref().map(str::trim) {
                Some("yes") => return Some(1.0),
                Some("no") => return Some(0.0),
                _ => {}
            }
        }
        let bid = live_parse_decimal(self.yes_bid_dollars.as_deref());
        let ask = live_parse_decimal(self.yes_ask_dollars.as_deref());
        if let (Some(b), Some(a)) = (bid, ask) {
            // A one-sided or empty book (bid 0 / ask 1) is not a price.
            if b > 0.0 && a < 1.0 && a >= b {
                return Some(((b + a) / 2.0).clamp(0.0, 1.0));
            }
        }
        live_parse_decimal(self.last_price_dollars.as_deref())
            .filter(|p| *p > 0.0)
            .or(bid.filter(|b| *b > 0.0))
    }

    /// Last-trade change versus Kalshi's own day-ago last trade, in points.
    /// Only reported when both prints exist and differ.
    fn change_24h_pp(&self) -> Option<f64> {
        let last = live_parse_decimal(self.last_price_dollars.as_deref())?;
        let prev = live_parse_decimal(self.previous_price_dollars.as_deref())?;
        if last <= 0.0 || prev <= 0.0 {
            return None;
        }
        let pp = ((last - prev) * 1000.0).round() / 10.0;
        (pp != 0.0).then_some(pp)
    }
}

/// One candidate event between discovery and hydration.
#[derive(Clone, Debug, Default)]
struct KalshiSearchCandidate {
    event_ticker: String,
    /// Position in Kalshi's own relevance order (0 = best).
    rank: usize,
    /// Text Kalshi matched the query against (event + series titles, tags).
    match_text: String,
    /// v1 index volume, ranking only (different basis from v2 volume_fp).
    index_volume: f64,
    frequency: Option<String>,
    category: Option<String>,
}

fn kalshi_frequency_penalty(frequency: Option<&str>) -> i64 {
    // 15-minute and hourly ladders match every price query ("oil", "bitcoin")
    // and expire before anyone can research them; keep them findable but
    // below the daily-and-longer markets.
    match frequency.unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "fifteen_min" => -40,
        "hourly" => -25,
        _ => 0,
    }
}

/// Discovery score: Kalshi's relevance rank, query terms in the event/series
/// text, and depth. Used to pick which events to hydrate and to order them.
fn kalshi_candidate_score(
    candidate: &KalshiSearchCandidate,
    query_phrase: &str,
    query_terms: &[String],
) -> i64 {
    // Kalshi's rank breaks ties; depth decides between on-topic events.
    let rank_bonus = (30_i64 - candidate.rank as i64).max(0) / 2;
    let text_score =
        live_scored_hits(&candidate.match_text, query_phrase, query_terms, 12, 6, 18).0;
    let depth = if candidate.index_volume > 0.0 {
        ((candidate.index_volume + 1.0).log10() * 8.0).round() as i64
    } else {
        0
    };
    rank_bonus + text_score + depth + kalshi_frequency_penalty(candidate.frequency.as_deref())
}

/// A query that is itself a Kalshi id (market, event or series ticker):
/// no spaces, ticker charset, and either a dash or the KX prefix.
fn kalshi_query_as_ticker(query: &str) -> Option<String> {
    let q = query.trim();
    if q.len() < 4 || q.chars().any(char::is_whitespace) {
        return None;
    }
    if !q
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
    {
        return None;
    }
    let upper = q.to_ascii_uppercase();
    if upper.contains('-') || upper.starts_with("KX") {
        Some(upper)
    } else {
        None
    }
}

fn kalshi_v1_candidates(resp: KalshiV1SearchResp, include_mentions: bool) -> Vec<KalshiSearchCandidate> {
    let mut out = Vec::new();
    for (rank, event) in resp.current_page.into_iter().enumerate() {
        let category = event.category.clone().filter(|c| !c.trim().is_empty());
        if !include_mentions
            && (is_mention_event_ticker(&event.event_ticker)
                || category.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("mentions")))
        {
            continue;
        }
        let meta = event.product_metadata.as_ref();
        let frequency = meta
            .and_then(|m| m.get("series_frequency"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let tags = meta
            .and_then(|m| m.get("series_tags"))
            .and_then(|v| v.as_array())
            .map(|tags| {
                tags.iter()
                    .filter_map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let match_text = [
            event.event_title.as_deref().unwrap_or_default(),
            event.event_subtitle.as_deref().unwrap_or_default(),
            event.series_title.as_deref().unwrap_or_default(),
            tags.as_str(),
        ]
        .join(" ");
        out.push(KalshiSearchCandidate {
            event_ticker: event.event_ticker,
            rank,
            match_text: live_normalize_whitespace(&match_text),
            index_volume: event.total_volume.unwrap_or(0.0),
            frequency,
            category,
        });
    }
    out
}

/// GET returning Ok(None) on 404. Spacing and 429 backoff come from
/// venue_send (Kalshi sends no Retry-After; the docs say back off); a
/// server error is retried here.
async fn kalshi_get_json_opt(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, String)],
    label: &str,
) -> std::result::Result<Option<serde_json::Value>, String> {
    let mut attempt = 0usize;
    loop {
        let resp = client
            .get(url)
            .query(query)
            .venue_send()
            .await
            .map_err(|e| format!("{label} failed: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<serde_json::Value>()
                .await
                .map(Some)
                .map_err(|e| format!("{label} parse failed: {e}"));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if let Some(note) = eli_core::finance::venue_http::refusal_note(&resp) {
            return Err(format!("{label} failed: {note}"));
        }
        if status.is_server_error() && attempt < 4 {
            attempt += 1;
            let backoff_ms = 400 * (1u64 << attempt);
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            continue;
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "{label} failed: http {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
}

/// Resolve a ticker-shaped query to event tickers: market → its event,
/// event → itself, series → its events (status-filtered), then the
/// historical archive for markets settled before the cutoff.
async fn kalshi_resolve_ticker_query(
    client: &reqwest::Client,
    ticker: &str,
    status: KalshiSearchStatus,
) -> Vec<String> {
    let enc = |t: &str| t.replace('/', "");
    let market_url = format!("{KALSHI_V2_BASE_URL}/markets/{}", enc(ticker));
    if let Ok(Some(v)) = kalshi_get_json_opt(client, &market_url, &[], "kalshi market").await {
        if let Some(ev) = v.pointer("/market/event_ticker").and_then(|e| e.as_str()) {
            return vec![ev.to_string()];
        }
    }
    let event_url = format!("{KALSHI_V2_BASE_URL}/events/{}", enc(ticker));
    let event_query = vec![("exclude_markets", "true".to_string())];
    if let Ok(Some(v)) = kalshi_get_json_opt(client, &event_url, &event_query, "kalshi event").await {
        if let Some(ev) = v.pointer("/event/event_ticker").and_then(|e| e.as_str()) {
            return vec![ev.to_string()];
        }
    }
    let mut series_query = vec![
        ("series_ticker", ticker.to_string()),
        ("limit", "50".to_string()),
    ];
    if let Some(s) = status.v1_param() {
        series_query.push(("status", s.to_string()));
    }
    let events_url = format!("{KALSHI_V2_BASE_URL}/events");
    if let Ok(Some(v)) =
        kalshi_get_json_opt(client, &events_url, &series_query, "kalshi series events").await
    {
        let events: Vec<String> = v
            .get("events")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("event_ticker").and_then(|t| t.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !events.is_empty() {
            return events;
        }
    }
    let hist_url = format!("{KALSHI_V2_BASE_URL}/historical/markets/{}", enc(ticker));
    if let Ok(Some(v)) = kalshi_get_json_opt(client, &hist_url, &[], "kalshi historical market").await {
        if let Some(ev) = v.pointer("/market/event_ticker").and_then(|e| e.as_str()) {
            return vec![ev.to_string()];
        }
    }
    Vec::new()
}

/// Everything the Kalshi side contributes to a live search response.
#[derive(Default)]
struct KalshiSearchOutput {
    events: Vec<serde_json::Value>,
    markets: Vec<serde_json::Value>,
    errors: Vec<serde_json::Value>,
}

/// Kalshi discovery + hydration for a search query. Err only when discovery
/// itself failed (the caller then falls back to the local-catalog path).
async fn kalshi_live_search(
    client: &reqwest::Client,
    query: &str,
    query_phrase: &str,
    query_terms: &[String],
    opts: &CsvSearchOptions,
    profile_applied: SearchProfile,
    delta_lookup: Option<&SyncDeltaLookup>,
) -> std::result::Result<KalshiSearchOutput, String> {
    let status = opts.status_filter;
    let mut out = KalshiSearchOutput::default();

    // 1. Discovery: exact Kalshi ids first, then Kalshi's search index.
    let mut candidates: Vec<KalshiSearchCandidate> = Vec::new();
    if let Some(ticker) = kalshi_query_as_ticker(query) {
        for (rank, event_ticker) in kalshi_resolve_ticker_query(client, &ticker, status)
            .await
            .into_iter()
            .enumerate()
        {
            candidates.push(KalshiSearchCandidate {
                event_ticker,
                rank,
                ..Default::default()
            });
        }
    }
    let exact_ticker_hit = !candidates.is_empty();
    // A ticker the caller already holds is shown whatever its status (a
    // round trip on a settled leg returns its settlement, not nothing).
    let status = if exact_ticker_hit && status == KalshiSearchStatus::Open {
        KalshiSearchStatus::All
    } else {
        status
    };
    if !exact_ticker_hit {
        let mut q = vec![
            ("query", query.to_string()),
            ("page_size", KALSHI_SEARCH_PAGE_SIZE.to_string()),
        ];
        if let Some(s) = status.v1_param() {
            q.push(("status", s.to_string()));
        }
        let resp: KalshiV1SearchResp =
            live_fetch_json(client, KALSHI_V1_SEARCH_URL, &q, "kalshi search").await?;
        candidates = kalshi_v1_candidates(resp, opts.include_mentions);
    }
    candidates.retain(|c| {
        c.category
            .as_deref()
            .map(|cat| category_matches_filter(cat, opts.category_filter.as_deref()))
            .unwrap_or(true)
    });
    if !exact_ticker_hit {
        candidates.sort_by_key(|c| std::cmp::Reverse(kalshi_candidate_score(c, query_phrase, query_terms)));
    }
    candidates.truncate(KALSHI_SEARCH_HYDRATE_EVENTS);
    if candidates.is_empty() {
        return Ok(out);
    }

    // 2. Hydrate every candidate in one v2 call.
    let tickers: Vec<String> = candidates.iter().map(|c| c.event_ticker.clone()).collect();
    let events_url = format!("{KALSHI_V2_BASE_URL}/events");
    let q = vec![
        ("tickers", tickers.join(",")),
        ("with_nested_markets", "true".to_string()),
        ("limit", "200".to_string()),
    ];
    let hydrated: KalshiV2EventsResp =
        match live_fetch_json(client, &events_url, &q, "kalshi events").await {
            Ok(resp) => resp,
            Err(error) => {
                out.errors.push(serde_json::json!({
                    "phase": "kalshi_event_hydration",
                    "source": "kalshi",
                    "error": error,
                }));
                return Ok(out);
            }
        };
    let mut by_ticker: std::collections::HashMap<String, KalshiV2Event> = hydrated
        .events
        .into_iter()
        .map(|e| (e.event_ticker.clone(), e))
        .collect();

    // 3. Settled legs older than the cutoff live only in the archive.
    if status != KalshiSearchStatus::Open {
        let mut historical_fetches = 0usize;
        for candidate in &candidates {
            if historical_fetches >= KALSHI_SEARCH_HISTORICAL_EVENTS {
                break;
            }
            let Some(event) = by_ticker.get_mut(&candidate.event_ticker) else {
                continue;
            };
            if event.markets.iter().any(|m| status.leg_matches(m.status.as_deref())) {
                continue;
            }
            historical_fetches += 1;
            let url = format!("{KALSHI_V2_BASE_URL}/historical/markets");
            let q = vec![
                ("event_ticker", event.event_ticker.clone()),
                ("limit", "200".to_string()),
            ];
            match live_fetch_json::<KalshiV2MarketsResp>(client, &url, &q, "kalshi historical markets").await {
                Ok(resp) => event.markets.extend(resp.markets),
                Err(error) => out.errors.push(serde_json::json!({
                    "phase": "kalshi_historical_markets",
                    "source": "kalshi",
                    "event_ticker": event.event_ticker,
                    "error": error,
                })),
            }
        }
    }

    // 4. Rows: one grouped event row per event, one market row per leg.
    let now = chrono::Utc::now();
    let close_cutoff = opts.closing_within.map(|d| now + d);
    for candidate in &candidates {
        let Some(event) = by_ticker.remove(&candidate.event_ticker) else {
            continue;
        };
        let event_score = if exact_ticker_hit {
            200 - candidate.rank as i64
        } else {
            kalshi_candidate_score(candidate, query_phrase, query_terms)
        };
        let (event_row, mut market_rows) = kalshi_event_rows(
            event,
            candidate,
            event_score,
            status,
            close_cutoff,
            query_phrase,
            query_terms,
            opts,
            profile_applied,
            delta_lookup,
        );
        if market_rows.is_empty() {
            continue;
        }
        out.events.push(event_row);
        out.markets.append(&mut market_rows);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn kalshi_event_rows(
    event: KalshiV2Event,
    candidate: &KalshiSearchCandidate,
    event_score: i64,
    status: KalshiSearchStatus,
    close_cutoff: Option<chrono::DateTime<chrono::Utc>>,
    query_phrase: &str,
    query_terms: &[String],
    opts: &CsvSearchOptions,
    profile_applied: SearchProfile,
    delta_lookup: Option<&SyncDeltaLookup>,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let now = chrono::Utc::now();
    let category = event
        .category
        .clone()
        .or_else(|| candidate.category.clone())
        .unwrap_or_default();
    let series_ticker = event
        .series_ticker
        .clone()
        .unwrap_or_else(|| event.event_ticker.split('-').next().unwrap_or_default().to_string());
    let sub_title = event.sub_title.clone().unwrap_or_default();
    let event_title = if kalshi_subtitle_is_redundant(&event.title, &sub_title) {
        event.title.clone()
    } else {
        format!("{} ({})", event.title.trim(), sub_title.trim())
    };

    let mut legs: Vec<&KalshiV2Market> = event
        .markets
        .iter()
        .filter(|m| status.leg_matches(m.status.as_deref()))
        .filter(|m| match close_cutoff {
            None => true,
            Some(cutoff) => m
                .close_time
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
                .is_some_and(|t| t >= now && t <= cutoff),
        })
        .collect();
    // Dedup: the archive and the live nest can both carry a leg.
    let mut seen = std::collections::HashSet::new();
    legs.retain(|m| seen.insert(m.ticker.clone()));

    let mut market_rows = Vec::new();
    for market in &legs {
        let settled = KalshiSearchStatus::Settled.leg_matches(market.status.as_deref());
        let volume_usd = market.volume_usd();
        let volume_24h_usd = live_parse_decimal(market.volume_24h_fp.as_deref()).unwrap_or(0.0);
        let open_interest = live_parse_decimal(market.open_interest_fp.as_deref()).unwrap_or(0.0);
        let probability_yes = market.probability_yes(settled);
        let display_title = live_kalshi_market_display_title(
            &event_title,
            &market.title,
            market.yes_sub_title.as_deref(),
            market.subtitle.as_deref(),
        );
        let (match_score, matched_terms) = live_market_score(
            &display_title,
            &event_title,
            &category,
            &candidate.match_text,
            query_phrase,
            query_terms,
            &[],
            volume_usd,
            volume_24h_usd,
            open_interest,
            market.close_time.as_deref(),
            market.status.as_deref(),
            probability_yes,
            event_score + kalshi_frequency_penalty(candidate.frequency.as_deref()),
            profile_applied,
            &opts.policy,
        );
        let mut row = serde_json::json!({
            "source": "kalshi",
            "ticker": market.ticker,
            "title": display_title,
            "event_ticker": event.event_ticker,
            "event_title": event_title,
            "yes_price": probability_yes,
            "yes_bid": live_parse_decimal(market.yes_bid_dollars.as_deref()),
            "yes_ask": live_parse_decimal(market.yes_ask_dollars.as_deref()),
            "volume": live_volume_to_cents(volume_usd),
            "volume_usd": volume_usd,
            "volume_24h_usd": volume_24h_usd,
            "open_interest": open_interest,
            "close_time": market.close_time,
            "status": market.status,
            "probability_yes": probability_yes,
            "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
            "series_ticker": series_ticker,
            "match_score": match_score,
            "match_terms": matched_terms,
            "score_components": {
                "kalshi_search_rank": candidate.rank,
                "event_score": event_score,
                "volume_usd": volume_usd,
                "volume_24h_usd": volume_24h_usd,
                "open_interest": open_interest,
            },
        });
        if let Some(pp) = market.change_24h_pp().filter(|_| !settled) {
            row["chg_24h_pp"] = serde_json::json!(pp);
        }
        if settled {
            if let Some(result) = market.result.as_deref().filter(|r| !r.trim().is_empty()) {
                row["result"] = serde_json::json!(result);
            }
            if let Some(ts) = market.settlement_ts.as_deref().filter(|t| !t.trim().is_empty()) {
                row["settled_at"] = serde_json::json!(ts);
            }
        }
        attach_market_delta(&mut row, "kalshi", &market.ticker, delta_lookup);
        market_rows.push(row);
    }

    let event_row = kalshi_event_group_row(
        &event,
        &event_title,
        &category,
        &series_ticker,
        event_score,
        &legs,
        status,
    );
    (event_row, market_rows)
}

/// Kalshi event subtitles mostly carry the date ("In 2026", "On Nov 3,
/// 2026"), which disambiguates recurring events; skip it when empty, already
/// in the title, or when every number in it is already in the title.
fn kalshi_subtitle_is_redundant(title: &str, sub_title: &str) -> bool {
    let sub = sub_title.trim();
    if sub.is_empty() || title.to_ascii_lowercase().contains(&sub.to_ascii_lowercase()) {
        return true;
    }
    let numbers: Vec<&str> = sub
        .split(|c: char| !c.is_ascii_digit())
        .filter(|n| !n.is_empty())
        .collect();
    !numbers.is_empty() && numbers.iter().all(|n| title.contains(n))
}

/// Grouped event row: the whole distribution in one place. Mutually
/// exclusive events list legs by probability; ladders keep Kalshi's strike
/// order. Past KALSHI_EVENT_ROW_LEGS, the deepest legs are kept.
fn kalshi_event_group_row(
    event: &KalshiV2Event,
    event_title: &str,
    category: &str,
    series_ticker: &str,
    event_score: i64,
    legs: &[&KalshiV2Market],
    status: KalshiSearchStatus,
) -> serde_json::Value {
    let mutually_exclusive = event.mutually_exclusive.unwrap_or(false);
    let mut shown: Vec<(usize, &KalshiV2Market)> = legs.iter().copied().enumerate().collect();
    if shown.len() > KALSHI_EVENT_ROW_LEGS {
        shown.sort_by(|a, b| {
            b.1.volume_usd()
                .partial_cmp(&a.1.volume_usd())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        shown.truncate(KALSHI_EVENT_ROW_LEGS);
    }
    let settled_leg = |m: &KalshiV2Market| KalshiSearchStatus::Settled.leg_matches(m.status.as_deref());
    if mutually_exclusive {
        shown.sort_by(|a, b| {
            let pa = a.1.probability_yes(settled_leg(a.1)).unwrap_or(-1.0);
            let pb = b.1.probability_yes(settled_leg(b.1)).unwrap_or(-1.0);
            pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        shown.sort_by_key(|(idx, _)| *idx);
    }
    let leg_rows: Vec<serde_json::Value> = shown
        .iter()
        .map(|(_, m)| {
            let outcome = m
                .yes_sub_title
                .as_deref()
                .or(m.subtitle.as_deref())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(m.title.as_str());
            let mut leg = serde_json::json!({
                "ticker": m.ticker,
                "outcome": live_normalize_whitespace(outcome),
                "p": m.probability_yes(settled_leg(m)),
            });
            if settled_leg(m) {
                if let Some(r) = m.result.as_deref().filter(|r| !r.trim().is_empty()) {
                    leg["result"] = serde_json::json!(r);
                }
            }
            leg
        })
        .collect();
    let volume_usd: f64 =
        (legs.iter().map(|m| m.volume_usd()).sum::<f64>() * 100.0).round() / 100.0;
    let close_times: std::collections::BTreeSet<&str> =
        legs.iter().filter_map(|m| m.close_time.as_deref()).collect();
    let mut row = serde_json::json!({
        "source": "kalshi",
        "event_ticker": event.event_ticker,
        "series_ticker": series_ticker,
        "title": event_title,
        "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
        "match_score": event_score,
        "volume_usd": volume_usd,
        "legs": leg_rows,
    });
    if mutually_exclusive {
        row["mutually_exclusive"] = serde_json::json!(true);
    }
    // One close time when every leg shares it; else the earliest.
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

/// Attach current Kalshi order books to the Kalshi rows of a search slice
/// (one /markets/orderbooks call per 100 rows). Kalshi books hold BIDS only:
/// yes_bids at p, no_bids at q (a NO bid at q is a YES offer at 1-q).
async fn attach_kalshi_orderbooks_to_live_markets(
    markets: &mut [serde_json::Value],
    depth: usize,
    api_errors: &mut Vec<serde_json::Value>,
) {
    let tickers: Vec<String> = markets
        .iter()
        .filter(|m| m.get("source").and_then(|v| v.as_str()) == Some("kalshi"))
        .filter_map(|m| m.get("ticker").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    if tickers.is_empty() {
        return;
    }
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let url = format!("{KALSHI_V2_BASE_URL}/markets/orderbooks");
    let mut books: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    let fetched_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for chunk in tickers.chunks(100) {
        let query: Vec<(&str, String)> = chunk.iter().map(|t| ("tickers", t.clone())).collect();
        match kalshi_get_json_opt(&client, &url, &query, "kalshi orderbooks").await {
            Ok(Some(body)) => {
                for book in body.get("orderbooks").and_then(|v| v.as_array()).into_iter().flatten() {
                    if let Some(t) = book.get("ticker").and_then(|v| v.as_str()) {
                        books.insert(t.to_string(), kalshi_compact_book(book, depth));
                    }
                }
            }
            Ok(None) => {}
            Err(error) => api_errors.push(serde_json::json!({
                "phase": "orderbook",
                "source": "kalshi",
                "error": error,
            })),
        }
    }
    for market in markets.iter_mut() {
        if market.get("source").and_then(|v| v.as_str()) != Some("kalshi") {
            continue;
        }
        let Some(book) = market
            .get("ticker")
            .and_then(|v| v.as_str())
            .and_then(|t| books.get(t))
            .cloned()
        else {
            continue;
        };
        market["orderbook"] = book;
        market["orderbook_timestamp"] = serde_json::json!(fetched_at);
    }
}

/// {"yes_bids": [[price, contracts], ...], "no_bids": [...]}, best first.
fn kalshi_compact_book(book: &serde_json::Value, depth: usize) -> serde_json::Value {
    let fp = book.get("orderbook_fp");
    let side = |key: &str| -> Vec<[f64; 2]> {
        let mut levels: Vec<[f64; 2]> = fp
            .and_then(|f| f.get(key))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|lvl| {
                        let pair = lvl.as_array()?;
                        let p = pair.first()?.as_str()?.trim().parse::<f64>().ok()?;
                        let q = pair.get(1)?.as_str()?.trim().parse::<f64>().ok()?;
                        Some([p, q])
                    })
                    .collect()
            })
            .unwrap_or_default();
        levels.sort_by(|a, b| b[0].partial_cmp(&a[0]).unwrap_or(std::cmp::Ordering::Equal));
        levels.truncate(depth.max(1));
        levels
    };
    serde_json::json!({
        "yes_bids": side("yes_dollars"),
        "no_bids": side("no_dollars"),
    })
}

/// Parse a trades window bound: RFC3339, YYYY-MM-DD (start of day for
/// `since`, end of day for `until`), or a lookback like 6h / 2d.
fn parse_trades_bound(
    raw: &str,
    end_of_day: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<chrono::DateTime<chrono::Utc>> {
    let raw = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let time = if end_of_day {
            date.and_hms_opt(23, 59, 59)
        } else {
            date.and_hms_opt(0, 0, 0)
        }
        .ok_or_else(|| anyhow::anyhow!("invalid date '{raw}'"))?;
        return Ok(chrono::DateTime::from_naive_utc_and_offset(time, chrono::Utc));
    }
    parse_closing_within(raw)
        .map(|d| now - d)
        .map_err(|_| anyhow::anyhow!("'{raw}': use RFC3339, YYYY-MM-DD, or a lookback like 6h, 2d"))
}

#[derive(Clone, Debug, serde::Deserialize)]
struct KalshiTrade {
    created_time: String,
    #[serde(default)]
    count_fp: Option<String>,
    #[serde(default)]
    yes_price_dollars: Option<String>,
    #[serde(default)]
    taker_side: Option<String>,
    #[serde(default)]
    is_block_trade: Option<bool>,
}

impl KalshiTrade {
    fn contracts(&self) -> f64 {
        live_parse_decimal(self.count_fp.as_deref()).unwrap_or(0.0)
    }
}

const KALSHI_TRADES_PAGE: usize = 1000;
const KALSHI_TRADES_MAX_PAGES: usize = 25;

/// Page trades newest-first through `url` for [min_ts, max_ts]. Returns the
/// trades and whether the page cap cut the window short.
async fn kalshi_fetch_trades_window(
    client: &reqwest::Client,
    url: &str,
    ticker: &str,
    min_ts: i64,
    max_ts: i64,
) -> Result<(Vec<KalshiTrade>, bool)> {
    #[derive(serde::Deserialize)]
    struct TradesResp {
        #[serde(default)]
        trades: Vec<KalshiTrade>,
        #[serde(default)]
        cursor: Option<String>,
    }
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..KALSHI_TRADES_MAX_PAGES {
        let mut q = vec![
            ("ticker", ticker.to_string()),
            ("min_ts", min_ts.to_string()),
            ("max_ts", max_ts.to_string()),
            ("limit", KALSHI_TRADES_PAGE.to_string()),
        ];
        if let Some(c) = cursor.as_ref() {
            q.push(("cursor", c.clone()));
        }
        let Some(body) = kalshi_get_json_opt(client, url, &q, "kalshi trades")
            .await
            .map_err(|e| anyhow::anyhow!(e))?
        else {
            return Ok((out, false));
        };
        let page: TradesResp = serde_json::from_value(body).context("parse kalshi trades")?;
        out.extend(page.trades);
        cursor = page.cursor.filter(|c| !c.trim().is_empty());
        if cursor.is_none() {
            return Ok((out, false));
        }
    }
    Ok((out, true))
}

/// `finance odds --market T --trades`: Kalshi tick-level prints for one
/// market over a window, with taker-side totals and the largest prints.
/// Trades older than Kalshi's historical cutoff come from the archive.
async fn cmd_finance_odds_kalshi_trades(
    ticker: &str,
    since: Option<&str>,
    until: Option<&str>,
    min_contracts: Option<f64>,
    limit: Option<usize>,
    out_path: Option<&std::path::Path>,
) -> Result<()> {
    let now = chrono::Utc::now();
    let ticker = ticker.trim().to_ascii_uppercase();
    let start = match since {
        Some(raw) => parse_trades_bound(raw, false, now).context("--since")?,
        None => now - chrono::Duration::hours(24),
    };
    let end = match until {
        Some(raw) => parse_trades_bound(raw, true, now).context("--until")?,
        None => now,
    };
    if start >= end {
        anyhow::bail!("--since must be before --until");
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("build kalshi client")?;

    // Fills before the cutoff live only in the archive.
    let cutoff = kalshi_get_json_opt(&client, &format!("{KALSHI_V2_BASE_URL}/historical/cutoff"), &[], "kalshi historical cutoff")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.get("trades_created_ts").and_then(|t| t.as_str()).map(str::to_string))
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| t.with_timezone(&chrono::Utc));

    let mut trades: Vec<KalshiTrade> = Vec::new();
    let mut truncated = false;
    let live_start = cutoff.map(|c| c.max(start)).unwrap_or(start);
    if live_start < end {
        let (mut t, cut) = kalshi_fetch_trades_window(
            &client,
            &format!("{KALSHI_V2_BASE_URL}/markets/trades"),
            &ticker,
            live_start.timestamp(),
            end.timestamp(),
        )
        .await?;
        trades.append(&mut t);
        truncated |= cut;
    }
    if let Some(c) = cutoff.filter(|c| start < *c) {
        let (mut t, cut) = kalshi_fetch_trades_window(
            &client,
            &format!("{KALSHI_V2_BASE_URL}/historical/trades"),
            &ticker,
            start.timestamp(),
            c.min(end).timestamp(),
        )
        .await?;
        trades.append(&mut t);
        truncated |= cut;
    }
    trades.sort_by(|a, b| a.created_time.cmp(&b.created_time));

    let resp = kalshi_trades_payload(&ticker, start, end, &trades, truncated, min_contracts, limit.unwrap_or(50));
    if let Some(path) = out_path {
        let wr = write_json_out_with_meta(path.to_path_buf(), &resp, "finance.odds.trades", &[format!("ticker={ticker}")])?;
        println!(
            "{}",
            serde_json::json!({"ok": true, "path": wr.out_path.display().to_string()})
        );
        return Ok(());
    }
    let mut payload = resp;
    round_json_floats(&mut payload);
    println!("{}", json_to_stdout_string(&payload)?);
    Ok(())
}

/// Summary + the `limit` largest prints (by contracts, at or above
/// min_contracts), listed in time order.
fn kalshi_trades_payload(
    ticker: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    trades: &[KalshiTrade],
    truncated: bool,
    min_contracts: Option<f64>,
    limit: usize,
) -> serde_json::Value {
    let side_sum = |side: &str| -> f64 {
        trades
            .iter()
            .filter(|t| t.taker_side.as_deref() == Some(side))
            .map(KalshiTrade::contracts)
            .sum()
    };
    let total: f64 = trades.iter().map(KalshiTrade::contracts).sum();
    let mut picked: Vec<&KalshiTrade> = trades
        .iter()
        .filter(|t| min_contracts.is_none_or(|m| t.contracts() >= m))
        .collect();
    let matching = picked.len();
    if picked.len() > limit {
        picked.sort_by(|a, b| b.contracts().partial_cmp(&a.contracts()).unwrap_or(std::cmp::Ordering::Equal));
        picked.truncate(limit);
        picked.sort_by(|a, b| a.created_time.cmp(&b.created_time));
    }
    let rows: Vec<serde_json::Value> = picked
        .iter()
        .map(|t| {
            let mut row = serde_json::json!({
                "t": t.created_time,
                "yes_price": live_parse_decimal(t.yes_price_dollars.as_deref()),
                "contracts": t.contracts(),
                "taker": t.taker_side,
            });
            if t.is_block_trade == Some(true) {
                row["block"] = serde_json::json!(true);
            }
            row
        })
        .collect();
    let mut out = serde_json::json!({
        "schema_version": "finance.odds.trades.v1",
        "source": "kalshi",
        "ticker": ticker,
        "start": start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "end": end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "_summary": {
            "trades": trades.len(),
            "contracts": (total * 100.0).round() / 100.0,
            "yes_taker_contracts": (side_sum("yes") * 100.0).round() / 100.0,
            "no_taker_contracts": (side_sum("no") * 100.0).round() / 100.0,
            "first": trades.first().map(|t| t.created_time.clone()),
            "last": trades.last().map(|t| t.created_time.clone()),
        },
        "trades": rows,
    });
    if matching > rows.len() {
        out["trades_note"] = serde_json::json!(format!(
            "{} of {matching} matching prints shown: the largest, in time order (raise --limit or --min-contracts)",
            rows.len()
        ));
    }
    if truncated {
        out["window_truncated"] = serde_json::json!(format!(
            "page cap hit ({} trades): prints before {} are missing; narrow --since/--until",
            KALSHI_TRADES_PAGE * KALSHI_TRADES_MAX_PAGES,
            trades.first().map(|t| t.created_time.as_str()).unwrap_or("?")
        ));
    }
    out
}

#[cfg(test)]
mod odds_kalshi_tests {
    use super::*;

    fn market(ticker: &str, sub: &str, bid: &str, ask: &str, vol: &str, status: &str) -> KalshiV2Market {
        KalshiV2Market {
            ticker: ticker.to_string(),
            title: format!("Will {sub} win?"),
            status: Some(status.to_string()),
            yes_sub_title: Some(sub.to_string()),
            yes_bid_dollars: Some(bid.to_string()),
            yes_ask_dollars: Some(ask.to_string()),
            volume_fp: Some(vol.to_string()),
            close_time: Some("2027-02-01T15:00:00Z".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn status_filter_parses_and_matches_v2_statuses() {
        assert_eq!(KalshiSearchStatus::parse(None).unwrap(), KalshiSearchStatus::Open);
        assert_eq!(KalshiSearchStatus::parse(Some("Resolved")).unwrap(), KalshiSearchStatus::Settled);
        assert!(KalshiSearchStatus::parse(Some("bogus")).is_err());
        assert!(KalshiSearchStatus::Open.leg_matches(Some("active")));
        assert!(!KalshiSearchStatus::Open.leg_matches(Some("finalized")));
        assert!(KalshiSearchStatus::Settled.leg_matches(Some("finalized")));
        assert!(KalshiSearchStatus::Settled.leg_matches(Some("determined")));
        assert!(KalshiSearchStatus::All.leg_matches(Some("initialized")));
        assert_eq!(KalshiSearchStatus::All.v1_param(), None);
    }

    #[test]
    fn subtitle_dropped_only_when_redundant() {
        assert!(kalshi_subtitle_is_redundant("Ohio Senate winner? (2028)", "In 2028"));
        assert!(kalshi_subtitle_is_redundant("Fed decision", ""));
        assert!(!kalshi_subtitle_is_redundant("Who will control the Senate?", "In 2026"));
        assert!(!kalshi_subtitle_is_redundant("Fed decision in December?", "Before Dec 2026"));
    }

    #[test]
    fn trades_payload_sums_taker_sides_and_keeps_largest_in_time_order() {
        let trade = |t: &str, n: &str, side: &str| KalshiTrade {
            created_time: t.to_string(),
            count_fp: Some(n.to_string()),
            yes_price_dollars: Some("0.7200".to_string()),
            taker_side: Some(side.to_string()),
            is_block_trade: Some(false),
        };
        let trades = vec![
            trade("2026-11-03T10:00:00Z", "5.00", "yes"),
            trade("2026-11-03T11:00:00Z", "900.00", "no"),
            trade("2026-11-03T12:00:00Z", "50.00", "yes"),
            trade("2026-11-03T13:00:00Z", "1200.00", "yes"),
        ];
        let start = chrono::DateTime::parse_from_rfc3339("2026-11-03T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let end = start + chrono::Duration::days(1);
        let out = kalshi_trades_payload("X-1-A", start, end, &trades, false, Some(10.0), 2);
        assert_eq!(out["_summary"]["contracts"], serde_json::json!(2155.0));
        assert_eq!(out["_summary"]["yes_taker_contracts"], serde_json::json!(1255.0));
        assert_eq!(out["_summary"]["no_taker_contracts"], serde_json::json!(900.0));
        let rows = out["trades"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["t"], serde_json::json!("2026-11-03T11:00:00Z"));
        assert_eq!(rows[1]["contracts"], serde_json::json!(1200.0));
        assert!(out["trades_note"].as_str().unwrap().starts_with("2 of 3"));
        assert!(out.get("window_truncated").is_none());
    }

    #[test]
    fn trades_bounds_accept_dates_and_lookbacks() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-03T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        assert_eq!(parse_trades_bound("6h", false, now).unwrap(), now - chrono::Duration::hours(6));
        assert_eq!(
            parse_trades_bound("2026-07-29", false, now).unwrap().to_rfc3339(),
            "2026-07-29T00:00:00+00:00"
        );
        assert_eq!(
            parse_trades_bound("2026-07-29", true, now).unwrap().to_rfc3339(),
            "2026-07-29T23:59:59+00:00"
        );
        assert!(parse_trades_bound("yesterday", false, now).is_err());
    }

    #[test]
    fn compact_book_is_best_first_and_capped() {
        let book = serde_json::json!({"ticker": "A", "orderbook_fp": {
            "yes_dollars": [["0.6900", "10.00"], ["0.7100", "5.50"], ["0.7000", "1.00"]],
            "no_dollars": [["0.2800", "3.00"]]
        }});
        let c = kalshi_compact_book(&book, 2);
        assert_eq!(c["yes_bids"], serde_json::json!([[0.71, 5.5], [0.70, 1.0]]));
        assert_eq!(c["no_bids"], serde_json::json!([[0.28, 3.0]]));
    }

    #[test]
    fn closing_within_parses_units() {
        assert_eq!(parse_closing_within("48h").unwrap(), chrono::Duration::hours(48));
        assert_eq!(parse_closing_within("2w").unwrap(), chrono::Duration::weeks(2));
        assert!(parse_closing_within("7").is_err());
        assert!(parse_closing_within("0d").is_err());
        assert!(parse_closing_within("3y").is_err());
    }

    #[test]
    fn ticker_shaped_queries_are_detected() {
        assert_eq!(kalshi_query_as_ticker("CONTROLS-2026-D").as_deref(), Some("CONTROLS-2026-D"));
        assert_eq!(kalshi_query_as_ticker("kxfeddecision").as_deref(), Some("KXFEDDECISION"));
        assert_eq!(kalshi_query_as_ticker("senate"), None);
        assert_eq!(kalshi_query_as_ticker("ohio senate"), None);
        assert_eq!(kalshi_query_as_ticker("KXFED 26DEC"), None);
    }

    #[test]
    fn probability_uses_mid_then_last_then_settlement() {
        let m = market("A", "Dem", "0.7200", "0.7300", "100", "active");
        assert!((m.probability_yes(false).unwrap() - 0.725).abs() < 1e-9);
        // Empty book: bid 0 / ask 1 is not a price; fall back to last trade.
        let mut thin = market("B", "Rep", "0.0000", "1.0000", "5", "active");
        thin.last_price_dollars = Some("0.0300".to_string());
        assert_eq!(thin.probability_yes(false), Some(0.03));
        let mut done = market("C", "Hold", "0.0000", "1.0000", "5", "finalized");
        done.result = Some("yes".to_string());
        assert_eq!(done.probability_yes(true), Some(1.0));
        done.settlement_value_dollars = Some("0.0000".to_string());
        assert_eq!(done.probability_yes(true), Some(0.0));
    }

    #[test]
    fn change_24h_needs_both_prints() {
        let mut m = market("A", "Dem", "0.72", "0.73", "1", "active");
        m.last_price_dollars = Some("0.7300".to_string());
        m.previous_price_dollars = Some("0.7400".to_string());
        assert_eq!(m.change_24h_pp(), Some(-1.0));
        m.previous_price_dollars = Some("0.7300".to_string());
        assert_eq!(m.change_24h_pp(), None);
        m.previous_price_dollars = Some("0.0000".to_string());
        assert_eq!(m.change_24h_pp(), None);
    }

    #[test]
    fn v1_candidates_drop_mentions_and_keep_rank_order() {
        let resp: KalshiV1SearchResp = serde_json::from_value(serde_json::json!({
            "current_page": [
                {"event_ticker": "SENATEOHS-26", "event_title": "Ohio Senate winner?", "series_title": "Ohio Senate", "category": "Elections", "total_volume": 11584672,
                 "product_metadata": {"series_frequency": "custom", "series_tags": ["US Elections"]}, "markets": []},
                {"event_ticker": "KXSENATEMENTION-26NOV", "event_title": "What will Sherrod Brown say?", "category": "Mentions", "markets": []},
                {"event_ticker": "KXMIDTERMMOV-OHSEND", "event_title": "Ohio Senate margin of victory", "total_volume": 283433, "markets": []}
            ]
        }))
        .unwrap();
        let c = kalshi_v1_candidates(resp, false);
        let tickers: Vec<&str> = c.iter().map(|c| c.event_ticker.as_str()).collect();
        assert_eq!(tickers, vec!["SENATEOHS-26", "KXMIDTERMMOV-OHSEND"]);
        assert_eq!(c[1].rank, 2);
        assert!(c[0].match_text.contains("US Elections"));
    }

    #[test]
    fn candidate_score_prefers_deep_daily_over_fifteen_minute() {
        let terms = vec!["oil".to_string()];
        let quick = KalshiSearchCandidate {
            event_ticker: "KXWTI15M-26NOV031230".into(),
            rank: 0,
            match_text: "WTI Oil 15 min".into(),
            index_volume: 22_570.0,
            frequency: Some("fifteen_min".into()),
            category: None,
        };
        let deep = KalshiSearchCandidate {
            event_ticker: "KXWTIMAX-26DEC31".into(),
            rank: 24,
            match_text: "How high will oil get this year? WTI oil yearly max".into(),
            index_volume: 10_776_782.0,
            frequency: Some("annual".into()),
            category: None,
        };
        assert!(kalshi_candidate_score(&deep, "oil", &terms) > kalshi_candidate_score(&quick, "oil", &terms));
    }

    #[test]
    fn event_group_row_orders_exclusive_legs_by_probability_and_caps() {
        let legs_owned: Vec<KalshiV2Market> = (0..15)
            .map(|i| {
                let p = 0.02 + i as f64 * 0.01;
                market(
                    &format!("EVT-26-L{i}"),
                    &format!("Leg {i}"),
                    &format!("{:.4}", p),
                    &format!("{:.4}", p + 0.01),
                    &format!("{}", 1000 - i * 10),
                    "active",
                )
            })
            .collect();
        let event = KalshiV2Event {
            event_ticker: "EVT-26".into(),
            title: "Who wins?".into(),
            mutually_exclusive: Some(true),
            markets: legs_owned.clone(),
            ..Default::default()
        };
        let legs: Vec<&KalshiV2Market> = legs_owned.iter().collect();
        let row = kalshi_event_group_row(&event, "Who wins?", "Elections", "EVT", 10, &legs, KalshiSearchStatus::Open);
        let shown = row["legs"].as_array().unwrap();
        assert_eq!(shown.len(), KALSHI_EVENT_ROW_LEGS);
        assert_eq!(row["legs_omitted"], serde_json::json!(3));
        // Deepest 12 are legs 0..11; exclusive → highest probability first.
        assert_eq!(shown[0]["ticker"], serde_json::json!("EVT-26-L11"));
        assert_eq!(row["close_time"], serde_json::json!("2027-02-01T15:00:00Z"));
        assert_eq!(row["mutually_exclusive"], serde_json::json!(true));
    }
}
