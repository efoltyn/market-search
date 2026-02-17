use super::super::*;
use super::odds::fetch_odds_kalshi;

/// Returns true if a category string looks like sports/esports — these are noise for financial analysis.
fn is_sports_category(cat: &str) -> bool {
    let c = cat.to_lowercase();
    c.contains("sport") || c.contains("esport")
}

/// Sync events from Kalshi with rate limiting.
/// Strategy:
///   1. Fetch all open events → build category map
///   2. Probe series for category metadata
///   3. Fetch markets PER non-sports series (skips the 50k+ sports markets entirely)
///   4. Assign categories, drop any remaining sports
///
/// Sports markets are filtered out — they dominate Kalshi (50k+ open) and are pure noise
/// for financial analysis.
pub(crate) async fn sync_kalshi_events(
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
    eprintln!("[kalshi] starting sync (max_pages={})", max_pages);
    let mut events_pages_fetched = 0usize;
    let mut events_exhausted = false;
    let mut markets_pages_fetched = 0usize;
    let markets_exhausted; // set at the end based on coverage
    let mut series_backfill_calls = 0usize;
    let mut series_backfill_cap: Option<usize> = None;
    let mut series_backfill_truncated: Option<bool> = None;
    let mut category_map: HashMap<String, String> = HashMap::new();
    let mut series_category_map: HashMap<String, String> = HashMap::new();
    let mut series_catalog: Vec<(String, Option<String>)> = Vec::new();
    let mut all_events = Vec::new();

    // Phase 1: Fetch all open events (for category metadata).
    // Kalshi API limit max = 100 per docs.
    {
        let mut cursor: Option<String> = None;
        let event_pages = max_pages.max(100);
        for page in 0..event_pages {
            let mut attempts = 0usize;
            loop {
                limiter.wait().await;
                let req = OddsRequest {
                    provider: Some("kalshi".to_string()),
                    list_events: true,
                    limit: Some(100),
                    cursor: cursor.clone(),
                    max_pages: Some(1),
                    status: Some("open".to_string()),
                    ..Default::default()
                };
                match fetch_odds_kalshi(req).await {
                    Ok(resp) => {
                        events_pages_fetched = events_pages_fetched.saturating_add(1);
                        limiter.on_success();
                        let events = resp.available_events.unwrap_or_default();
                        let count = events.len();
                        if page % 10 == 0 || count == 0 {
                            eprintln!(
                                "[kalshi] events page {}: {} events (total: {})",
                                page + 1,
                                count,
                                all_events.len() + count
                            );
                        }
                        if count == 0 {
                            break;
                        }
                        for e in &events {
                            if let Some(ref cat) = e.category {
                                category_map.insert(e.ticker.clone(), cat.clone());
                                if let Some(ref st) = e.series_ticker {
                                    series_category_map.insert(st.clone(), cat.clone());
                                }
                            }
                        }
                        all_events.extend(events);
                        cursor = resp.cursor.filter(|c| !c.trim().is_empty());
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let is_rate_limit = msg.contains("http 429")
                            || msg.contains("429 Too Many Requests")
                            || msg.contains("http 401")
                            || msg.contains("token_authentication_failure");
                        if is_rate_limit && attempts < 5 {
                            attempts += 1;
                            limiter.on_rate_limited();
                            let cooldown_secs =
                                5u64.saturating_mul(1u64 << ((attempts - 1) as u32));
                            eprintln!(
                                "[kalshi] rate limited on events page {}, retry {}/5 (backoff {}s)",
                                page + 1,
                                attempts,
                                cooldown_secs.min(60)
                            );
                            sleep(TokioDuration::from_secs(cooldown_secs.min(60))).await;
                            continue;
                        }
                        eprintln!("[kalshi] events page {} failed: {}", page + 1, e);
                        break;
                    }
                }
            }
            if cursor.is_none() {
                events_exhausted = true;
                break;
            }
        }
    }

    // Filter out sports events
    let sports_events_before = all_events.len();
    all_events.retain(|e| !e.category.as_deref().map_or(false, is_sports_category));
    let sports_events_dropped = sports_events_before - all_events.len();

    eprintln!(
        "[kalshi] events done: {} non-sports events (dropped {} sports)",
        all_events.len(),
        sports_events_dropped,
    );

    // Phase 2: Series probes for category metadata.
    // We probe non-sports categories to build our series catalog.
    {
        let probes: Vec<Option<&str>> = vec![
            None,
            Some("Economics"),
            Some("Financials"),
            Some("Politics"),
            Some("Companies"),
            Some("World"),
            Some("Crypto"),
            Some("Science and Technology"),
        ];
        for category in probes {
            let mut attempts = 0usize;
            loop {
                limiter.wait().await;
                let req = OddsRequest {
                    provider: Some("kalshi".to_string()),
                    list_series: true,
                    category: category.map(|s| s.to_string()),
                    limit: Some(100),
                    max_pages: Some(1),
                    ..Default::default()
                };
                match fetch_odds_kalshi(req).await {
                    Ok(resp) => {
                        limiter.on_success();
                        if let Some(series_list) = resp.available_series {
                            for s in &series_list {
                                series_catalog.push((s.ticker.clone(), s.category.clone()));
                                if let Some(ref cat) = s.category {
                                    series_category_map
                                        .entry(s.ticker.clone())
                                        .or_insert_with(|| cat.clone());
                                }
                            }
                        }
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let is_rate_limit = msg.contains("http 429")
                            || msg.contains("429 Too Many Requests")
                            || msg.contains("http 401")
                            || msg.contains("token_authentication_failure");
                        if is_rate_limit && attempts < 4 {
                            attempts += 1;
                            limiter.on_rate_limited();
                            let cooldown_secs =
                                4u64.saturating_mul(1u64 << ((attempts - 1) as u32));
                            sleep(TokioDuration::from_secs(cooldown_secs.min(45))).await;
                            continue;
                        }
                        warn!(
                            "Kalshi series list probe failed for category {:?} (non-fatal): {}",
                            category, e
                        );
                        break;
                    }
                }
            }
        }
        let search_terms = [
            "recession",
            "gdp",
            "inflation",
            "cpi",
            "fed",
            "tariff",
            "greenland",
            "bitcoin",
            "ethereum",
            "trump",
            "congress",
        ];
        for term in search_terms {
            let mut attempts = 0usize;
            loop {
                limiter.wait().await;
                let req = OddsRequest {
                    provider: Some("kalshi".to_string()),
                    list_series: true,
                    search: Some(term.to_string()),
                    limit: Some(100),
                    max_pages: Some(1),
                    ..Default::default()
                };
                match fetch_odds_kalshi(req).await {
                    Ok(resp) => {
                        limiter.on_success();
                        if let Some(series_list) = resp.available_series {
                            for s in &series_list {
                                series_catalog.push((s.ticker.clone(), s.category.clone()));
                                if let Some(ref cat) = s.category {
                                    series_category_map
                                        .entry(s.ticker.clone())
                                        .or_insert_with(|| cat.clone());
                                }
                            }
                        }
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let is_rate_limit = msg.contains("http 429")
                            || msg.contains("429 Too Many Requests")
                            || msg.contains("http 401")
                            || msg.contains("token_authentication_failure");
                        if is_rate_limit && attempts < 4 {
                            attempts += 1;
                            limiter.on_rate_limited();
                            let cooldown_secs =
                                4u64.saturating_mul(1u64 << ((attempts - 1) as u32));
                            sleep(TokioDuration::from_secs(cooldown_secs.min(45))).await;
                            continue;
                        }
                        warn!(
                            "Kalshi series list search probe failed for '{}' (non-fatal): {}",
                            term, e
                        );
                        break;
                    }
                }
            }
        }
        let mut seen_series = HashSet::new();
        series_catalog.retain(|(ticker, _)| seen_series.insert(ticker.clone()));
        eprintln!(
            "[kalshi] series probes: {} unique series",
            series_catalog.len()
        );
    }

    // Phase 3: Fetch markets per NON-SPORTS series.
    // Instead of paginating all 50k+ open markets (dominated by sports),
    // we fetch markets per series_ticker for non-sports series only.
    // This is much more efficient and gives complete non-sports coverage.
    let mut all_markets = Vec::new();
    let mut seen_market_tickers: HashSet<String> = HashSet::new();
    {
        // Also collect non-sports series from events (series_ticker on events)
        let mut fetch_series: Vec<String> = Vec::new();

        // Priority: economics/financials/companies/crypto first
        let mut hi: Vec<String> = Vec::new();
        let mut mid: Vec<String> = Vec::new();
        let mut low: Vec<String> = Vec::new();
        for (ticker, cat) in &series_catalog {
            let c = cat.as_deref().unwrap_or("").to_lowercase();
            if is_sports_category(&c) {
                continue;
            }
            if c.contains("econom")
                || c.contains("financial")
                || c.contains("compan")
                || c.contains("crypto")
            {
                hi.push(ticker.clone());
            } else if c.contains("politic") || c.contains("world") || c.contains("elect") {
                mid.push(ticker.clone());
            } else {
                low.push(ticker.clone());
            }
        }
        fetch_series.extend(hi);
        fetch_series.extend(mid);
        fetch_series.extend(low);

        // Also add series_tickers discovered from events that aren't in the catalog yet
        let catalog_set: HashSet<String> = series_catalog.iter().map(|(t, _)| t.clone()).collect();
        for e in &all_events {
            if let Some(ref st) = e.series_ticker {
                if !catalog_set.contains(st) {
                    // Check if the series category (from events) is non-sports
                    let cat = series_category_map
                        .get(st)
                        .map(|c| c.as_str())
                        .unwrap_or("");
                    if !is_sports_category(cat) {
                        fetch_series.push(st.clone());
                    }
                }
            }
        }

        let mut seen = HashSet::new();
        fetch_series.retain(|s| seen.insert(s.clone()));

        let series_cap = (max_pages.saturating_mul(50)).clamp(200, 10_000);
        series_backfill_cap = Some(series_cap);
        let considered_series = fetch_series.len();
        eprintln!(
            "[kalshi] fetching markets for {} non-sports series (cap: {})",
            considered_series.min(series_cap),
            series_cap,
        );

        let mut calls = 0usize;
        for series_ticker in fetch_series.into_iter().take(series_cap) {
            let mut attempts = 0usize;
            loop {
                limiter.wait().await;
                let req = OddsRequest {
                    provider: Some("kalshi".to_string()),
                    list_markets: true,
                    series_ticker: Some(series_ticker.clone()),
                    limit: Some(100),
                    max_pages: Some(1),
                    status: Some("open".to_string()),
                    ..Default::default()
                };
                match fetch_odds_kalshi(req).await {
                    Ok(resp) => {
                        limiter.on_success();
                        calls += 1;
                        markets_pages_fetched = markets_pages_fetched.saturating_add(1);
                        for market in resp.available_markets.unwrap_or_default() {
                            if seen_market_tickers.insert(market.ticker.clone()) {
                                all_markets.push(market);
                            }
                        }
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let is_rate_limit = msg.contains("http 429")
                            || msg.contains("429 Too Many Requests")
                            || msg.contains("http 401")
                            || msg.contains("token_authentication_failure");
                        if is_rate_limit && attempts < 4 {
                            attempts += 1;
                            limiter.on_rate_limited();
                            let cooldown_secs =
                                4u64.saturating_mul(1u64 << ((attempts - 1) as u32));
                            sleep(TokioDuration::from_secs(cooldown_secs.min(45))).await;
                            continue;
                        }
                        warn!(
                            "Kalshi series market fetch failed for {}: {}",
                            series_ticker, e
                        );
                        break;
                    }
                }
            }
            if calls % 100 == 0 && calls > 0 {
                eprintln!(
                    "[kalshi] series progress: {} calls, {} markets so far",
                    calls,
                    all_markets.len()
                );
            }
        }

        series_backfill_calls = calls;
        series_backfill_truncated = Some(considered_series > series_cap);
        markets_exhausted = considered_series <= series_cap;

        eprintln!(
            "[kalshi] series fetch done: {} calls, {} unique markets",
            calls,
            all_markets.len()
        );
    }

    // Phase 4: Assign categories via event→category map and series prefix matching.
    let mut direct_hits = 0usize;
    let mut series_hits = 0usize;
    for m in &mut all_markets {
        if m.category.is_none() {
            if let Some(cat) = category_map.get(&m.event_ticker) {
                m.category = Some(cat.clone());
                direct_hits += 1;
            } else {
                let mut best: Option<(&str, &str)> = None;
                for (series, cat) in &series_category_map {
                    if m.event_ticker.starts_with(series.as_str()) {
                        if best.map_or(true, |(bs, _)| series.len() > bs.len()) {
                            best = Some((series.as_str(), cat.as_str()));
                        }
                    }
                }
                if let Some((_, cat)) = best {
                    m.category = Some(cat.to_string());
                    series_hits += 1;
                }
            }
        }
    }

    // Phase 5: Final sports filter (belt and suspenders).
    let pre_filter = all_markets.len();
    all_markets.retain(|m| !m.category.as_deref().map_or(false, is_sports_category));
    let sports_dropped = pre_filter - all_markets.len();

    eprintln!(
        "[kalshi] sync complete: {} markets, {} events (dropped {} sports markets)",
        all_markets.len(),
        all_events.len(),
        sports_dropped,
    );

    let mut strict_fail_reasons = Vec::new();
    if !events_exhausted {
        strict_fail_reasons.push("events pagination not exhausted".to_string());
    }
    if !markets_exhausted {
        strict_fail_reasons
            .push("series-based market fetch hit cap before all series covered".to_string());
    }
    if series_backfill_truncated.unwrap_or(false) {
        strict_fail_reasons
            .push("series backfill hit cap before covering all discovered series".to_string());
    }

    let coverage = OddsSyncCoverage {
        requested_max_pages: max_pages,
        events_pages_fetched,
        events_exhausted,
        markets_pages_fetched,
        markets_exhausted,
        series_backfill_calls: Some(series_backfill_calls),
        series_backfill_cap,
        series_backfill_truncated,
        strict_pass: strict_fail_reasons.is_empty(),
        strict_fail_reasons,
    };

    Ok((all_events, all_markets, coverage))
}
