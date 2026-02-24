use serde::de::DeserializeOwned;

const KALSHI_EVENTS_PAGE_LIMIT: usize = 200;
const KALSHI_MARKETS_PAGE_LIMIT: usize = 1000;
const KALSHI_MAX_RETRIES: usize = 6;

#[derive(Default, Clone, Copy)]
struct RetryMetrics {
    attempts: usize,
    retry_429: usize,
    retry_5xx: usize,
}

impl RetryMetrics {
    fn merge(&mut self, other: RetryMetrics) {
        self.attempts = self.attempts.saturating_add(other.attempts);
        self.retry_429 = self.retry_429.saturating_add(other.retry_429);
        self.retry_5xx = self.retry_5xx.saturating_add(other.retry_5xx);
    }
}

#[derive(Deserialize)]
struct EventsResp {
    #[serde(default)]
    events: Vec<EventRow>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Clone, Deserialize)]
struct EventRow {
    event_ticker: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    series_ticker: Option<String>,
}

#[derive(Deserialize)]
struct MarketsResp {
    #[serde(default)]
    markets: Vec<MarketRow>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Clone, Deserialize)]
struct MarketRow {
    ticker: String,
    #[serde(default)]
    title: Option<String>,
    event_ticker: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    volume: Option<i64>,
    #[serde(default)]
    volume_24h: Option<i64>,
    #[serde(default)]
    yes_bid_dollars: Option<String>,
    #[serde(default)]
    yes_ask_dollars: Option<String>,
    #[serde(default)]
    last_price_dollars: Option<String>,
    #[serde(default)]
    yes_bid: Option<i64>,
    #[serde(default)]
    yes_ask: Option<i64>,
    #[serde(default, rename = "last_price", alias = "yes_price")]
    last_price: Option<i64>,
}

struct EventsFetchResult {
    rows: Vec<EventRow>,
    pages_fetched: usize,
    exhausted: bool,
    metrics: RetryMetrics,
}

struct GlobalMarketsFetchResult {
    pages_fetched: usize,
    exhausted: bool,
    metrics: RetryMetrics,
}

/// Sync events/markets from Kalshi with explicit dual-path ingestion:
///   - include_sports=true: global `/markets` cursor pagination (full breadth).
///   - include_sports=false: global `/markets` with `mve_filter=exclude`, then
///     event-category filtering to keep non-sports coverage with lower token/noise load.
pub(crate) async fn sync_kalshi_events(
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
        "[kalshi] starting sync (max_pages={}, include_sports={})",
        max_pages
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unbounded".to_string()),
        include_sports
    );

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(StdDuration::from_secs(30))
        .build()
        .map_err(|e| format!("Kalshi client init failed: {e}"))?;

    let page_cap = max_pages.unwrap_or(usize::MAX);

    let events_pages_fetched;
    let events_exhausted;
    let events_requests;
    let mut markets_pages_fetched = 0usize;
    let markets_exhausted;
    let mut markets_requests = 0usize;
    let mut retry_count_429 = 0usize;
    let mut retry_count_5xx = 0usize;

    let events_fetch = fetch_open_events(&client, limiter, page_cap).await?;
    events_pages_fetched = events_fetch.pages_fetched;
    events_exhausted = events_fetch.exhausted;
    events_requests = events_fetch.metrics.attempts;
    retry_count_429 = retry_count_429.saturating_add(events_fetch.metrics.retry_429);
    retry_count_5xx = retry_count_5xx.saturating_add(events_fetch.metrics.retry_5xx);

    let mut category_map: HashMap<String, Option<String>> = HashMap::new();
    let mut all_events: Vec<OddsListedEvent> = Vec::new();
    for row in events_fetch.rows {
        let category = normalize_event_category(row.category.as_deref())
            .or_else(|| infer_category_from_event_ticker(&row.event_ticker));
        let is_sports = category.as_deref().map_or(false, is_sports_category);
        category_map.insert(row.event_ticker.clone(), category.clone());

        if !include_sports && is_sports {
            continue;
        }

        all_events.push(build_event_record(row, category));
    }

    let mut all_markets: Vec<OddsListedMarket> = Vec::new();
    let mut seen_market_tickers: HashSet<String> = HashSet::new();

    if include_sports {
        eprintln!("[kalshi] mode=global-markets (sports included)");
        let global = run_global_markets_sync(
            &client,
            limiter,
            page_cap,
            include_sports,
            &category_map,
            &mut seen_market_tickers,
            &mut all_markets,
        )
        .await?;
        markets_pages_fetched = markets_pages_fetched.saturating_add(global.pages_fetched);
        markets_exhausted = global.exhausted;
        markets_requests = markets_requests.saturating_add(global.metrics.attempts);
        retry_count_429 = retry_count_429.saturating_add(global.metrics.retry_429);
        retry_count_5xx = retry_count_5xx.saturating_add(global.metrics.retry_5xx);
    } else {
        eprintln!("[kalshi] mode=global-markets (non-sports filtered)");
        let global = run_global_markets_sync(
            &client,
            limiter,
            page_cap,
            false,
            &category_map,
            &mut seen_market_tickers,
            &mut all_markets,
        )
        .await?;
        markets_pages_fetched = markets_pages_fetched.saturating_add(global.pages_fetched);
        markets_exhausted = global.exhausted;
        markets_requests = markets_requests.saturating_add(global.metrics.attempts);
        retry_count_429 = retry_count_429.saturating_add(global.metrics.retry_429);
        retry_count_5xx = retry_count_5xx.saturating_add(global.metrics.retry_5xx);
    }

    all_events.sort_by(|a, b| a.ticker.cmp(&b.ticker));
    all_markets.sort_by(|a, b| a.ticker.cmp(&b.ticker));

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
        "[kalshi] sync complete: {} markets, {} events",
        all_markets.len(),
        all_events.len()
    );

    Ok((all_events, all_markets, coverage))
}

fn build_event_record(row: EventRow, category: Option<String>) -> OddsListedEvent {
    let title = row
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&row.event_ticker)
        .to_string();

    OddsListedEvent {
        ticker: row.event_ticker,
        title,
        category,
        series_ticker: row.series_ticker,
        source: Some("kalshi".to_string()),
        event_id: None,
        slug: None,
        tags: None,
    }
}

async fn fetch_open_events(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    page_cap: usize,
) -> std::result::Result<EventsFetchResult, String> {
    let mut rows = Vec::new();
    let mut pages_fetched = 0usize;
    let mut exhausted = false;
    let mut metrics = RetryMetrics::default();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut page = 0usize;

    while page < page_cap {
        let mut url = reqwest::Url::parse(&format!("{}/events", KALSHI_BASE_URL))
            .map_err(|e| format!("Kalshi events url parse failed: {e}"))?;
        url.query_pairs_mut()
            .append_pair("status", "open")
            .append_pair("limit", &KALSHI_EVENTS_PAGE_LIMIT.to_string());
        if let Some(ref c) = cursor {
            if !c.trim().is_empty() {
                url.query_pairs_mut().append_pair("cursor", c.trim());
            }
        }

        let (body, req_metrics) =
            fetch_json_with_retry::<EventsResp>(client, limiter, url, "Kalshi events").await?;
        metrics.merge(req_metrics);

        let count = body.events.len();
        pages_fetched = pages_fetched.saturating_add(1);
        if page == 0 || (page + 1) % 10 == 0 || count == 0 {
            eprintln!(
                "[kalshi] events page {}: {} rows (total: {})",
                page + 1,
                count,
                rows.len() + count
            );
        }

        if count == 0 {
            exhausted = true;
            break;
        }

        rows.extend(body.events.into_iter());

        let next_cursor = body.cursor.filter(|c| !c.trim().is_empty());
        if let Some(ref c) = next_cursor {
            if !seen_cursors.insert(c.clone()) {
                eprintln!("[kalshi] events stop: cursor cycle detected");
                exhausted = true;
                break;
            }
        }
        cursor = next_cursor;
        if cursor.is_none() || count < KALSHI_EVENTS_PAGE_LIMIT {
            exhausted = true;
            break;
        }

        page = page.saturating_add(1);
    }

    Ok(EventsFetchResult {
        rows,
        pages_fetched,
        exhausted,
        metrics,
    })
}

async fn run_global_markets_sync(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    page_cap: usize,
    include_sports: bool,
    category_map: &HashMap<String, Option<String>>,
    seen_market_tickers: &mut HashSet<String>,
    all_markets: &mut Vec<OddsListedMarket>,
) -> std::result::Result<GlobalMarketsFetchResult, String> {
    let mut market_cursor: Option<String> = None;
    let mut seen_market_cursors: HashSet<String> = HashSet::new();
    let mut page = 0usize;
    let mut pages_fetched = 0usize;
    let mut fetched_market_rows = 0usize;
    let mut metrics = RetryMetrics::default();
    let mut exhausted = false;

    while page < page_cap {
        let mut url = reqwest::Url::parse(&format!("{}/markets", KALSHI_BASE_URL))
            .map_err(|e| format!("Kalshi markets url parse failed: {e}"))?;
        url.query_pairs_mut()
            .append_pair("status", "open")
            .append_pair("limit", &KALSHI_MARKETS_PAGE_LIMIT.to_string());
        if !include_sports {
            // Default non-sports mode excludes multivariate combo markets, which are
            // overwhelmingly sports-heavy and dominate sync runtime/noise.
            url.query_pairs_mut().append_pair("mve_filter", "exclude");
        }
        if let Some(ref c) = market_cursor {
            if !c.trim().is_empty() {
                url.query_pairs_mut().append_pair("cursor", c.trim());
            }
        }

        let (body, req_metrics) =
            fetch_json_with_retry::<MarketsResp>(client, limiter, url, "Kalshi markets").await?;
        metrics.merge(req_metrics);
        pages_fetched = pages_fetched.saturating_add(1);

        let count = body.markets.len();
        fetched_market_rows = fetched_market_rows.saturating_add(count);
        let kept_this_page = append_market_rows(
            body.markets,
            category_map,
            include_sports,
            seen_market_tickers,
            all_markets,
        );
        let dropped_this_page = count.saturating_sub(kept_this_page);
        eprintln!(
            "[kalshi] markets page {}: fetched={} fetched_total={} kept_total={} kept_this_page={} dropped_this_page={}",
            page + 1,
            count,
            fetched_market_rows,
            all_markets.len(),
            kept_this_page,
            dropped_this_page
        );

        if count == 0 {
            exhausted = true;
            break;
        }

        let next_cursor = body.cursor.filter(|c| !c.trim().is_empty());
        if let Some(ref c) = next_cursor {
            if !seen_market_cursors.insert(c.clone()) {
                eprintln!("[kalshi] markets stop: cursor cycle detected");
                exhausted = true;
                break;
            }
        }
        market_cursor = next_cursor;
        if market_cursor.is_none() || count < KALSHI_MARKETS_PAGE_LIMIT {
            exhausted = true;
            break;
        }

        page = page.saturating_add(1);
    }

    Ok(GlobalMarketsFetchResult {
        pages_fetched,
        exhausted,
        metrics,
    })
}

fn append_market_rows(
    rows: Vec<MarketRow>,
    category_map: &HashMap<String, Option<String>>,
    include_sports: bool,
    seen_market_tickers: &mut HashSet<String>,
    all_markets: &mut Vec<OddsListedMarket>,
) -> usize {
    let mut kept = 0usize;
    for row in rows {
        if !seen_market_tickers.insert(row.ticker.clone()) {
            continue;
        }

        let category = category_map
            .get(&row.event_ticker)
            .cloned()
            .flatten()
            .or_else(|| infer_category_from_event_ticker(&row.event_ticker));
        if !include_sports && category.as_deref().map_or(false, is_sports_category) {
            continue;
        }

        let probability_yes = probability_from_kalshi_fields(
            row.last_price_dollars.as_deref(),
            row.last_price,
            row.yes_bid_dollars.as_deref(),
            row.yes_ask_dollars.as_deref(),
            row.yes_bid,
            row.yes_ask,
        );
        let yes_price = probability_yes.map(probability_to_cents);
        let title = row
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&row.ticker)
            .to_string();

        all_markets.push(OddsListedMarket {
            ticker: row.ticker,
            title,
            event_ticker: row.event_ticker,
            yes_price,
            volume: row.volume.or(row.volume_24h),
            status: normalize_kalshi_status(row.status.as_deref()),
            source: Some("kalshi".to_string()),
            market_id: None,
            event_id: None,
            slug: None,
            outcomes: None,
            outcome_prices: None,
            clob_token_ids: None,
            probability_yes,
            category,
        });
        kept = kept.saturating_add(1);
    }
    kept
}

fn normalize_event_category(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

async fn fetch_json_with_retry<T: DeserializeOwned>(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    url: reqwest::Url,
    label: &str,
) -> std::result::Result<(T, RetryMetrics), String> {
    let mut attempt = 0usize;
    let mut metrics = RetryMetrics::default();

    loop {
        limiter.wait().await;
        attempt = attempt.saturating_add(1);
        metrics.attempts = metrics.attempts.saturating_add(1);

        let resp = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| format!("{label} fetch failed: {e}"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < KALSHI_MAX_RETRIES {
            metrics.retry_429 = metrics.retry_429.saturating_add(1);
            limiter.on_rate_limited();
            sleep(backoff_for_attempt(attempt)).await;
            continue;
        }
        if status.is_server_error() && attempt < KALSHI_MAX_RETRIES {
            metrics.retry_5xx = metrics.retry_5xx.saturating_add(1);
            limiter.on_rate_limited();
            sleep(backoff_for_attempt(attempt)).await;
            continue;
        }
        if !status.is_success() {
            return Err(format!("{label} fetch failed: http {status}"));
        }

        let body = resp
            .json::<T>()
            .await
            .map_err(|e| format!("{label} parse failed: {e}"))?;
        limiter.on_success();
        return Ok((body, metrics));
    }
}

fn parse_prob_from_dollars(raw: Option<&str>) -> Option<f64> {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
}

fn parse_prob_from_cents(raw: Option<i64>) -> Option<f64> {
    raw.map(|v| (v as f64 / 100.0).clamp(0.0, 1.0))
}

fn midpoint(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(((x + y) / 2.0).clamp(0.0, 1.0)),
        (Some(x), None) | (None, Some(x)) => Some(x.clamp(0.0, 1.0)),
        _ => None,
    }
}

fn probability_from_kalshi_fields(
    last_price_dollars: Option<&str>,
    last_price: Option<i64>,
    yes_bid_dollars: Option<&str>,
    yes_ask_dollars: Option<&str>,
    yes_bid: Option<i64>,
    yes_ask: Option<i64>,
) -> Option<f64> {
    parse_prob_from_dollars(last_price_dollars)
        .or_else(|| parse_prob_from_cents(last_price))
        .or_else(|| {
            midpoint(
                parse_prob_from_dollars(yes_bid_dollars),
                parse_prob_from_dollars(yes_ask_dollars),
            )
        })
        .or_else(|| midpoint(parse_prob_from_cents(yes_bid), parse_prob_from_cents(yes_ask)))
}

fn probability_to_cents(probability_yes: f64) -> i64 {
    (probability_yes * 100.0).round() as i64
}

fn normalize_kalshi_status(status: Option<&str>) -> Option<String> {
    let normalized = status
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())?;

    if normalized.contains("open") || normalized.contains("active") {
        return Some("open".to_string());
    }
    if normalized.contains("close") || normalized.contains("settled") || normalized.contains("final") {
        return Some("closed".to_string());
    }
    Some(normalized)
}

fn infer_category_from_event_ticker(event_ticker: &str) -> Option<String> {
    let upper = event_ticker.to_ascii_uppercase();
    let looks_sports = upper.contains("SPORT")
        || upper.starts_with("KXNBA")
        || upper.starts_with("KXNFL")
        || upper.starts_with("KXMLB")
        || upper.starts_with("KXNHL")
        || upper.starts_with("KXNCAA")
        || upper.starts_with("KXSOCCER")
        || upper.starts_with("KXEPL")
        || upper.starts_with("KXMLS")
        || upper.starts_with("KXATP")
        || upper.starts_with("KXWTA")
        || upper.starts_with("KXGOLF")
        || upper.starts_with("KXF1")
        || upper.starts_with("KXNASCAR")
        || upper.starts_with("KXMMA")
        || upper.starts_with("KXUFC")
        || upper.starts_with("KXCRICKET")
        || upper.starts_with("KXNCAAB")
        || upper.starts_with("KXNCAAF")
        || upper.starts_with("KXESPORT");
    if looks_sports {
        Some("Sports".to_string())
    } else {
        None
    }
}

fn backoff_for_attempt(attempt: usize) -> TokioDuration {
    let seconds = 2u64.saturating_mul(1u64 << ((attempt.saturating_sub(1)) as u32));
    TokioDuration::from_secs(seconds.min(30))
}
