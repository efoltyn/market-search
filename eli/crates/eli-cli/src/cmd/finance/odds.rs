async fn cmd_finance_odds(args: FinanceOddsArgs) -> Result<()> {
    if let Some(action) = args.action {
        match action {
            FinanceOddsAction::Sync(sync_args) => return cmd_finance_sync(sync_args).await,
            FinanceOddsAction::Where(where_args) => return cmd_finance_odds_where(where_args),
        }
    }

    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    // When --search is provided alone (no --list-events/--list-markets/ticker),
    // search the local CSV cache from `eli finance sync` instead of hitting the API.
    let has_search = args.search.is_some();
    let has_list_or_ticker = args.list_events
        || args.list_markets
        || args.list_series
        || args.list_tags
        || args.series.is_some()
        || args.event.is_some()
        || args.market.is_some();

    if has_search && !has_list_or_ticker {
        let search_opts = CsvSearchOptions::from_cli(
            &args.sort_by,
            args.deltas_only,
            args.min_delta_pp,
            args.category.as_deref(),
        )?;

        // Check if local CSV cache exists
        let cache_dir = directories::ProjectDirs::from("", "", "eli")
            .map(|d| d.cache_dir().join("odds"))
            .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));
        let csv_path = cache_dir.join("all_markets.csv");

        if csv_path.exists() && !args.live {
            // CSV exists — search locally (instant, no API calls)
            return cmd_finance_odds_search_csv(
                args.search.as_deref().unwrap_or(""),
                args.limit,
                args.country.as_deref(),
                args.min_volume,
                args.top,
                args.explain,
                &search_opts,
            );
        }

        if csv_path.exists() && args.live {
            // CSV exists + --live: search CSV for tickers, then fetch fresh prices
            return cmd_finance_odds_search_live(
                args.search.as_deref().unwrap_or(""),
                &csv_path,
                args.limit,
                args.country.as_deref(),
                args.min_volume,
                args.top,
                args.explain,
                &search_opts,
            )
            .await;
        }

        if search_opts.requires_delta_index() {
            anyhow::bail!(
                "delta-aware CSV search requested (sort/filter by delta), but local cache is missing at {}. Run `eli finance sync` first.",
                csv_path.display()
            );
        }

        // No CSV — fall back to live API search (Kalshi events → markets)
        eprintln!(
            "no local CSV cache; falling back to live API search for {:?}",
            args.search.as_deref().unwrap_or("")
        );
        return cmd_finance_odds_search_live_no_csv(
            args.search.as_deref().unwrap_or(""),
            args.limit,
            args.top,
        )
        .await;
    }

    let provider = args
        .provider
        .as_ref()
        .map(|s| s.trim().to_ascii_lowercase());
    let provider = match provider {
        None => None,
        Some(p) if p.is_empty() => None,
        Some(p) => match p.as_str() {
            "kalshi" | "polymarket" | "auto" => Some(p),
            other => anyhow::bail!(
                "unsupported --provider '{other}' (supported: kalshi, polymarket, auto)"
            ),
        },
    };

    let req = eli_core::finance::OddsRequest {
        provider,
        disable_kalshi: false,
        series_ticker: args.series,
        event_ticker: args.event,
        market_ticker: args.market,
        status: args.status,
        limit: args.limit,
        cursor: args.cursor,
        max_pages: args.max_pages,
        include_orderbook: args.orderbook,
        orderbook_depth: args.depth,
        list_series: args.list_series,
        list_events: args.list_events,
        list_markets: args.list_markets,
        list_tags: args.list_tags,
        category: args.category,
        search: args.search,
    };

    let resp = eli_core::finance::fetch_odds(req.clone())
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch odds")?;
    let enriched_resp =
        enrich_odds_response_with_sync_delta(&resp, req.provider.as_deref())?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &enriched_resp,
            "finance.odds",
            &[format!(
                "provider={}",
                args.provider.clone().unwrap_or_default()
            )],
        )?;
        let prediction_markets_path = prediction_markets_path_for_output(&wr.out_path);
        update_prediction_markets(&prediction_markets_path, &req, &resp, Some(&wr.out_path))
            .context("update prediction markets")?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{},\"prediction_markets_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&prediction_markets_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string())
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&enriched_resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

#[derive(Clone)]
struct SyncDeltaLookup {
    by_market: std::collections::HashMap<String, eli_core::finance::OddsSyncMarketDelta>,
    context: serde_json::Value,
}

fn sync_delta_key(source: &str, ticker: &str) -> String {
    format!(
        "{}::{}",
        source.trim().to_ascii_lowercase(),
        ticker.trim().to_ascii_uppercase()
    )
}

fn load_sync_delta_lookup(cache_dir: &std::path::Path) -> Option<SyncDeltaLookup> {
    let path = cache_dir.join("sync_last_delta.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let parsed: eli_core::finance::OddsSyncDeltaIndex = serde_json::from_str(&raw).ok()?;

    let mut by_market = std::collections::HashMap::new();
    for (key, delta) in parsed.market_deltas {
        by_market.insert(key, delta.clone());
        by_market.insert(sync_delta_key(&delta.source, &delta.ticker), delta);
    }

    let context = serde_json::json!({
        "available": true,
        "path": path.display().to_string(),
        "previous_sync_at": parsed.previous_sync_at,
        "current_sync_at": parsed.current_sync_at,
        "changed_markets": parsed.changed_markets,
        "top_probability_moves": parsed.top_probability_moves,
        "top_yes_price_moves": parsed.top_yes_price_moves,
        "top_volume_moves": parsed.top_volume_moves,
    });
    Some(SyncDeltaLookup { by_market, context })
}

fn attach_market_delta(
    row_json: &mut serde_json::Value,
    source: &str,
    ticker: &str,
    lookup: Option<&SyncDeltaLookup>,
) {
    let Some(lookup) = lookup else {
        return;
    };
    let key = sync_delta_key(source, ticker);
    if let Some(delta) = lookup.by_market.get(&key) {
        if let Ok(delta_value) = serde_json::to_value(delta) {
            row_json["delta_since_last_sync"] = delta_value;
        }
    }
}

fn enrich_odds_response_with_sync_delta(
    resp: &eli_core::finance::OddsResponse,
    provider_hint: Option<&str>,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(resp).context("serialize odds response")?;

    let cache_dir = directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));
    let Some(lookup) = load_sync_state_lookup(&cache_dir) else {
        return Ok(value);
    };

    let fallback_source = provider_hint.and_then(|p| match p {
        "kalshi" | "polymarket" => Some(p),
        _ => None,
    });

    let mut attached = 0usize;
    for field in ["markets", "available_markets"] {
        let Some(items) = value.get_mut(field).and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for item in items {
            let source = item
                .get("source")
                .and_then(|v| v.as_str())
                .or(fallback_source);
            let ticker = item.get("ticker").and_then(|v| v.as_str());
            let (Some(source), Some(ticker)) = (source, ticker) else {
                continue;
            };
            let key = sync_delta_key(source, ticker);
            let Some(previous) = lookup.by_market.get(&key) else {
                continue;
            };
            let current_probability_yes = item
                .get("probability_yes")
                .and_then(json_to_f64)
                .or_else(|| item.get("probability").and_then(json_to_f64));
            let current_yes_price = item.get("yes_price").and_then(json_to_i64);
            let current_volume = item.get("volume").and_then(json_to_i64);
            let current_status = item
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let volume_comparable =
                volume_scales_look_comparable(previous.volume, current_volume);
            let comparable_current_volume = if volume_comparable {
                current_volume
            } else {
                previous.volume
            };

            let probability_delta =
                option_f64_delta(previous.probability_yes, current_probability_yes)
                    .filter(|d| d.abs() > 0.0001);
            let yes_price_delta = option_i64_delta(previous.yes_price, current_yes_price);
            let volume_delta = if volume_comparable {
                option_i64_delta(previous.volume, current_volume)
            } else {
                None
            };
            let status_changed = previous.status != current_status;

            if probability_delta.is_none()
                && yes_price_delta.is_none()
                && volume_delta.is_none()
                && !status_changed
            {
                continue;
            }

            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| previous.title.clone());
            let event_ticker = item
                .get("event_ticker")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| previous.event_ticker.clone());
            let category = item
                .get("category")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty())
                .or_else(|| previous.category.clone());

            let delta = eli_core::finance::OddsSyncMarketDelta {
                source: source.to_string(),
                ticker: ticker.to_string(),
                title,
                event_ticker,
                category,
                change_kind: "updated".to_string(),
                previous_probability_yes: previous.probability_yes,
                current_probability_yes,
                probability_delta,
                probability_delta_pct_points: probability_delta.map(|d| d * 100.0),
                previous_yes_price: previous.yes_price,
                current_yes_price,
                yes_price_delta,
                previous_volume: previous.volume,
                current_volume: comparable_current_volume,
                volume_delta,
                previous_status: previous.status.clone(),
                current_status,
            };

            if let Ok(delta_value) = serde_json::to_value(delta) {
                item["delta_since_last_sync"] = delta_value;
                attached = attached.saturating_add(1);
            }
        }
    }

    // Avoid noisy null-ish metadata: only include context when at least one
    // market was enriched with a delta payload.
    if attached > 0 {
        value["delta_context"] = serde_json::json!({
            "available": true,
            "source": "sync_state_snapshot",
            "path": lookup.path,
            "sync_at": lookup.sync_at,
            "attached_markets": attached,
        });
    }

    Ok(value)
}

#[derive(Clone)]
struct SyncStateLookup {
    by_market: std::collections::HashMap<String, SyncStateMarket>,
    sync_at: Option<String>,
    path: String,
}

#[derive(Clone)]
struct SyncStateMarket {
    title: String,
    event_ticker: String,
    category: Option<String>,
    probability_yes: Option<f64>,
    yes_price: Option<i64>,
    volume: Option<i64>,
    status: Option<String>,
}

#[derive(serde::Deserialize)]
struct SyncStateFile {
    #[serde(default)]
    last_sync_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    sources: std::collections::HashMap<String, SyncStateSource>,
}

#[derive(serde::Deserialize)]
struct SyncStateSource {
    #[serde(default)]
    markets: std::collections::HashMap<String, SyncStateMarketRecord>,
}

#[derive(serde::Deserialize)]
struct SyncStateMarketRecord {
    ticker: String,
    title: String,
    event_ticker: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    probability_yes: Option<f64>,
    #[serde(default)]
    yes_price: Option<i64>,
    #[serde(default)]
    volume: Option<i64>,
    #[serde(default)]
    status: Option<String>,
}

fn load_sync_state_lookup(cache_dir: &std::path::Path) -> Option<SyncStateLookup> {
    let path = cache_dir.join("sync_state.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let parsed: SyncStateFile = serde_json::from_str(&raw).ok()?;

    let mut by_market = std::collections::HashMap::new();
    for (source, source_state) in parsed.sources {
        let source_norm = source.trim().to_ascii_lowercase();
        for (ticker_key, market) in source_state.markets {
            let ticker = if market.ticker.trim().is_empty() {
                ticker_key
            } else {
                market.ticker
            };
            let key = sync_delta_key(&source_norm, &ticker);
            by_market.insert(
                key,
                SyncStateMarket {
                    title: market.title,
                    event_ticker: market.event_ticker,
                    category: market.category,
                    probability_yes: market.probability_yes,
                    yes_price: market.yes_price,
                    volume: market.volume,
                    status: market.status,
                },
            );
        }
    }

    Some(SyncStateLookup {
        by_market,
        sync_at: parsed.last_sync_at.map(|d| d.to_rfc3339()),
        path: path.display().to_string(),
    })
}

fn json_to_f64(value: &serde_json::Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_i64().map(|v| v as f64))
}

fn json_to_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|v| v.round() as i64))
}

fn option_f64_delta(previous: Option<f64>, current: Option<f64>) -> Option<f64> {
    match (previous, current) {
        (Some(p), Some(c)) => Some(c - p),
        (None, Some(c)) => Some(c),
        (Some(p), None) => Some(-p),
        (None, None) => None,
    }
}

fn option_i64_delta(previous: Option<i64>, current: Option<i64>) -> Option<i64> {
    match (previous, current) {
        (Some(p), Some(c)) if p != c => Some(c - p),
        (None, Some(c)) if c != 0 => Some(c),
        (Some(p), None) if p != 0 => Some(-p),
        (None, None) => None,
        _ => None,
    }
}

fn volume_scales_look_comparable(previous: Option<i64>, current: Option<i64>) -> bool {
    let (Some(previous), Some(current)) = (previous, current) else {
        return true;
    };
    if previous <= 0 || current <= 0 {
        return true;
    }
    let ratio = (current as f64) / (previous as f64);
    (0.1..=10.0).contains(&ratio)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CsvSortBy {
    Relevance,
    Volume,
    DeltaProb,
    DeltaYesPrice,
    DeltaVolume,
}

impl CsvSortBy {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "relevance" => Ok(Self::Relevance),
            "volume" => Ok(Self::Volume),
            "delta_prob" | "delta-prob" | "delta_probability" | "delta-probability" => {
                Ok(Self::DeltaProb)
            }
            "delta_yes_price" | "delta-yes-price" | "delta_price" | "delta-price" => {
                Ok(Self::DeltaYesPrice)
            }
            "delta_volume" | "delta-volume" => Ok(Self::DeltaVolume),
            other => anyhow::bail!(
                "invalid --sort-by '{other}' (expected relevance|volume|delta_prob|delta_yes_price|delta_volume)"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Volume => "volume",
            Self::DeltaProb => "delta_prob",
            Self::DeltaYesPrice => "delta_yes_price",
            Self::DeltaVolume => "delta_volume",
        }
    }

    fn uses_delta(self) -> bool {
        matches!(self, Self::DeltaProb | Self::DeltaYesPrice | Self::DeltaVolume)
    }
}

#[derive(Clone)]
struct CsvSearchOptions {
    sort_by: CsvSortBy,
    deltas_only: bool,
    min_delta_pp: Option<f64>,
    category_filter: Option<String>,
}

impl CsvSearchOptions {
    fn from_cli(
        sort_by_raw: &str,
        deltas_only: bool,
        min_delta_pp: Option<f64>,
        category_filter: Option<&str>,
    ) -> Result<Self> {
        let sort_by = CsvSortBy::parse(sort_by_raw)?;
        if let Some(v) = min_delta_pp {
            if !v.is_finite() || v < 0.0 {
                anyhow::bail!("--min-delta-pp must be a non-negative number");
            }
        }
        let category_filter = category_filter
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty());
        Ok(Self {
            sort_by,
            deltas_only,
            min_delta_pp,
            category_filter,
        })
    }

    fn requires_delta_index(&self) -> bool {
        self.sort_by.uses_delta() || self.deltas_only || self.min_delta_pp.is_some()
    }
}

#[derive(Clone, Copy, Default)]
struct DeltaMetrics {
    has_delta: bool,
    probability_pp_abs: f64,
    yes_price_abs: f64,
    volume_abs: f64,
}

fn delta_metrics(source: &str, ticker: &str, lookup: Option<&SyncDeltaLookup>) -> DeltaMetrics {
    let Some(lookup) = lookup else {
        return DeltaMetrics::default();
    };
    let key = sync_delta_key(source, ticker);
    let Some(delta) = lookup.by_market.get(&key) else {
        return DeltaMetrics::default();
    };
    DeltaMetrics {
        has_delta: true,
        probability_pp_abs: delta.probability_delta_pct_points.unwrap_or(0.0).abs(),
        yes_price_abs: delta.yes_price_delta.unwrap_or(0).unsigned_abs() as f64,
        volume_abs: delta.volume_delta.unwrap_or(0).unsigned_abs() as f64,
    }
}

fn category_matches_filter(row: &str, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    row.to_ascii_lowercase().contains(filter)
}

#[derive(Clone)]
struct SearchCandidate {
    row_json: serde_json::Value,
    match_score: i64,
    volume_usd: f64,
    delta: DeltaMetrics,
}

fn sort_search_candidates(candidates: &mut [SearchCandidate], sort_by: CsvSortBy) {
    candidates.sort_by(|a, b| {
        let delta_prob_cmp = b
            .delta
            .probability_pp_abs
            .partial_cmp(&a.delta.probability_pp_abs)
            .unwrap_or(std::cmp::Ordering::Equal);
        let delta_yes_price_cmp = b
            .delta
            .yes_price_abs
            .partial_cmp(&a.delta.yes_price_abs)
            .unwrap_or(std::cmp::Ordering::Equal);
        let delta_volume_cmp = b
            .delta
            .volume_abs
            .partial_cmp(&a.delta.volume_abs)
            .unwrap_or(std::cmp::Ordering::Equal);
        let volume_cmp = b
            .volume_usd
            .partial_cmp(&a.volume_usd)
            .unwrap_or(std::cmp::Ordering::Equal);

        match sort_by {
            CsvSortBy::Relevance => b.match_score.cmp(&a.match_score).then_with(|| volume_cmp),
            CsvSortBy::Volume => volume_cmp.then_with(|| b.match_score.cmp(&a.match_score)),
            CsvSortBy::DeltaProb => delta_prob_cmp
                .then_with(|| b.match_score.cmp(&a.match_score))
                .then_with(|| volume_cmp),
            CsvSortBy::DeltaYesPrice => delta_yes_price_cmp
                .then_with(|| b.match_score.cmp(&a.match_score))
                .then_with(|| volume_cmp),
            CsvSortBy::DeltaVolume => delta_volume_cmp
                .then_with(|| b.match_score.cmp(&a.match_score))
                .then_with(|| volume_cmp),
        }
    });
}

/// Search the local prediction market CSV cache (from `eli finance sync`).
/// Returns matching markets as JSON, sorted by caller-selected ranking.
fn cmd_finance_odds_search_csv(
    query: &str,
    limit: Option<usize>,
    country: Option<&str>,
    min_volume_usd: Option<f64>,
    top: Option<usize>,
    explain: bool,
    opts: &CsvSearchOptions,
) -> Result<()> {
    #[derive(Deserialize)]
    struct OddsCsvRow {
        source: String,
        ticker: String,
        title: String,
        event_ticker: String,
        yes_price: String,
        volume: String,
        status: String,
        probability: String,
        category: String,
        topic: String,
    }

    fn contains_keyword(haystack: &str, keyword: &str) -> bool {
        if keyword.contains(' ') || keyword.contains('.') {
            return haystack.contains(keyword);
        }
        haystack
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|tok| tok == keyword)
    }

    fn find_us_hints(row: &OddsCsvRow) -> Vec<String> {
        let text = format!(
            "{} {} {} {} {}",
            row.title, row.event_ticker, row.category, row.topic, row.ticker
        )
        .to_ascii_lowercase();
        let keywords = [
            "us",
            "u.s.",
            "united states",
            "american",
            "nfp",
            "nonfarm payrolls",
            "fomc",
            "federal reserve",
            "cpi",
            "pce",
            "gdpnow",
        ];
        keywords
            .iter()
            .filter(|k| contains_keyword(&text, k))
            .map(|k| k.to_string())
            .collect()
    }

    fn compile_term_patterns(terms: &[String]) -> Vec<(String, regex::Regex)> {
        terms
            .iter()
            .filter_map(|t| {
                regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t)))
                    .ok()
                    .map(|re| (t.clone(), re))
            })
            .collect()
    }

    fn compute_match_terms(text: &str, term_patterns: &[(String, regex::Regex)]) -> Vec<String> {
        term_patterns
            .iter()
            .filter_map(|(term, re)| re.is_match(text).then_some(term.clone()))
            .collect()
    }

    fn has_phrase_match(text: &str, phrase_pattern: &Option<regex::Regex>) -> bool {
        phrase_pattern.as_ref().is_some_and(|re| re.is_match(text))
    }

    fn compute_match_score(row: &OddsCsvRow, query: &str, matched_terms: &[String], volume_usd: f64) -> i64 {
        let q = query.to_ascii_lowercase();
        let title = row.title.to_ascii_lowercase();
        let ticker = row.ticker.to_ascii_lowercase();
        let event = row.event_ticker.to_ascii_lowercase();
        let category = row.category.to_ascii_lowercase();
        let topic = row.topic.to_ascii_lowercase();

        let mut score = 0.0f64;
        if !q.is_empty() && title.contains(&q) {
            score += 30.0;
        }
        for t in matched_terms {
            if title.contains(t) {
                score += 10.0;
            }
            if ticker.contains(t) || event.contains(t) {
                score += 6.0;
            }
            if category.contains(t) || topic.contains(t) {
                score += 4.0;
            }
        }
        score += (matched_terms.len() as f64) * 8.0;
        score += (volume_usd.max(0.0) + 1.0).log10() * 3.0;
        score.round() as i64
    }

    fn explain_reasons(
        row: &OddsCsvRow,
        query: &str,
        matched_terms: &[String],
        volume_usd: f64,
    ) -> Vec<String> {
        let mut reasons = Vec::new();
        let query_l = query.to_ascii_lowercase();
        let title_l = row.title.to_ascii_lowercase();
        if !query_l.is_empty() && title_l.contains(&query_l) {
            reasons.push("title contains full query".to_string());
        }
        if !matched_terms.is_empty() {
            reasons.push(format!("matched terms: {}", matched_terms.join(", ")));
        }
        reasons.push(format!("volume_usd={volume_usd:.2}"));
        reasons.truncate(3);
        reasons
    }

    let cache_dir = directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));
    let delta_index_path = cache_dir.join("sync_last_delta.json");
    let delta_lookup = load_sync_delta_lookup(&cache_dir);
    if opts.requires_delta_index() && delta_lookup.is_none() {
        anyhow::bail!(
            "delta-aware search requested, but sync delta index is missing at {}. Run `eli finance sync` first.",
            delta_index_path.display()
        );
    }
    let delta_context = delta_lookup
        .as_ref()
        .map(|d| d.context.clone())
        .unwrap_or_else(|| serde_json::json!({ "available": false }));

    let csv_path = cache_dir.join("all_markets.csv");
    if !csv_path.exists() {
        anyhow::bail!(
            "no local prediction market cache found at {}. Run `eli finance sync` first.",
            csv_path.display()
        );
    }

    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let match_all = query == "*" || query.eq_ignore_ascii_case("all");
    let query_lower = query.to_ascii_lowercase();
    let terms: Vec<String> = if match_all {
        Vec::new()
    } else {
        query_lower
            .split_whitespace()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    if !match_all && terms.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let term_patterns = compile_term_patterns(&terms);
    let phrase_pattern = {
        if match_all {
            None
        } else {
            let q = query.trim();
            if q.is_empty() {
                None
            } else {
                regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(q))).ok()
            }
        }
    };

    let country_normalized = country.map(|c| c.trim().to_ascii_uppercase());
    if let Some(ref c) = country_normalized {
        if c != "US" {
            anyhow::bail!("unsupported --country '{c}' (v1 supports: US)");
        }
    }

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(&csv_path)
        .with_context(|| format!("open {}", csv_path.display()))?;

    let mut matches: Vec<SearchCandidate> = Vec::new();
    let mut loose_matches: Vec<SearchCandidate> = Vec::new();
    let mut semantic_query_expanded = false;
    let federal_reserve_policy_query = !match_all && query_lower.contains("federal reserve");
    let fed_context_terms = [
        "fed",
        "federal reserve",
        "fomc",
        "fed funds",
        "interest rate",
    ];
    let policy_action_terms = [
        "rate", "cut", "cuts", "hike", "hikes", "hold", "decrease", "increase", "bps",
        "basis point", "decision", "meeting",
    ];
    for row in rdr.deserialize::<OddsCsvRow>() {
        let row = match row {
            Ok(r) => r,
            Err(_) => continue,
        };
        let searchable = format!(
            "{} {} {} {} {} {}",
            row.source, row.ticker, row.title, row.event_ticker, row.category, row.topic
        );
        let searchable_lower = searchable.to_ascii_lowercase();
        let mut matched_terms = if match_all {
            Vec::new()
        } else {
            compute_match_terms(&searchable, &term_patterns)
        };
        let mut expanded_match = false;
        let has_fed_context = fed_context_terms.iter().any(|t| searchable_lower.contains(t));
        let has_policy_action = policy_action_terms.iter().any(|t| searchable_lower.contains(t));
        let policy_like = has_fed_context && has_policy_action;
        if !match_all && matched_terms.is_empty() {
            if federal_reserve_policy_query && policy_like {
                matched_terms.push("fed_policy_expanded".to_string());
                semantic_query_expanded = true;
                expanded_match = true;
            } else {
                continue;
            }
        }
        if !match_all && terms.len() >= 2 {
            let matched_unique: std::collections::BTreeSet<String> =
                matched_terms.iter().cloned().collect();
            if matched_unique.len() < 2
                && !has_phrase_match(&searchable, &phrase_pattern)
                && !expanded_match
            {
                continue;
            }
        }

        let country_hints = find_us_hints(&row);
        if country_normalized.as_deref() == Some("US") && country_hints.is_empty() {
            continue;
        }

        let category_searchable = format!("{} {}", row.category, row.topic);
        if !category_matches_filter(&category_searchable, opts.category_filter.as_deref()) {
            continue;
        }

        let vol_cents: f64 = row.volume.trim().parse().unwrap_or(0.0);
        let volume_usd = vol_cents / 100.0;
        if let Some(min_usd) = min_volume_usd {
            if volume_usd < min_usd {
                continue;
            }
        }

        let yes_price: f64 = row.yes_price.trim().parse().unwrap_or(0.0);
        let prob: f64 = row.probability.trim().parse().unwrap_or(0.0);
        let phrase_boost = if has_phrase_match(&searchable, &phrase_pattern) {
            7
        } else {
            0
        };
        let match_score =
            compute_match_score(&row, query, &matched_terms, volume_usd) + phrase_boost;
        let source_for_delta = row.source.clone();
        let ticker_for_delta = row.ticker.clone();
        let delta = delta_metrics(&source_for_delta, &ticker_for_delta, delta_lookup.as_ref());
        if opts.deltas_only && !delta.has_delta {
            continue;
        }
        if let Some(min_pp) = opts.min_delta_pp {
            if delta.probability_pp_abs < min_pp {
                continue;
            }
        }
        let mut row_json = serde_json::json!({
            "source": row.source,
            "ticker": row.ticker,
            "title": row.title,
            "event_ticker": row.event_ticker,
            "yes_price": yes_price,
            "volume": vol_cents,
            "volume_usd": volume_usd,
            "status": row.status,
            "probability": prob,
            "category": row.category,
            "topic": row.topic,
            "match_score": match_score,
            "match_terms": matched_terms,
            "country_hints": country_hints,
        });
        if explain {
            let matched_terms_vec: Vec<String> = row_json["match_terms"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            row_json["explain"] = serde_json::json!({
                "reasons": explain_reasons(&row, query, &matched_terms_vec, volume_usd)
            });
        }
        attach_market_delta(
            &mut row_json,
            &source_for_delta,
            &ticker_for_delta,
            delta_lookup.as_ref(),
        );
        let candidate = SearchCandidate {
            row_json,
            match_score,
            volume_usd,
            delta,
        };
        if federal_reserve_policy_query && !policy_like {
            loose_matches.push(candidate);
            continue;
        }
        matches.push(candidate);
    }

    let mut semantic_filter_relaxed = false;
    if federal_reserve_policy_query && matches.is_empty() && !loose_matches.is_empty() {
        semantic_filter_relaxed = true;
        matches = loose_matches;
    }

    sort_search_candidates(&mut matches, opts.sort_by);

    let total_matches = matches.len();
    let final_limit = top.or(limit).unwrap_or(25);
    matches.truncate(final_limit);
    let markets: Vec<serde_json::Value> = matches.into_iter().map(|m| m.row_json).collect();

    let resp = serde_json::json!({
        "query": query,
        "source": "local_csv_cache",
        "csv_path": csv_path.display().to_string(),
        "total_matches": total_matches,
        "returned_matches": markets.len(),
        "limit": final_limit,
        "sort_by": opts.sort_by.as_str(),
        "deltas_only": opts.deltas_only,
        "min_delta_pp": opts.min_delta_pp,
        "category_filter": opts.category_filter.clone(),
        "match_all": match_all,
        "country": country_normalized,
        "min_volume_usd": min_volume_usd,
        "top": top,
        "semantic_filter_relaxed": semantic_filter_relaxed,
        "semantic_query_expanded": semantic_query_expanded,
        "delta_context": delta_context,
        "markets": markets,
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize search results")?;
    println!("{json}");
    Ok(())
}

/// No CSV available — fall back to live API: search Kalshi events, then fetch markets
/// for matched events. Also queries Polymarket. Returns combined results.
async fn cmd_finance_odds_search_live_no_csv(
    query: &str,
    limit: Option<usize>,
    top: Option<usize>,
) -> Result<()> {
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let cache_dir = directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("odds"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));
    let delta_lookup = load_sync_delta_lookup(&cache_dir);
    let delta_context = delta_lookup
        .as_ref()
        .map(|d| d.context.clone())
        .unwrap_or_else(|| serde_json::json!({ "available": false }));

    // Step 1: Search Kalshi events
    let kalshi_events_req = eli_core::finance::OddsRequest {
        provider: Some("kalshi".to_string()),
        disable_kalshi: false,
        series_ticker: None,
        event_ticker: None,
        market_ticker: None,
        status: Some("open".to_string()),
        limit: Some(200),
        cursor: None,
        max_pages: Some(3),
        include_orderbook: false,
        orderbook_depth: None,
        list_series: false,
        list_events: true,
        list_markets: false,
        list_tags: false,
        category: None,
        search: Some(query.to_string()),
    };

    // Step 2: Search Polymarket events
    let poly_events_req = eli_core::finance::OddsRequest {
        provider: Some("polymarket".to_string()),
        ..kalshi_events_req.clone()
    };

    let (kalshi_resp, poly_resp) = tokio::join!(
        eli_core::finance::fetch_odds(kalshi_events_req),
        eli_core::finance::fetch_odds(poly_events_req),
    );

    let mut all_events: Vec<serde_json::Value> = Vec::new();

    // Collect Kalshi events
    if let Ok(resp) = &kalshi_resp {
        if let Some(events) = &resp.available_events {
            for e in events {
                all_events.push(serde_json::json!({
                    "source": "kalshi",
                    "event_ticker": e.ticker,
                    "title": e.title,
                    "category": e.category,
                    "series_ticker": e.series_ticker,
                }));
            }
        }
    }

    // Collect Polymarket events
    if let Ok(resp) = &poly_resp {
        if let Some(events) = &resp.available_events {
            for e in events {
                all_events.push(serde_json::json!({
                    "source": "polymarket",
                    "event_ticker": e.ticker,
                    "title": e.title,
                    "category": e.category,
                    "slug": e.slug,
                }));
            }
        }
    }

    // Step 3: For the top events, fetch their markets with live prices
    let final_limit = top.or(limit).unwrap_or(10);
    all_events.truncate(final_limit.min(15)); // cap at 15 events to avoid rate limits

    let mut live_markets: Vec<serde_json::Value> = Vec::new();

    for event_json in &all_events {
        let source = event_json["source"].as_str().unwrap_or("");
        let event_ticker = event_json["event_ticker"].as_str().unwrap_or("");
        if event_ticker.is_empty() {
            continue;
        }

        let market_req = eli_core::finance::OddsRequest {
            provider: Some(source.to_string()),
            disable_kalshi: source == "polymarket",
            series_ticker: None,
            event_ticker: Some(event_ticker.to_string()),
            market_ticker: None,
            status: None,
            limit: None,
            cursor: None,
            max_pages: None,
            include_orderbook: false,
            orderbook_depth: None,
            list_series: false,
            list_events: false,
            list_markets: false,
            list_tags: false,
            category: None,
            search: None,
        };

        if let Ok(resp) = eli_core::finance::fetch_odds(market_req).await {
            for m in &resp.markets {
                let mut market_json = serde_json::json!({
                    "source": source,
                    "ticker": m.ticker,
                    "title": m.title,
                    "event_ticker": m.event_ticker,
                    "yes_price": m.yes_price,
                    "yes_bid": m.yes_bid,
                    "yes_ask": m.yes_ask,
                    "volume": m.volume,
                    "volume_usd": m.volume.map(|v| v as f64 / 100.0),
                    "status": m.status,
                    "probability": m.probability_yes,
                });
                attach_market_delta(
                    &mut market_json,
                    source,
                    &m.ticker,
                    delta_lookup.as_ref(),
                );
                live_markets.push(market_json);
            }
        }
    }

    let resp = serde_json::json!({
        "query": query,
        "source": "live_api",
        "note": "no local CSV cache; results fetched from live Kalshi + Polymarket APIs",
        "events_found": all_events.len(),
        "events": all_events,
        "markets": live_markets,
        "total_markets": live_markets.len(),
        "delta_context": delta_context,
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize live search results")?;
    println!("{json}");
    Ok(())
}

/// CSV exists + --live: search CSV for event tickers, then fetch fresh prices from API.
async fn cmd_finance_odds_search_live(
    query: &str,
    csv_path: &std::path::Path,
    limit: Option<usize>,
    country: Option<&str>,
    min_volume_usd: Option<f64>,
    top: Option<usize>,
    _explain: bool,
    opts: &CsvSearchOptions,
) -> Result<()> {
    // First, run the normal CSV search to find matching events
    // We read the CSV and extract unique event_tickers + source
    let query_trimmed = query.trim();
    if query_trimmed.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let match_all = query_trimmed == "*" || query_trimmed.eq_ignore_ascii_case("all");
    let query_lower = query_trimmed.to_ascii_lowercase();
    let terms: Vec<String> = if match_all {
        Vec::new()
    } else {
        query_lower
            .split_whitespace()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    if !match_all && terms.is_empty() {
        anyhow::bail!("--search query cannot be empty");
    }
    let cache_dir = csv_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let delta_index_path = cache_dir.join("sync_last_delta.json");
    let delta_lookup = load_sync_delta_lookup(cache_dir);
    if opts.requires_delta_index() && delta_lookup.is_none() {
        anyhow::bail!(
            "delta-aware search requested, but sync delta index is missing at {}. Run `eli finance sync` first.",
            delta_index_path.display()
        );
    }
    let delta_context = delta_lookup
        .as_ref()
        .map(|d| d.context.clone())
        .unwrap_or_else(|| serde_json::json!({ "available": false }));

    // Build regex patterns for matching
    let term_patterns: Vec<(String, regex::Regex)> = terms
        .iter()
        .filter_map(|t| {
            regex::Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t)))
                .ok()
                .map(|re| (t.clone(), re))
        })
        .collect();

    #[derive(serde::Deserialize)]
    struct OddsCsvRow {
        source: String,
        ticker: String,
        title: String,
        event_ticker: String,
        yes_price: String,
        volume: String,
        status: String,
        probability: String,
        category: String,
        topic: String,
    }

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(csv_path)
        .with_context(|| format!("open {}", csv_path.display()))?;

    #[derive(Clone)]
    struct LiveEventAggregate {
        source: String,
        category: String,
        total_volume_usd: f64,
        delta_prob_pp_abs_max: f64,
        delta_yes_price_abs_max: f64,
        delta_volume_abs_max: f64,
    }

    // Find unique event_tickers that match the query
    let mut event_map: std::collections::BTreeMap<String, LiveEventAggregate> =
        std::collections::BTreeMap::new();

    for row in rdr.deserialize::<OddsCsvRow>() {
        let row = match row {
            Ok(r) => r,
            Err(_) => continue,
        };
        let searchable = format!(
            "{} {} {} {} {} {}",
            row.source, row.ticker, row.title, row.event_ticker, row.category, row.topic
        );
        if !match_all {
            let matched: Vec<String> = term_patterns
                .iter()
                .filter_map(|(term, re)| re.is_match(&searchable).then_some(term.clone()))
                .collect();
            if matched.is_empty() {
                continue;
            }
            if terms.len() >= 2 {
                let unique: std::collections::BTreeSet<&String> = matched.iter().collect();
                if unique.len() < 2 {
                    continue;
                }
            }
        }

        // Apply country filter
        if let Some("US") = country {
            let text = format!(
                "{} {} {} {}",
                row.title, row.event_ticker, row.category, row.ticker
            )
            .to_ascii_lowercase();
            let us_terms = [
                "us", "u.s.", "united states", "american", "fomc", "federal reserve", "cpi", "pce",
            ];
            if !us_terms.iter().any(|t| text.contains(t)) {
                continue;
            }
        }
        let category_searchable = format!("{} {}", row.category, row.topic);
        if !category_matches_filter(&category_searchable, opts.category_filter.as_deref()) {
            continue;
        }

        let vol_cents: f64 = row.volume.trim().parse().unwrap_or(0.0);
        let volume_usd = vol_cents / 100.0;
        if let Some(min_usd) = min_volume_usd {
            if volume_usd < min_usd {
                continue;
            }
        }
        let delta = delta_metrics(&row.source, &row.ticker, delta_lookup.as_ref());
        if opts.deltas_only && !delta.has_delta {
            continue;
        }
        if let Some(min_pp) = opts.min_delta_pp {
            if delta.probability_pp_abs < min_pp {
                continue;
            }
        }

        let entry = event_map
            .entry(row.event_ticker.clone())
            .or_insert_with(|| LiveEventAggregate {
                source: row.source.clone(),
                category: row.category.clone(),
                total_volume_usd: 0.0,
                delta_prob_pp_abs_max: 0.0,
                delta_yes_price_abs_max: 0.0,
                delta_volume_abs_max: 0.0,
            });
        entry.total_volume_usd += volume_usd;
        entry.delta_prob_pp_abs_max = entry.delta_prob_pp_abs_max.max(delta.probability_pp_abs);
        entry.delta_yes_price_abs_max = entry.delta_yes_price_abs_max.max(delta.yes_price_abs);
        entry.delta_volume_abs_max = entry.delta_volume_abs_max.max(delta.volume_abs);
    }

    if event_map.is_empty() {
        let resp = serde_json::json!({
            "query": query_trimmed,
            "source": "live_api_via_csv",
            "note": "no matching events found in CSV cache",
            "sort_by": opts.sort_by.as_str(),
            "deltas_only": opts.deltas_only,
            "min_delta_pp": opts.min_delta_pp,
            "category_filter": opts.category_filter.clone(),
            "match_all": match_all,
            "events_found": 0,
            "events": [],
            "markets": [],
            "total_markets": 0,
            "delta_context": delta_context,
        });
        let json = serde_json::to_string_pretty(&resp).context("serialize")?;
        println!("{json}");
        return Ok(());
    }

    // Sort events by selected ranking mode.
    let mut events_sorted: Vec<(String, LiveEventAggregate)> = event_map.into_iter().collect();
    events_sorted.sort_by(|a, b| {
        let av = &a.1;
        let bv = &b.1;
        let vol_cmp = bv
            .total_volume_usd
            .partial_cmp(&av.total_volume_usd)
            .unwrap_or(std::cmp::Ordering::Equal);
        let delta_prob_cmp = bv
            .delta_prob_pp_abs_max
            .partial_cmp(&av.delta_prob_pp_abs_max)
            .unwrap_or(std::cmp::Ordering::Equal);
        let delta_yes_price_cmp = bv
            .delta_yes_price_abs_max
            .partial_cmp(&av.delta_yes_price_abs_max)
            .unwrap_or(std::cmp::Ordering::Equal);
        let delta_volume_cmp = bv
            .delta_volume_abs_max
            .partial_cmp(&av.delta_volume_abs_max)
            .unwrap_or(std::cmp::Ordering::Equal);
        match opts.sort_by {
            CsvSortBy::Relevance | CsvSortBy::Volume => vol_cmp,
            CsvSortBy::DeltaProb => delta_prob_cmp.then_with(|| vol_cmp),
            CsvSortBy::DeltaYesPrice => delta_yes_price_cmp.then_with(|| vol_cmp),
            CsvSortBy::DeltaVolume => delta_volume_cmp.then_with(|| vol_cmp),
        }
    });
    let final_limit = top.or(limit).unwrap_or(10).min(15);
    events_sorted.truncate(final_limit);

    // Fetch fresh prices for each event
    let mut all_events: Vec<serde_json::Value> = Vec::new();
    let mut all_markets: Vec<serde_json::Value> = Vec::new();

    for (event_ticker, aggregate) in &events_sorted {
        let source = &aggregate.source;
        let category = &aggregate.category;
        let csv_volume = aggregate.total_volume_usd;
        let market_req = eli_core::finance::OddsRequest {
            provider: Some(source.clone()),
            disable_kalshi: source == "polymarket",
            series_ticker: None,
            event_ticker: Some(event_ticker.clone()),
            market_ticker: None,
            status: None,
            limit: None,
            cursor: None,
            max_pages: None,
            include_orderbook: false,
            orderbook_depth: None,
            list_series: false,
            list_events: false,
            list_markets: false,
            list_tags: false,
            category: None,
            search: None,
        };

        match eli_core::finance::fetch_odds(market_req).await {
            Ok(resp) => {
                let event_title = resp
                    .events
                    .first()
                    .map(|e| e.title.clone())
                    .unwrap_or_default();
                let mut event_markets: Vec<serde_json::Value> = Vec::new();
                for m in &resp.markets {
                    let mut mkt_json = serde_json::json!({
                        "source": source,
                        "ticker": m.ticker,
                        "title": m.title,
                        "event_ticker": m.event_ticker,
                        "yes_price": m.yes_price,
                        "yes_bid": m.yes_bid,
                        "yes_ask": m.yes_ask,
                        "volume": m.volume,
                        "volume_usd": m.volume.map(|v| v as f64 / 100.0),
                        "status": m.status,
                        "probability": m.probability_yes,
                    });
                    attach_market_delta(&mut mkt_json, source, &m.ticker, delta_lookup.as_ref());
                    all_markets.push(mkt_json.clone());
                    event_markets.push(mkt_json);
                }
                all_events.push(serde_json::json!({
                    "event_ticker": event_ticker,
                    "title": event_title,
                    "source": source,
                    "category": category,
                    "csv_volume_usd": csv_volume,
                    "delta_abs_probability_pp_max": aggregate.delta_prob_pp_abs_max,
                    "delta_abs_yes_price_cents_max": aggregate.delta_yes_price_abs_max,
                    "delta_abs_volume_cents_max": aggregate.delta_volume_abs_max,
                    "markets": event_markets,
                }));
            }
            Err(e) => {
                all_events.push(serde_json::json!({
                    "event_ticker": event_ticker,
                    "source": source,
                    "category": category,
                    "csv_volume_usd": csv_volume,
                    "delta_abs_probability_pp_max": aggregate.delta_prob_pp_abs_max,
                    "delta_abs_yes_price_cents_max": aggregate.delta_yes_price_abs_max,
                    "delta_abs_volume_cents_max": aggregate.delta_volume_abs_max,
                    "error": format!("{e}"),
                    "markets": [],
                }));
            }
        }
    }

    let resp = serde_json::json!({
        "query": query_trimmed,
        "source": "live_api_via_csv",
        "note": "CSV used for discovery, live API used for fresh prices",
        "events_found": all_events.len(),
        "sort_by": opts.sort_by.as_str(),
        "deltas_only": opts.deltas_only,
        "min_delta_pp": opts.min_delta_pp,
        "category_filter": opts.category_filter.clone(),
        "match_all": match_all,
        "events": all_events,
        "total_markets": all_markets.len(),
        "delta_context": delta_context,
    });

    let json = serde_json::to_string_pretty(&resp).context("serialize live search results")?;
    println!("{json}");
    Ok(())
}

fn cmd_finance_odds_where(args: FinanceOddsWhereArgs) -> Result<()> {
    #[derive(Serialize)]
    struct OddsIdLanguageKalshi {
        /// Kalshi identifiers are human-readable tickers.
        ///
        /// - event_ticker: groups a set of related markets (e.g. `KXFED-26MAR`)
        /// - market_ticker: a specific market within the event (e.g. `KXFED-26MAR-T3.50`)
        event_ticker_example: String,
        market_ticker_example: String,
    }

    #[derive(Serialize)]
    struct OddsIdLanguagePolymarket {
        /// Polymarket markets have numeric IDs.
        ///
        /// - market_id: numeric market id (e.g. `609655`)
        /// - event_id: numeric event id (sometimes also exposed as a slug/ticker in search results)
        market_id_example: String,
        event_id_example: String,
        event_slug_example: String,
    }

    #[derive(Serialize)]
    struct OddsIdLanguage {
        kalshi: OddsIdLanguageKalshi,
        polymarket: OddsIdLanguagePolymarket,
    }

    #[derive(Serialize)]
    struct OddsWhereResponse {
        cache_dir: String,
        kalshi_csv_path: String,
        polymarket_csv_path: String,
        merged_csv_path: String,
        sync_state_path: String,
        sync_delta_index_path: String,
        csv_schema: Vec<&'static str>,
        id_language: OddsIdLanguage,
    }

    let cache_dir = args.cache_dir.unwrap_or_else(|| {
        directories::ProjectDirs::from("", "", "eli")
            .map(|d| d.cache_dir().join("odds"))
            .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"))
    });

    let resp = OddsWhereResponse {
        cache_dir: cache_dir.display().to_string(),
        kalshi_csv_path: cache_dir.join("kalshi_markets.csv").display().to_string(),
        polymarket_csv_path: cache_dir
            .join("polymarket_markets.csv")
            .display()
            .to_string(),
        merged_csv_path: cache_dir.join("all_markets.csv").display().to_string(),
        sync_state_path: cache_dir.join("sync_state.json").display().to_string(),
        sync_delta_index_path: cache_dir.join("sync_last_delta.json").display().to_string(),
        csv_schema: vec![
            "source",
            "ticker",
            "title",
            "event_ticker",
            "yes_price",
            "volume",
            "status",
            "probability",
            "category",
            "topic",
        ],
        id_language: OddsIdLanguage {
            kalshi: OddsIdLanguageKalshi {
                event_ticker_example: "KXFED-26MAR".to_string(),
                market_ticker_example: "KXFED-26MAR-T3.50".to_string(),
            },
            polymarket: OddsIdLanguagePolymarket {
                market_id_example: "609655".to_string(),
                event_id_example: "48802".to_string(),
                event_slug_example: "us-recession-by-end-of-2026".to_string(),
            },
        },
    };

    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
