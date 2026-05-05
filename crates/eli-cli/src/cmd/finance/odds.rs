use chrono::Datelike;

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
    let policy_mode = eli_core::finance::policy::parse_policy_mode(Some(&args.policy_mode))
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --policy-mode")?;
    let resolved_policy =
        eli_core::finance::policy::load_policy(args.policy_file.as_deref(), policy_mode)
            .map_err(|e| anyhow::anyhow!(e))
            .context("load policy")?;

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
            &args.profile,
            args.deltas_only,
            args.min_delta_pp,
            args.category.as_deref(),
            resolved_policy.clone(),
            args.include_mentions,
        )?;

        // Check if local caches exist (SQLite FTS5 preferred, CSV fallback).
        let cache_dir = directories::ProjectDirs::from("", "", "eli")
            .map(|d| d.cache_dir().join("odds"))
            .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"));
        let db_path = cache_dir.join("markets.db");
        let has_db = db_path.exists();

        if args.live {
            // --orderbook: pass depth (1+) to live paths so Polymarket books
            // get attached. None disables. --depth defaults to 5.
            let orderbook_depth = args.orderbook.then(|| args.depth.unwrap_or(5).max(1));
            // --live: if FTS5 DB exists, use discovery+hydration (fast).
            // Otherwise fall back to full catalog download (slow).
            if has_db {
                return cmd_finance_odds_search_live_fts(
                    args.search.as_deref().unwrap_or(""),
                    args.limit,
                    args.top,
                    args.out.as_deref(),
                    &search_opts,
                    provider.as_deref(),
                    orderbook_depth,
                )
                .await;
            }
            return cmd_finance_odds_search_live_no_csv(
                args.search.as_deref().unwrap_or(""),
                args.limit,
                args.top,
                args.out.as_deref(),
                &search_opts,
                provider.as_deref(),
                orderbook_depth,
            )
            .await;
        }

        if has_db {
            // SQLite FTS5 index exists — instant local search.
            return cmd_finance_odds_search_fts(
                args.search.as_deref().unwrap_or(""),
                args.limit,
                args.min_volume,
                args.top,
                args.out.as_deref(),
                &search_opts,
                provider.as_deref(),
            );
        }

        if search_opts.requires_delta_index() {
            anyhow::bail!(
                "delta-aware search requested but local cache is missing. Run `eli finance sync` first."
            );
        }

        // No local index — fall back to live API search with the same response shape.
        // (No --live here, so orderbook never makes sense.)
        eprintln!("hint: run `eli finance sync` for instant FTS5 search");
        return cmd_finance_odds_search_live_no_csv(
            args.search.as_deref().unwrap_or(""),
            args.limit,
            args.top,
            args.out.as_deref(),
            &search_opts,
            provider.as_deref(),
            None,
        )
        .await;
    }

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
    let mut enriched_resp = enrich_odds_response_with_sync_delta(&resp, req.provider.as_deref())?;
    if let Some(obj) = enriched_resp.as_object_mut() {
        obj.insert(
            "applied_policy".to_string(),
            serde_json::json!({
                "mode": resolved_policy.mode,
                "sources": resolved_policy.sources,
            }),
        );
        obj.insert(
            "decision_trace".to_string(),
            serde_json::json!([
                "policy_driven_metadata=true",
                "sync_delta_enrichment=best_effort",
            ]),
        );
    }

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

fn emit_odds_response(
    resp: &eli_core::finance::OddsResponse,
    req: Option<&eli_core::finance::OddsRequest>,
    provider: Option<&str>,
    resolved_policy: &eli_core::finance::policy::ResolvedPolicy,
    out_path: Option<&std::path::Path>,
) -> Result<()> {
    let mut enriched_resp = enrich_odds_response_with_sync_delta(resp, provider)?;
    if let Some(obj) = enriched_resp.as_object_mut() {
        obj.insert(
            "applied_policy".to_string(),
            serde_json::json!({
                "mode": resolved_policy.mode,
                "sources": resolved_policy.sources,
            }),
        );
        obj.insert(
            "decision_trace".to_string(),
            serde_json::json!([
                "policy_driven_metadata=true",
                "sync_delta_enrichment=best_effort",
            ]),
        );
    }

    if let Some(out_path) = out_path {
        let wr = write_json_out_with_meta(
            out_path.to_path_buf(),
            &enriched_resp,
            "finance.odds",
            &[format!("provider={}", provider.unwrap_or_default())],
        )?;
        let prediction_markets_path = prediction_markets_path_for_output(&wr.out_path);
        let fallback_req = eli_core::finance::OddsRequest {
            provider: provider.map(str::to_string),
            disable_kalshi: false,
            series_ticker: None,
            event_ticker: None,
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
        let req = req.unwrap_or(&fallback_req);
        update_prediction_markets(&prediction_markets_path, req, resp, Some(&wr.out_path))
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

fn odds_search_freshness_summary(
    generated_at: chrono::DateTime<chrono::Utc>,
    markets: &[serde_json::Value],
) -> serde_json::Value {
    let mut data_as_of = generated_at;
    let mut max_age_seconds = 0i64;
    let mut stale_count = 0usize;

    for market in markets {
        let Some(freshness) = market.get("freshness") else {
            continue;
        };
        if freshness
            .get("state")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("stale"))
        {
            stale_count = stale_count.saturating_add(1);
        }
        if let Some(age) = freshness.get("age_seconds").and_then(|v| v.as_i64()) {
            max_age_seconds = max_age_seconds.max(age);
        }
        if let Some(observed) = freshness.get("observed_at").and_then(|v| v.as_str()) {
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(observed) {
                let observed_utc = parsed.with_timezone(&chrono::Utc);
                if observed_utc > data_as_of {
                    data_as_of = observed_utc;
                }
            }
        }
    }

    serde_json::json!({
        "data_as_of": data_as_of,
        "max_age_seconds": max_age_seconds,
        "stale_count": stale_count,
    })
}

const STDOUT_INTERESTING_MIN_PROBABILITY: f64 = 0.02;
const STDOUT_INTERESTING_MAX_PROBABILITY: f64 = 0.99;
const STDOUT_DEFAULT_MARKET_LIMIT: usize = 12;
const STDOUT_FALLBACK_MARKET_LIMIT: usize = 6;
const STDOUT_DEFAULT_EVENT_LIMIT: usize = 8;

fn resolve_live_market_limit(top: Option<usize>, limit: Option<usize>) -> usize {
    top.or(limit).unwrap_or(200)
}

fn odds_status_is_open(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "" | "open" | "active"
    )
}

fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact_odds_title(raw: &str) -> String {
    let compact = compact_whitespace(raw);
    let trimmed = compact
        .trim_end_matches('?')
        .trim_end_matches('.')
        .trim()
        .to_string();
    if trimmed.chars().count() <= 96 {
        return trimmed;
    }
    let mut out: String = trimmed.chars().take(93).collect();
    out.push_str("...");
    out
}

fn odds_title_is_low_signal_stdout(row: &serde_json::Value) -> bool {
    let title = row
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    title.contains(" say ")
        || title.contains(" says ")
        || title.contains("mention ")
        || title.contains("mentions ")
        || title.contains("mentioning ")
}

/// Returns true if an event ticker belongs to a mention/speech-prediction market.
/// These are "will X mention Y" markets that pollute search results — e.g.
/// KXMENTION-BERN26MAR10-OIL matches "oil" but is about Bernie Sanders, not oil prices.
/// Filtered by default (like sports at sync time). Opt-in with --include-mentions.
fn is_mention_event_ticker(event_ticker: &str) -> bool {
    let t = event_ticker.to_ascii_uppercase();
    t.contains("MENTION")
        || t.contains("KXSOTU")
        || t.contains("KXTWEET")
        || t.starts_with("KXPRESMENTION")
        || t.starts_with("KXENTMENTION")
}

fn odds_market_probability(row: &serde_json::Value) -> Option<f64> {
    row.get("probability_yes")
        .and_then(json_to_f64)
        .or_else(|| row.get("probability").and_then(json_to_f64))
        .map(|v| if v > 1.0 { v / 100.0 } else { v })
}

fn odds_market_volume_usd(row: &serde_json::Value) -> f64 {
    row.get("volume_usd")
        .and_then(json_to_f64)
        .or_else(|| row.get("volume").and_then(json_to_f64))
        .unwrap_or(0.0)
}

fn odds_market_volume_cents(row: &serde_json::Value) -> Option<i64> {
    row.get("volume")
        .and_then(|v| v.as_i64().or_else(|| json_to_f64(v).map(|n| n.round() as i64)))
        .or_else(|| live_volume_to_cents(odds_market_volume_usd(row)))
}

fn odds_market_sort_key(row: &serde_json::Value) -> (i32, i32, i64, i64) {
    let status = row
        .get("status")
        .and_then(|v| v.as_str())
        .map(odds_status_is_open)
        .unwrap_or(true);
    let probability = odds_market_probability(row);
    let in_band = probability
        .map(|p| {
            (STDOUT_INTERESTING_MIN_PROBABILITY..=STDOUT_INTERESTING_MAX_PROBABILITY).contains(&p)
        })
        .unwrap_or(true);
    let volume_key = (odds_market_volume_usd(row) * 100.0).round() as i64;
    let age_key = row
        .get("freshness")
        .and_then(|v| v.get("age_seconds"))
        .and_then(|v| v.as_i64())
        .unwrap_or(i64::MAX);

    (
        if in_band { 0 } else { 1 },
        if status { 0 } else { 1 },
        -volume_key,
        age_key,
    )
}

fn odds_search_stdout_preserves_ranked_order(resp: &serde_json::Value) -> bool {
    matches!(
        resp.get("source").and_then(|v| v.as_str()),
        Some("fts5" | "fts5_live" | "live_api")
    )
}

fn odds_market_stdout_row(row: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::json!({
        "source": row.get("source").cloned().unwrap_or(serde_json::Value::Null),
        "ticker": row.get("ticker").cloned().unwrap_or(serde_json::Value::Null),
        "event_ticker": row.get("event_ticker").cloned().unwrap_or(serde_json::Value::Null),
        "title": row
            .get("title")
            .and_then(|v| v.as_str())
            .map(compact_odds_title)
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
        "probability_yes": odds_market_probability(row),
        "yes_price": row.get("yes_price").and_then(json_to_f64),
        "yes_bid": row.get("yes_bid").and_then(json_to_f64),
        "yes_ask": row.get("yes_ask").and_then(json_to_f64),
        "volume": odds_market_volume_cents(row),
        "volume_usd": odds_market_volume_usd(row),
        "status": row.get("status").cloned().unwrap_or(serde_json::Value::Null),
    });
    // Attach compact delta (prob_delta_pp + vol_delta) if present
    if let Some(delta) = row.get("delta_since_last_sync") {
        let mut compact = serde_json::Map::new();
        if let Some(pp) = delta
            .get("probability_delta_pct_points")
            .and_then(|v| v.as_f64())
        {
            compact.insert(
                "prob_delta_pp".to_string(),
                serde_json::json!((pp * 10.0).round() / 10.0),
            );
        }
        if let Some(vd) = delta.get("volume_delta").and_then(|v| v.as_i64()) {
            compact.insert("vol_delta".to_string(), serde_json::json!(vd));
        }
        if !compact.is_empty() {
            out["delta"] = serde_json::Value::Object(compact);
        }
    }
    out
}

fn odds_event_stdout_row(row: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "source": row.get("source").cloned().unwrap_or(serde_json::Value::Null),
        "event_ticker": row
            .get("event_ticker")
            .or_else(|| row.get("ticker"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "title": row
            .get("title")
            .and_then(|v| v.as_str())
            .map(compact_odds_title)
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
        "category": row.get("category").cloned().unwrap_or(serde_json::Value::Null),
    })
}

fn compact_odds_search_stdout_payload(resp: &serde_json::Value) -> serde_json::Value {
    let mut out = resp.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };

    // Strip global top-mover lists from delta_context in compact mode.
    // The per-market `delta_since_last_sync` already carries per-result deltas;
    // the global top-movers are identical across every odds call and waste tokens.
    if let Some(dc) = obj.get_mut("delta_context").and_then(|v| v.as_object_mut()) {
        dc.remove("top_probability_moves");
        dc.remove("top_volume_moves");
        dc.remove("top_yes_price_moves");
    }

    let markets = resp
        .get("markets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let events = resp
        .get("events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let interesting_total = markets
        .iter()
        .filter(|row| {
            odds_market_probability(row).is_some_and(|p| {
                (STDOUT_INTERESTING_MIN_PROBABILITY..=STDOUT_INTERESTING_MAX_PROBABILITY)
                    .contains(&p)
            })
        })
        .count();
    let low_signal_total = markets.len().saturating_sub(interesting_total);
    let preserve_ranked_order = odds_search_stdout_preserves_ranked_order(resp);

    let mut ranked_markets: Vec<serde_json::Value> = markets
        .clone()
        .into_iter()
        .filter(|row| !odds_title_is_low_signal_stdout(row))
        .collect();
    if ranked_markets.is_empty() {
        ranked_markets = markets.clone();
    }

    let (stdout_markets, probability_fallback_used): (Vec<serde_json::Value>, bool) =
        if preserve_ranked_order {
            (
                ranked_markets
                    .into_iter()
                    .take(STDOUT_DEFAULT_MARKET_LIMIT)
                    .map(|row| odds_market_stdout_row(&row))
                    .collect(),
                false,
            )
        } else {
            ranked_markets.sort_by_key(odds_market_sort_key);

            let mut in_band: Vec<serde_json::Value> = ranked_markets
                .iter()
                .filter(|row| {
                    odds_market_probability(row).is_some_and(|p| {
                        (STDOUT_INTERESTING_MIN_PROBABILITY..=STDOUT_INTERESTING_MAX_PROBABILITY)
                            .contains(&p)
                    })
                })
                .cloned()
                .collect();
            let mut fallback: Vec<serde_json::Value> = ranked_markets
                .iter()
                .filter(|row| {
                    !odds_market_probability(row).is_some_and(|p| {
                        (STDOUT_INTERESTING_MIN_PROBABILITY..=STDOUT_INTERESTING_MAX_PROBABILITY)
                            .contains(&p)
                    })
                })
                .cloned()
                .collect();

            in_band.truncate(STDOUT_DEFAULT_MARKET_LIMIT);
            if in_band.is_empty() {
                fallback.truncate(STDOUT_FALLBACK_MARKET_LIMIT);
            } else {
                // Keep a few near-certain (>0.99) or near-zero (<0.02) fallback markets even
                // when in-band results exist — they represent hardened signals worth seeing.
                fallback.retain(|row| {
                    odds_market_probability(row).is_some_and(|p| p >= 0.99 || p <= 0.02)
                });
                fallback.truncate(3);
            }

            let stdout_markets: Vec<serde_json::Value> = in_band
                .into_iter()
                .chain(fallback.into_iter())
                .map(|row| odds_market_stdout_row(&row))
                .collect();
            let probability_fallback_used = interesting_total == 0 && !stdout_markets.is_empty();
            (stdout_markets, probability_fallback_used)
        };
    let stdout_events: Vec<serde_json::Value> = events
        .iter()
        .take(STDOUT_DEFAULT_EVENT_LIMIT)
        .map(odds_event_stdout_row)
        .collect();

    obj.insert(
        "stdout_compaction".to_string(),
        serde_json::json!({
            "enabled": true,
            "interesting_probability_band": [
                STDOUT_INTERESTING_MIN_PROBABILITY,
                STDOUT_INTERESTING_MAX_PROBABILITY
            ],
            "interesting_markets_total": interesting_total,
            "low_signal_markets_total": low_signal_total,
            "events_total": events.len(),
            "markets_total": markets.len(),
            "markets_shown": stdout_markets.len(),
            "events_shown": stdout_events.len(),
            "fallback_used": probability_fallback_used,
            "ranking_preserved": preserve_ranked_order,
            "full_results_preserved_with_out": true,
        }),
    );
    obj.insert(
        "markets".to_string(),
        serde_json::Value::Array(stdout_markets),
    );
    if obj.contains_key("events") {
        obj.insert(
            "events".to_string(),
            serde_json::Value::Array(stdout_events),
        );
    }
    out
}

fn emit_odds_search_response(
    resp: &serde_json::Value,
    out_path: Option<&std::path::Path>,
    tool_name: &str,
    meta: &[String],
) -> Result<()> {
    if let Some(out_path) = out_path {
        let wr = write_json_out_with_meta(out_path.to_path_buf(), resp, tool_name, meta)?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let compact = compact_odds_search_stdout_payload(resp);
    let json = serde_json::to_string_pretty(&compact).context("serialize search results")?;
    println!("{json}");
    Ok(())
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

    // Skip the 444MB sync_state.json load when there are no markets to enrich
    // (list_series / list_events / list_tags responses have no per-market price data
    // for delta computation). Saves ~3s on every list-* call.
    let has_markets = value
        .get("markets")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let has_available_markets = value
        .get("available_markets")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_markets && !has_available_markets {
        return Ok(value);
    }

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
            let volume_comparable = volume_scales_look_comparable(previous.volume, current_volume);
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

            // Compact per-item delta: just the ticker + what changed (not the full blob)
            let prob_pp = delta.probability_delta_pct_points;
            let vol_d = delta.volume_delta;
            // Only store compact delta on item if there's a real move
            if prob_pp.is_some() || vol_d.is_some() {
                let mut compact = serde_json::Map::new();
                if let Some(pp) = prob_pp {
                    compact.insert(
                        "prob_delta_pp".to_string(),
                        serde_json::json!((pp * 10.0).round() / 10.0),
                    );
                }
                if let Some(vd) = vol_d {
                    compact.insert("vol_delta".to_string(), serde_json::json!(vd));
                }
                item["delta"] = serde_json::Value::Object(compact);
            }
            attached = attached.saturating_add(1);
        }
    }

    // Top-level delta context: compact summary instead of per-item blobs
    if attached > 0 {
        value["delta_context"] = serde_json::json!({
            "sync_at": lookup.sync_at,
            "markets_with_changes": attached,
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
        matches!(
            self,
            Self::DeltaProb | Self::DeltaYesPrice | Self::DeltaVolume
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchProfile {
    Auto,
    Macro,
    Broad,
}

impl SearchProfile {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "macro" => Ok(Self::Macro),
            "broad" => Ok(Self::Broad),
            other => {
                anyhow::bail!("unsupported --profile '{other}' (supported: auto, macro, broad)")
            }
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Macro => "macro",
            Self::Broad => "broad",
        }
    }

    fn resolve(
        self,
        query: &str,
        category_filter: Option<&str>,
        policy: &eli_core::finance::policy::ResolvedPolicy,
    ) -> Self {
        match self {
            Self::Macro | Self::Broad => self,
            Self::Auto => {
                if category_filter.is_some() {
                    return Self::Broad;
                }
                if query_looks_macro(query, &policy.policy.filtering.macro_keywords) {
                    Self::Macro
                } else {
                    Self::Broad
                }
            }
        }
    }
}

#[derive(Clone)]
struct CsvSearchOptions {
    sort_by: CsvSortBy,
    profile: SearchProfile,
    deltas_only: bool,
    min_delta_pp: Option<f64>,
    category_filter: Option<String>,
    policy: eli_core::finance::policy::ResolvedPolicy,
    include_mentions: bool,
}

impl CsvSearchOptions {
    fn from_cli(
        sort_by_raw: &str,
        profile_raw: &str,
        deltas_only: bool,
        min_delta_pp: Option<f64>,
        category_filter: Option<&str>,
        policy: eli_core::finance::policy::ResolvedPolicy,
        include_mentions: bool,
    ) -> Result<Self> {
        let sort_by = CsvSortBy::parse(sort_by_raw)?;
        let profile = SearchProfile::parse(profile_raw)?;
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
            profile,
            deltas_only,
            min_delta_pp,
            category_filter,
            policy,
            include_mentions,
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

fn query_looks_macro(query: &str, keywords: &[String]) -> bool {
    let q = query.to_ascii_lowercase();
    keywords.iter().any(|k| q.contains(&k.to_ascii_lowercase()))
}

fn macro_relevance_score(
    title: &str,
    category: &str,
    topic: &str,
    country_hints: &[String],
    matched_terms: &[String],
    policy: &eli_core::finance::policy::ResolvedPolicy,
) -> i64 {
    let title_l = title.to_ascii_lowercase();
    let category_l = category.to_ascii_lowercase();
    let topic_l = topic.to_ascii_lowercase();
    let ranking = &policy.policy.ranking;
    let filtering = &policy.policy.filtering;
    let mut score = 0i64;
    if category_l.contains("econom") || topic_l.contains("econom") {
        score += ranking.macro_economics_boost;
    }
    if category_l.contains("financial") || topic_l.contains("financial") {
        score += ranking.macro_financials_boost;
    }
    if category_l.contains("trade") || topic_l.contains("trade") {
        score += 20;
    }
    if country_hints
        .iter()
        .any(|h| h == "us" || h == "fomc" || h == "federal reserve")
    {
        score += 10;
    }
    for kw in &filtering.macro_keywords {
        if title_l.contains(&kw.to_ascii_lowercase()) {
            score += ranking.macro_keyword_weight;
        }
    }
    for kw in &filtering.macro_offtopic_keywords {
        let kw_l = kw.to_ascii_lowercase();
        if category_l.contains(&kw_l) || topic_l.contains(&kw_l) || title_l.contains(&kw_l) {
            score += ranking.macro_offtopic_penalty;
        }
    }
    score += (matched_terms.len() as i64) * 3i64;
    score
}

/// Search the local SQLite FTS5 index (from `eli finance sync`).
/// Returns matching markets as JSON, sorted by FTS5 BM25 relevance + volume.
fn cmd_finance_odds_search_fts(
    query: &str,
    limit: Option<usize>,
    min_volume_usd: Option<f64>,
    top: Option<usize>,
    out_path: Option<&std::path::Path>,
    opts: &CsvSearchOptions,
    provider: Option<&str>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let generated_at = chrono::Utc::now();

    let db_path = eli_core::finance::odds_db::default_db_path();
    let conn = eli_core::finance::odds_db::open_markets_db_readonly(&db_path)
        .ok_or_else(|| anyhow::anyhow!("markets.db not found at {}", db_path.display()))?;

    let min_vol_cents = min_volume_usd.map(|usd| (usd * 100.0) as i64);
    let filters = eli_core::finance::odds_db::SearchFilters {
        category: opts.category_filter.clone(),
        min_volume: min_vol_cents,
        status: Some("open".to_string()),
        source: provider
            .filter(|p| !p.eq_ignore_ascii_case("auto"))
            .map(str::to_string),
        exclude_mentions: !opts.include_mentions,
    };

    let final_limit = top.or(limit).unwrap_or(25);
    let results = eli_core::finance::odds_db::search_markets(&conn, query, final_limit, &filters)
        .map_err(|e| anyhow::anyhow!("FTS search: {e}"))?;

    let duration_ms = started.elapsed().as_millis() as u64;

    let mut markets: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            let yes_price = r.yes_price.unwrap_or(0);
            let volume = r.volume.unwrap_or(0);
            let vol_usd = volume as f64 / 100.0;
            // Derive probability from yes_price (cents) if not stored directly
            let probability = r.probability.unwrap_or_else(|| yes_price as f64 / 100.0);
            serde_json::json!({
                "source": r.source,
                "ticker": r.ticker,
                "title": r.title,
                "event_ticker": r.event_ticker,
                "yes_price": yes_price,
                "volume": volume,
                "volume_usd": vol_usd,
                "status": r.status,
                "probability_yes": probability,
                "category": r.category,
                "match_score": (-r.fts_rank * 100.0) as i64,
                "match_terms": [],
            })
        })
        .collect();
    // Preserve search relevance in the raw response instead of re-sorting by price band.
    sort_live_markets(&mut markets);

    let synced_at = eli_core::finance::odds_db::get_sync_meta(&conn, "last_sync_at")
        .ok()
        .flatten()
        .unwrap_or_default();
    let total = eli_core::finance::odds_db::market_count(&conn).unwrap_or(0);

    let response = serde_json::json!({
        "query": query,
        "source": "fts5",
        "generated_at": generated_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "schema_version": "1.0",
        "freshness_summary": format!("FTS5 index | synced_at={} | {} total markets", synced_at, total),
        "total_matches": results.len(),
        "returned_matches": results.len(),
        "limit": final_limit,
        "top": top,
        "markets": markets,
        "decision_trace": [
            "search_mode=fts5",
            format!("db_path={}", db_path.display()),
            format!("fts_query={}", query),
            format!("duration_ms={}", duration_ms),
        ],
        "run_meta": {
            "duration_ms": duration_ms,
            "db_markets": total,
        },
        "applied_policy": {
            "mode": format!("{:?}", opts.policy.mode),
            "sources": &opts.policy.sources,
        },
    });

    if let Some(path) = out_path {
        let json = serde_json::to_string_pretty(&response)?;
        std::fs::write(path, &json)?;
        let wrapper = serde_json::json!({"ok": true, "path": path.to_string_lossy()});
        println!("{}", serde_json::to_string(&wrapper)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&response)?);
    }

    Ok(())
}

/// Attach Polymarket orderbook depth (per-outcome bids/asks ladders, plus
/// `outcome_best_bids` / `outcome_best_asks` / `orderbook_timestamp` for
/// downstream compatibility) to every Polymarket market in `markets` that
/// carries `clob_token_ids`. One batch REST `/books` call covers them all.
/// Errors are swallowed into `api_errors` so a flaky orderbook fetch never
/// blanks the rest of the search response.
async fn attach_polymarket_orderbooks_to_live_markets(
    markets: &mut [serde_json::Value],
    depth: usize,
    api_errors: &mut Vec<serde_json::Value>,
) {
    let mut token_ids = Vec::new();
    for market in markets.iter() {
        if market.get("source").and_then(|v| v.as_str()) != Some("polymarket") {
            continue;
        }
        let Some(tokens) = market.get("clob_token_ids").and_then(|v| v.as_array()) else {
            continue;
        };
        for token in tokens {
            if let Some(s) = token.as_str() {
                if !s.is_empty() && !token_ids.iter().any(|t: &String| t == s) {
                    token_ids.push(s.to_string());
                }
            }
        }
    }
    if token_ids.is_empty() {
        return;
    }
    let books = match eli_core::finance::fetch_polymarket_orderbooks(&token_ids, depth).await {
        Ok(b) => b,
        Err(e) => {
            api_errors.push(serde_json::json!({
                "phase": "orderbook",
                "source": "polymarket",
                "error": e.to_string(),
            }));
            return;
        }
    };
    for market in markets.iter_mut() {
        if market.get("source").and_then(|v| v.as_str()) != Some("polymarket") {
            continue;
        }
        let Some(tokens) = market
            .get("clob_token_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect::<Vec<_>>())
        else {
            continue;
        };
        if tokens.is_empty() {
            continue;
        }
        let mut best_bids = Vec::with_capacity(tokens.len());
        let mut best_asks = Vec::with_capacity(tokens.len());
        let mut ladders = Vec::with_capacity(tokens.len());
        let mut timestamp: Option<String> = None;
        for token in &tokens {
            let book = books.get(token);
            best_bids.push(
                book.and_then(|b| b.bids.last().map(|l| l.price.clone()))
                    .unwrap_or_default(),
            );
            best_asks.push(
                book.and_then(|a| a.asks.last().map(|l| l.price.clone()))
                    .unwrap_or_default(),
            );
            ladders.push(serde_json::json!({
                "asset_id": token,
                "bids": book.map(|b| b.bids.clone()).unwrap_or_default(),
                "asks": book.map(|b| b.asks.clone()).unwrap_or_default(),
            }));
            if timestamp.is_none() {
                timestamp = book.and_then(|b| b.timestamp.clone());
            }
        }
        if let Some(obj) = market.as_object_mut() {
            obj.insert("outcome_best_bids".to_string(), serde_json::json!(best_bids));
            obj.insert("outcome_best_asks".to_string(), serde_json::json!(best_asks));
            obj.insert(
                "orderbook_timestamp".to_string(),
                timestamp
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
            obj.insert("orderbook".to_string(), serde_json::json!({
                "depth": depth,
                "outcomes": ladders,
            }));
        }
    }
}

/// FTS5 discovery + live hydration: instant local discovery, then targeted API calls.
/// Replaces the full-catalog Kalshi download with FTS5 lookup + per-event hydration.
async fn cmd_finance_odds_search_live_fts(
    query: &str,
    limit: Option<usize>,
    top: Option<usize>,
    out_path: Option<&std::path::Path>,
    opts: &CsvSearchOptions,
    provider: Option<&str>,
    orderbook_depth: Option<usize>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let generated_at = chrono::Utc::now();
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
    let query_terms = live_query_terms(query);
    let query_phrase = query.to_ascii_lowercase();

    // Phase 1: FTS5 Discovery (1ms) — find relevant event tickers from local DB.
    let db_path = eli_core::finance::odds_db::default_db_path();
    let fts_conn = eli_core::finance::odds_db::open_markets_db_readonly(&db_path)
        .ok_or_else(|| anyhow::anyhow!("markets.db not found — run `eli finance sync` first"))?;

    let fts_filters = eli_core::finance::odds_db::SearchFilters {
        category: opts.category_filter.clone(),
        min_volume: None,
        status: Some("open".to_string()),
        source: provider
            .filter(|p| !p.eq_ignore_ascii_case("auto"))
            .map(str::to_string),
        exclude_mentions: !opts.include_mentions,
    };
    let mut fts_query_used = query.to_string();
    let mut fts_results =
        eli_core::finance::odds_db::search_markets(&fts_conn, query, 50, &fts_filters)
            .map_err(|e| anyhow::anyhow!("FTS search: {e}"))?;
    if fts_results.is_empty() {
        for fallback_query in live_search_fallback_queries(query) {
            let fallback_results =
                eli_core::finance::odds_db::search_markets(&fts_conn, &fallback_query, 50, &fts_filters)
                    .map_err(|e| anyhow::anyhow!("FTS search fallback: {e}"))?;
            if !fallback_results.is_empty() {
                fts_query_used = fallback_query;
                fts_results = fallback_results;
                break;
            }
        }
    }
    let fts_ms = started.elapsed().as_millis() as u64;

    // Group FTS results by (source, series_ticker) for hydration.
    // For Kalshi: event_ticker often IS the series_ticker, or parse prefix before the dash.
    let mut kalshi_series: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in &fts_results {
        if r.source == "kalshi" {
            // Extract series ticker: "KXNBERRECESSQ-Q1-2026" → "KXNBERRECESSQ"
            // "KXBTC-26MAR1314-B60875" → "KXBTC"
            let series = r
                .event_ticker
                .split('-')
                .next()
                .unwrap_or(&r.event_ticker)
                .to_string();
            kalshi_series.insert(series);
        }
    }
    let hinted_kalshi_series = live_query_kalshi_series_hints(&query_terms);
    for series in &hinted_kalshi_series {
        kalshi_series.insert(series.clone());
    }

    // Phase 2: Hydrate — parallel Kalshi events + Polymarket search.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build live odds client")?;

    let profile_applied =
        opts.profile
            .resolve(query, opts.category_filter.as_deref(), &opts.policy);

    let hydrate_polymarket = !matches!(provider, Some("kalshi"));
    let hydrate_kalshi = !matches!(provider, Some("polymarket"));

    // Parallel: Polymarket server-side search + Kalshi targeted hydration.
    let kalshi_series_vec: Vec<String> = kalshi_series.into_iter().take(15).collect();
    let (poly_result, kalshi_markets) = tokio::join!(
        async {
            if hydrate_polymarket {
                live_fetch_polymarket_results(
                    &client,
                    query,
                    &query_phrase,
                    &query_terms,
                    opts,
                    profile_applied,
                )
                .await
            } else {
                Ok((Vec::new(), Vec::new(), Vec::new(), Vec::new(), None))
            }
        },
        async {
            let mut all_markets: Vec<serde_json::Value> = Vec::new();
            let mut all_events: Vec<serde_json::Value> = Vec::new();
            let mut errors: Vec<serde_json::Value> = Vec::new();
            if !hydrate_kalshi {
                return (all_events, all_markets, errors);
            }
            for (idx, series_ticker) in kalshi_series_vec.iter().enumerate() {
                if idx > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                match live_fetch_kalshi_series_markets(&client, series_ticker).await {
                    Ok(resp) => {
                        for event in resp.events {
                            if !opts.include_mentions
                                && is_mention_event_ticker(&event.event_ticker)
                            {
                                continue;
                            }
                            let category = event.category.clone().unwrap_or_default();
                            if !category_matches_filter(&category, opts.category_filter.as_deref())
                            {
                                continue;
                            }
                            let event_title = event.title.clone();
                            let event_ticker = event.event_ticker.clone();
                            all_events.push(serde_json::json!({
                                "source": "kalshi",
                                "event_ticker": event_ticker,
                                "title": event_title.clone(),
                                "category": if category.is_empty() {
                                    serde_json::Value::Null
                                } else {
                                    serde_json::json!(category)
                                },
                                "series_ticker": event.series_ticker.or_else(|| Some(series_ticker.clone())),
                            }));
                            for market in event.markets {
                                if !live_is_open_status(market.status.as_deref()) {
                                    continue;
                                }
                                let volume_usd =
                                    live_parse_decimal(market.volume_fp.as_deref()).unwrap_or(0.0);
                                let volume_24h_usd =
                                    live_parse_decimal(market.volume_24h_fp.as_deref())
                                        .unwrap_or(0.0);
                                let open_interest =
                                    live_parse_decimal(market.open_interest_fp.as_deref())
                                        .unwrap_or(0.0);
                                let probability_yes = live_kalshi_market_probability_yes(&market);
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
                                    "",
                                    &query_phrase,
                                    &query_terms,
                                    &[],
                                    volume_usd,
                                    volume_24h_usd,
                                    open_interest,
                                    market.close_time.as_deref(),
                                    market.status.as_deref(),
                                    probability_yes,
                                    50,
                                    profile_applied,
                                    &opts.policy,
                                );
                                let mut market_json = serde_json::json!({
                                    "source": "kalshi",
                                    "ticker": market.ticker,
                                    "title": display_title,
                                    "event_ticker": market.event_ticker,
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
                                    "category": if category.is_empty() {
                                        serde_json::Value::Null
                                    } else {
                                        serde_json::json!(category)
                                    },
                                    "series_ticker": series_ticker,
                                    "match_score": match_score,
                                    "match_terms": matched_terms,
                                    "score_components": {
                                        "fts_discovery": true,
                                        "kalshi_series_hint": hinted_kalshi_series.iter().any(|hint| hint == series_ticker),
                                        "volume_usd": volume_usd,
                                    },
                                });
                                let market_ticker = market.ticker.clone();
                                attach_market_delta(
                                    &mut market_json,
                                    "kalshi",
                                    &market_ticker,
                                    delta_lookup.as_ref(),
                                );
                                all_markets.push(market_json);
                            }
                        }
                    }
                    Err(error) => {
                        errors.push(serde_json::json!({
                            "phase": "fts_hydration",
                            "source": "kalshi",
                            "series_ticker": series_ticker,
                            "error": error,
                        }));
                    }
                }
            }
            (all_events, all_markets, errors)
        }
    );

    let mut api_errors: Vec<serde_json::Value> = Vec::new();
    let mut all_events: Vec<serde_json::Value> = Vec::new();
    let mut live_markets: Vec<serde_json::Value> = Vec::new();

    // Merge Polymarket results.
    let mut polymarket_exact_tag = None;
    match poly_result {
        Ok((events, markets, _expansion_terms, mut poly_errors, exact_tag)) => {
            api_errors.append(&mut poly_errors);
            polymarket_exact_tag = exact_tag;
            all_events.extend(events);
            live_markets.extend(markets);
        }
        Err(error) => {
            api_errors.push(serde_json::json!({
                "phase": "polymarket_search",
                "source": "polymarket",
                "error": error,
            }));
        }
    }

    // Merge Kalshi hydration results.
    let (kalshi_events, kalshi_mkt, kalshi_errors) = kalshi_markets;
    api_errors.extend(kalshi_errors);
    all_events.extend(kalshi_events);
    live_markets.extend(kalshi_mkt);

    // Dedup, sort, diversity select.
    live_markets.retain(|market| !is_dead_market(market));
    sort_live_markets(&mut live_markets);
    let total_events_found = all_events.len();
    let total_markets_found = live_markets.len();
    let final_limit = resolve_live_market_limit(top, limit);
    let ranked_live_markets = live_markets.clone();
    write_back_live_markets_to_db(&ranked_live_markets);
    all_events.truncate(final_limit.max(8));
    live_markets = select_diverse_live_markets(&ranked_live_markets, final_limit);
    // Ensure both sources represented if available.
    for source in ["kalshi", "polymarket"] {
        let source_available = ranked_live_markets.iter().any(|row| {
            row.get("source")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == source)
        });
        let source_selected = live_markets.iter().any(|row| {
            row.get("source")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == source)
        });
        if !source_available || source_selected {
            continue;
        }
        if let Some(candidate) = ranked_live_markets.iter().find(|row| {
            row.get("source")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == source)
        }) {
            if live_markets.len() >= final_limit && !live_markets.is_empty() {
                live_markets.pop();
            }
            live_markets.push(candidate.clone());
        }
    }

    // --orderbook: attach Polymarket book depth to the limited slice the user
    // sees (cheaper than running it across every ranked candidate).
    if let Some(depth) = orderbook_depth {
        attach_polymarket_orderbooks_to_live_markets(&mut live_markets, depth, &mut api_errors)
            .await;
    }

    let mut decision_trace = vec![
        "search_mode=fts5_live".to_string(),
        format!("fts_query={fts_query_used}"),
        format!("fts_discovery_ms={fts_ms}"),
        format!("fts_candidates={}", fts_results.len()),
        format!("kalshi_series_hydrated={}", kalshi_series_vec.len()),
        format!("kalshi_series_hints={}", hinted_kalshi_series.len()),
        format!("polymarket_exact_tag={}", polymarket_exact_tag.clone().unwrap_or_else(|| "-".to_string())),
        format!("returned={}", total_markets_found.min(final_limit)),
        format!("orderbook={}", orderbook_depth.map(|d| d.to_string()).unwrap_or_else(|| "off".to_string())),
    ];
    decision_trace.extend(live_search_provider_trace(
        provider,
        hydrate_kalshi,
        hydrate_polymarket,
    ));

    let resp = serde_json::json!({
        "schema_version": "finance.odds.search_live_fts.v1",
        "query": query,
        "generated_at": generated_at,
        "freshness_summary": odds_search_freshness_summary(generated_at, &live_markets),
        "applied_policy": {
            "mode": opts.policy.mode,
            "sources": opts.policy.sources,
        },
        "run_meta": {
            "latency_ms": started.elapsed().as_millis() as u64,
            "fts_discovery_ms": fts_ms,
            "fts_candidates": fts_results.len(),
            "kalshi_series_hydrated": kalshi_series_vec.len(),
            "db_markets": eli_core::finance::odds_db::market_count(&fts_conn).unwrap_or(0),
        },
        "source": "fts5_live",
        "note": "FTS5 discovery + targeted live hydration",
        "profile_requested": opts.profile.as_str(),
        "profile_applied": profile_applied.as_str(),
        "events_found": total_events_found,
        "events": all_events,
        "markets": live_markets,
        "total_markets": total_markets_found,
        "decision_trace": decision_trace,
        "api_errors": api_errors,
        "delta_context": delta_context,
    });

    emit_odds_search_response(
        &resp,
        out_path,
        "finance.odds.search_live_fts",
        &[format!("query={query}"), "source=fts5_live".to_string()],
    )
}

/// Returns true if the market status indicates it is currently active/open.
fn is_market_active(status: Option<&str>) -> bool {
    match status {
        Some(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "open" | "active" | ""
        ),
        None => true, // null status treated as potentially active
    }
}

/// Returns true if a market is dead data: settled/closed AND has no probability.
/// These are expired markets with no useful information.
fn is_dead_market(market_json: &serde_json::Value) -> bool {
    let status = market_json.get("status").and_then(|v| v.as_str());
    if is_market_active(status) {
        return false;
    }
    // Settled/closed — check if probability is missing or "?"
    let prob_missing = match market_json.get("probability_yes") {
        None => true,
        Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(s)) => s.trim().is_empty() || s.trim() == "?",
        Some(serde_json::Value::Number(_)) => false,
        _ => true,
    };
    let yes_price_missing = match market_json.get("yes_price") {
        None => true,
        Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0) == 0.0,
        _ => false,
    };
    prob_missing && yes_price_missing
}

/// Write live-fetched markets back to the SQLite FTS5 index so future
/// searches discover them instantly.  Fire-and-forget: failures are logged
/// to stderr but never block the search response.
fn write_back_live_markets_to_db(markets_json: &[serde_json::Value]) {
    let db_path = eli_core::finance::odds_db::default_db_path();
    let conn = match eli_core::finance::odds_db::open_markets_db(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[odds write-back] open DB: {e}");
            return;
        }
    };

    let synced_at = chrono::Utc::now().to_rfc3339();
    let mut markets: Vec<eli_core::finance::OddsListedMarket> = Vec::new();
    for m in markets_json {
        let source = match m.get("source").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let ticker = match m.get("ticker").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let title = m
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let event_ticker = m
            .get("event_ticker")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        markets.push(eli_core::finance::OddsListedMarket {
            ticker: ticker.to_string(),
            title,
            event_ticker,
            source: Some(source.to_string()),
            yes_price: m.get("yes_price").and_then(|v| v.as_i64()).or_else(|| {
                m.get("probability_yes")
                    .and_then(|v| v.as_f64())
                    .map(|p| (p * 100.0) as i64)
            }),
            volume: m.get("volume").and_then(|v| v.as_i64()),
            status: m.get("status").and_then(|v| v.as_str()).map(String::from),
            probability_yes: m.get("probability_yes").and_then(|v| v.as_f64()),
            category: m.get("category").and_then(|v| v.as_str()).map(String::from),
            slug: m.get("slug").and_then(|v| v.as_str()).map(String::from),
            clob_token_ids: None,
            freshness: Default::default(),
            market_id: None,
            event_id: None,
            outcomes: None,
            outcome_prices: None,
        });
    }

    // Group by source for batch upsert (incremental — never wipes existing rows).
    let mut by_source: std::collections::HashMap<String, Vec<eli_core::finance::OddsListedMarket>> =
        std::collections::HashMap::new();
    for m in markets {
        let src = m.source.clone().unwrap_or_default();
        by_source.entry(src).or_default().push(m);
    }
    let mut total = 0usize;
    for (source, batch) in &by_source {
        match eli_core::finance::odds_db::upsert_markets(&conn, batch, source, &synced_at, false) {
            Ok(n) => total += n,
            Err(e) => eprintln!("[odds write-back] {source}: {e}"),
        }
    }
    let _ = total;
}

const LIVE_POLY_LIMIT_PER_TYPE: usize = 12;
const LIVE_POLY_EXACT_TAG_EVENT_LIMIT: usize = 8;
const LIVE_KALSHI_SERIES_PAGE_LIMIT: usize = 200;
const LIVE_KALSHI_SERIES_MAX_PAGES: usize = 5;
const LIVE_KALSHI_SERIES_FETCH_BUDGET: usize = 10;
const LIVE_KALSHI_SERIES_EVENT_LIMIT: usize = 25;
const LIVE_KALSHI_SERIES_EVENT_MAX_PAGES: usize = 4;
const LIVE_KALSHI_TAG_FETCH_LIMIT: usize = 3;
const LIVE_KALSHI_TAG_SERIES_MIN: usize = 8;
const LIVE_EXPANSION_LIMIT: usize = 4;
const LIVE_EVENT_FIRST_PASS_CAP: usize = 1;

#[derive(Clone, Default)]
struct LiveMatchHits {
    phrase_match: bool,
    exact_terms: Vec<String>,
    prefix_terms: Vec<String>,
}

#[derive(Clone, serde::Deserialize)]
struct LiveKalshiSeriesResp {
    series: Vec<LiveKalshiSeries>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Clone, serde::Deserialize)]
struct LiveKalshiTagsByCategoryResp {
    #[serde(default)]
    tags_by_categories: std::collections::BTreeMap<String, Option<Vec<String>>>,
}

#[derive(Clone, serde::Deserialize)]
struct LiveKalshiSeries {
    ticker: String,
    title: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    frequency: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    volume_fp: Option<String>,
}

#[derive(Clone, serde::Deserialize)]
struct LiveKalshiEventsResp {
    events: Vec<LiveKalshiEvent>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Clone, serde::Deserialize)]
struct LiveKalshiEvent {
    event_ticker: String,
    title: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    series_ticker: Option<String>,
    #[serde(default)]
    markets: Vec<LiveKalshiMarket>,
}

#[derive(Clone, serde::Deserialize)]
struct LiveKalshiMarket {
    ticker: String,
    title: String,
    event_ticker: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    close_time: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    yes_sub_title: Option<String>,
    #[serde(default)]
    no_sub_title: Option<String>,
    #[serde(default)]
    last_price_dollars: Option<String>,
    #[serde(default)]
    yes_bid_dollars: Option<String>,
    #[serde(default)]
    yes_ask_dollars: Option<String>,
    #[serde(default)]
    volume_24h_fp: Option<String>,
    #[serde(default)]
    open_interest_fp: Option<String>,
    #[serde(default)]
    volume_fp: Option<String>,
}

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

#[derive(Clone, serde::Deserialize)]
struct LivePolymarketMarket {
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
    #[serde(default, rename = "volume24hr")]
    volume_24hr: Option<serde_json::Value>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    closed: Option<bool>,
    #[serde(default, rename = "bestBid")]
    best_bid: Option<f64>,
    #[serde(default, rename = "bestAsk")]
    best_ask: Option<f64>,
    #[serde(default, rename = "lastTradePrice")]
    last_trade_price: Option<f64>,
}

#[derive(Clone)]
struct RankedKalshiSeries {
    ticker: String,
    title: String,
    category: Option<String>,
    frequency: Option<String>,
    volume_fp: Option<String>,
    score: i64,
}

#[derive(Default)]
struct LiveKalshiSeriesDiscovery {
    api_errors: Vec<serde_json::Value>,
    matched_tags: Vec<String>,
    mode: &'static str,
    series: Vec<LiveKalshiSeries>,
}

fn live_parse_decimal(raw: Option<&str>) -> Option<f64> {
    raw.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<f64>().ok()
        }
    })
}

fn live_parse_decimal_value(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn live_volume_to_cents(volume_usd: f64) -> Option<i64> {
    if !volume_usd.is_finite() || volume_usd <= 0.0 {
        return None;
    }
    Some((volume_usd * 100.0).round() as i64)
}

fn live_json_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn live_jsonish_string_array(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Array(arr) => arr.iter().map(live_json_value_to_string).collect(),
        serde_json::Value::String(s) => serde_json::from_str::<Vec<serde_json::Value>>(s)
            .unwrap_or_default()
            .iter()
            .map(live_json_value_to_string)
            .collect(),
        serde_json::Value::Null => Vec::new(),
        other => vec![live_json_value_to_string(other)],
    }
}

fn live_probability_yes_from_outcomes(outcomes: &[String], prices: &[String]) -> Option<f64> {
    for (outcome, price) in outcomes.iter().zip(prices.iter()) {
        if outcome.eq_ignore_ascii_case("yes") {
            return price.parse::<f64>().ok();
        }
    }
    if outcomes.len() == 2 && prices.len() == 2 && outcomes[1].eq_ignore_ascii_case("no") {
        return prices[0].parse::<f64>().ok();
    }
    None
}

fn live_ascii_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
    {
        if !out.contains(&token) {
            out.push(token);
        }
    }
    out
}

fn live_query_terms(query: &str) -> Vec<String> {
    let tokens = live_ascii_tokens(query);
    let mut out: Vec<String> = tokens
        .iter()
        .filter(|t| !live_is_stopword(t))
        .cloned()
        .collect();
    // If ALL tokens are stopwords, keep them — otherwise drop stopwords to reduce noise.
    if out.is_empty() {
        out = tokens;
    }
    append_live_query_alias_terms(&mut out);
    out
}

fn live_query_kalshi_series_hints(query_terms: &[String]) -> Vec<String> {
    let has_term = |needle: &str| query_terms.iter().any(|term| term == needle);
    let has_any = |needles: &[&str]| needles.iter().any(|needle| has_term(needle));
    let has_month = has_any(&[
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ]);
    let mut hints = Vec::new();

    let mut push_hint = |series: &str| {
        if !hints.iter().any(|existing| existing == series) {
            hints.push(series.to_string());
        }
    };

    if has_any(&["recession", "nber"]) {
        for series in ["KXRECSSNBER", "KXNBERRECESSQ", "KXIMFRECESS"] {
            push_hint(series);
        }
    }

    if has_any(&["fed", "federal", "reserve", "fomc"])
        || (has_any(&["rate", "rates", "cut", "hike", "hold", "decision"]) && has_month)
    {
        push_hint("KXFEDDECISION");
        if !has_month {
            for series in ["KXRATECUT", "FEDHIKE"] {
                push_hint(series);
            }
        }
    }

    if has_any(&["tariff", "tariffs", "trade", "import", "imports"]) {
        for series in [
            "KXNEWTARIFFS",
            "KXTARIFFRATEPRC",
            "KXTARIFFRATECAN",
            "KXTARIFFRATEEU",
            "KXTARIFFRATEINDIA",
            "KXTARIFFBILL",
            "KXTARIFFREVENUE",
        ] {
            push_hint(series);
        }
    }

    if has_any(&["oil", "crude", "wti", "brent"]) {
        for series in ["KXWTI", "KXWTIW", "KXBARRELS"] {
            push_hint(series);
        }
    }

    hints
}

fn live_month_number(token: &str) -> Option<u32> {
    match token {
        "january" => Some(1),
        "february" => Some(2),
        "march" => Some(3),
        "april" => Some(4),
        "may" => Some(5),
        "june" => Some(6),
        "july" => Some(7),
        "august" => Some(8),
        "september" => Some(9),
        "october" => Some(10),
        "november" => Some(11),
        "december" => Some(12),
        _ => None,
    }
}

fn live_query_month(query_terms: &[String]) -> Option<u32> {
    query_terms.iter().find_map(|term| live_month_number(term))
}

fn live_text_mentions_month(text: &str, month: u32) -> bool {
    live_ascii_tokens(text)
        .iter()
        .any(|token| live_month_number(token) == Some(month))
}

fn live_close_year_month(close_time: Option<&str>) -> Option<(i32, u32)> {
    let value = close_time?.trim();
    let year = value.get(0..4)?.parse::<i32>().ok()?;
    let month = value.get(5..7)?.parse::<u32>().ok()?;
    Some((year, month))
}

fn live_month_specificity_score(
    title: &str,
    event_title: &str,
    query_terms: &[String],
    close_time: Option<&str>,
) -> i64 {
    let Some(query_month) = live_query_month(query_terms) else {
        return 0;
    };

    let mut score = if live_text_mentions_month(title, query_month)
        || live_text_mentions_month(event_title, query_month)
    {
        45
    } else {
        -35
    };

    if let Some((year, month)) = live_close_year_month(close_time) {
        if month == query_month {
            let now = chrono::Utc::now();
            let months_out =
                ((year - now.year()) * 12 + month as i32 - now.month() as i32).max(0);
            score += match months_out {
                0..=3 => 35,
                4..=6 => 24,
                7..=12 => 12,
                13..=18 => 4,
                _ => 0,
            };
        } else {
            score -= 20;
        }
    }

    score
}

fn live_direction_specificity_score(title: &str, event_title: &str, query_terms: &[String]) -> i64 {
    let text = format!(
        "{} {}",
        title.to_ascii_lowercase(),
        event_title.to_ascii_lowercase()
    );
    let query_has = |needle: &str| query_terms.iter().any(|term| term == needle);
    let mentions_cut = text.contains(" cut ") || text.contains("cut rates") || text.contains("cut by");
    let mentions_hike =
        text.contains(" hike ") || text.contains("hike rates") || text.contains("hike by");
    let mentions_hold = text.contains("maintains rate")
        || text.contains("maintain rate")
        || text.contains("holds rates")
        || text.contains("0bps");

    let mut score = 0;
    if query_has("cut") || query_has("cuts") {
        if mentions_cut {
            score += 70;
        }
        if mentions_hike {
            score -= 45;
        }
        if mentions_hold {
            score -= 55;
        }
    }
    if query_has("hike") || query_has("hikes") {
        if mentions_hike {
            score += 70;
        }
        if mentions_cut {
            score -= 55;
        }
        if mentions_hold {
            score -= 35;
        }
    }
    if query_has("hold")
        || query_has("holds")
        || query_has("pause")
        || query_has("paused")
        || query_has("maintain")
        || query_has("maintains")
    {
        if mentions_hold {
            score += 55;
        }
        if mentions_cut || mentions_hike {
            score -= 35;
        }
    }
    score
}

fn append_live_query_alias_terms(terms: &mut Vec<String>) {
    let has_term = |needle: &str, haystack: &[String]| haystack.iter().any(|term| term == needle);
    if has_term("fomc", terms) {
        for alias in ["fed", "federal", "reserve", "decision", "rate", "rates"] {
            if !has_term(alias, terms) {
                terms.push(alias.to_string());
            }
        }
    }
    if has_term("fed", terms) || (has_term("federal", terms) && has_term("reserve", terms)) {
        for alias in ["fomc", "decision", "rates"] {
            if !has_term(alias, terms) {
                terms.push(alias.to_string());
            }
        }
    }
    if has_term("oil", terms) || has_term("crude", terms) {
        for alias in ["energy", "wti", "brent"] {
            if !has_term(alias, terms) {
                terms.push(alias.to_string());
            }
        }
    }
}

fn live_search_fallback_queries(query: &str) -> Vec<String> {
    let tokens = live_ascii_tokens(query);
    let mut fallbacks = Vec::new();
    if tokens.iter().any(|token| token == "fomc") {
        fallbacks.push("federal reserve".to_string());
        fallbacks.push("fed decision".to_string());
        fallbacks.push("interest rates".to_string());
    }
    if tokens.iter().any(|token| token == "fed") {
        fallbacks.push("federal reserve".to_string());
        fallbacks.push("fomc".to_string());
    }
    if tokens.iter().any(|token| token == "oil") {
        fallbacks.push("crude oil".to_string());
        fallbacks.push("oil and energy".to_string());
    }
    if tokens.iter().any(|token| token == "spy" || token == "spx" || token == "sp500") {
        fallbacks.push("S&P 500".to_string());
        fallbacks.push("stock market".to_string());
    }
    if tokens.iter().any(|token| token == "btc" || token == "bitcoin") {
        fallbacks.push("bitcoin".to_string());
        fallbacks.push("crypto".to_string());
    }
    if tokens.iter().any(|token| token == "gdp") {
        fallbacks.push("economic growth".to_string());
        fallbacks.push("recession".to_string());
    }
    if tokens.iter().any(|token| token == "cpi" || token == "inflation") {
        fallbacks.push("inflation".to_string());
        fallbacks.push("consumer prices".to_string());
    }
    if tokens.iter().any(|token| token == "tariff" || token == "tariffs") {
        fallbacks.push("tariffs".to_string());
        fallbacks.push("trade war".to_string());
    }
    fallbacks.retain(|candidate| candidate != query);
    fallbacks
}

fn live_search_provider_trace(
    provider: Option<&str>,
    kalshi_enabled: bool,
    polymarket_enabled: bool,
) -> Vec<String> {
    vec![
        format!("kalshi_live_search={kalshi_enabled}"),
        format!("polymarket_public_search={polymarket_enabled}"),
        format!("provider_filter={}", provider.unwrap_or("auto")),
    ]
}

fn live_shared_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

fn live_normalize_token(token: &str) -> String {
    let lower = token.trim().to_ascii_lowercase();
    if lower.len() > 4 && lower.ends_with("ies") {
        return format!("{}y", &lower[..lower.len() - 3]);
    }
    if lower.len() > 4 && lower.ends_with("es") {
        return lower[..lower.len() - 2].to_string();
    }
    if lower.len() > 3 && lower.ends_with('s') {
        return lower[..lower.len() - 1].to_string();
    }
    lower
}

fn live_token_match_kind(query_term: &str, candidate: &str) -> Option<bool> {
    let lhs = live_normalize_token(query_term);
    let rhs = live_normalize_token(candidate);
    if lhs.is_empty() || rhs.is_empty() {
        return None;
    }
    if lhs == rhs {
        return Some(true);
    }
    // Short query terms (≤3 chars like "war", "ai", "oil") require exact match only.
    // Prefix matching "war" → "warner"/"warranty" produces too many false positives.
    if lhs.len() <= 3 {
        return None;
    }
    if lhs.starts_with(&rhs) || rhs.starts_with(&lhs) {
        let common = live_shared_prefix_len(&lhs, &rhs);
        if common >= 5 {
            return Some(false);
        }
        if common >= 4 && (lhs.len() <= 4 || rhs.len() <= 4) {
            return Some(false);
        }
    }
    None
}

fn live_collect_match_hits(text: &str, query_phrase: &str, terms: &[String]) -> LiveMatchHits {
    let text_lower = text.to_ascii_lowercase();
    let tokens = live_ascii_tokens(text);
    let mut hits = LiveMatchHits {
        phrase_match: !query_phrase.is_empty() && text_lower.contains(query_phrase),
        ..LiveMatchHits::default()
    };

    for term in terms {
        let mut exact = false;
        let mut prefix = false;
        for token in &tokens {
            match live_token_match_kind(term, token) {
                Some(true) => {
                    exact = true;
                    break;
                }
                Some(false) => {
                    prefix = true;
                }
                None => {}
            }
        }
        if exact {
            if !hits.exact_terms.contains(term) {
                hits.exact_terms.push(term.clone());
            }
            continue;
        }
        if prefix && !hits.prefix_terms.contains(term) {
            hits.prefix_terms.push(term.clone());
        }
    }

    hits
}

fn live_score_match_hits(
    hits: &LiveMatchHits,
    exact_weight: i64,
    prefix_weight: i64,
    phrase_bonus: i64,
) -> i64 {
    let mut score = 0i64;
    if hits.phrase_match {
        score += phrase_bonus;
    }
    score += (hits.exact_terms.len() as i64) * exact_weight;
    score += (hits.prefix_terms.len() as i64) * prefix_weight;
    score
}

fn live_extend_unique(target: &mut Vec<String>, values: &[String]) {
    for value in values {
        if !target.contains(value) {
            target.push(value.clone());
        }
    }
}

fn live_normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn live_scored_hits(
    text: &str,
    query_phrase: &str,
    terms: &[String],
    exact_weight: i64,
    prefix_weight: i64,
    phrase_bonus: i64,
) -> (i64, Vec<String>) {
    let hits = live_collect_match_hits(text, query_phrase, terms);
    let mut matched_terms = hits.exact_terms.clone();
    live_extend_unique(&mut matched_terms, &hits.prefix_terms);
    (
        live_score_match_hits(&hits, exact_weight, prefix_weight, phrase_bonus),
        matched_terms,
    )
}

fn live_is_stopword(token: &str) -> bool {
    matches!(
        token,
        "a" | "about"
            | "above"
            | "after"
            | "against"
            | "all"
            | "an"
            | "and"
            | "at"
            | "before"
            | "below"
            | "between"
            | "by"
            | "daily"
            | "day"
            | "end"
            | "et"
            | "for"
            | "from"
            | "hit"
            | "in"
            | "march"
            | "month"
            | "of"
            | "on"
            | "or"
            | "price"
            | "settle"
            | "settleat"
            | "than"
            | "that"
            | "the"
            | "this"
            | "through"
            | "to"
            | "up"
            | "us"
            | "usa"
            | "what"
            | "week"
            | "will"
            | "year"
    )
}

fn live_extract_expansion_terms(
    query_terms: &[String],
    events: &[LivePolymarketEvent],
) -> Vec<String> {
    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for (idx, event) in events.iter().take(6).enumerate() {
        let rank_weight = (6usize.saturating_sub(idx)) as f64;
        let volume_weight = event
            .volume_24hr
            .as_ref()
            .and_then(live_parse_decimal_value)
            .or_else(|| event.volume.as_ref().and_then(live_parse_decimal_value))
            .unwrap_or(0.0);
        let weight = rank_weight + (volume_weight + 1.0).log10();
        let title = event.title.as_deref().unwrap_or_default();
        let slug = event.slug.as_deref().unwrap_or_default();
        let text = format!("{title} {slug}");
        for token in live_ascii_tokens(&text) {
            if token.len() < 4 || live_is_stopword(&token) || query_terms.contains(&token) {
                continue;
            }
            *scores.entry(token).or_insert(0.0) += weight;
        }
    }

    let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .take(LIVE_EXPANSION_LIMIT)
        .map(|(token, _)| token)
        .collect()
}

fn live_volume_score(volume_usd: f64) -> i64 {
    if !volume_usd.is_finite() || volume_usd <= 0.0 {
        return 0;
    }
    ((volume_usd + 1.0).log10() * 6.0).round() as i64
}

fn live_recent_volume_score(volume_usd: f64) -> i64 {
    if !volume_usd.is_finite() || volume_usd <= 0.0 {
        return 0;
    }
    ((volume_usd + 1.0).log10() * 8.0).round() as i64
}

fn live_open_interest_score(open_interest: f64) -> i64 {
    if !open_interest.is_finite() || open_interest <= 0.0 {
        return 0;
    }
    ((open_interest + 1.0).log10() * 4.0).round() as i64
}

fn live_frequency_boost(frequency: Option<&str>) -> i64 {
    match frequency
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "fifteen_min" => 18,
        "hourly" => 16,
        "daily" => 14,
        "weekly" => 12,
        "monthly" => 8,
        "custom" => 7,
        "one_off" => 6,
        "annual" => 4,
        _ => 0,
    }
}

fn live_is_open_status(status: Option<&str>) -> bool {
    matches!(
        status
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "open" | "active"
    )
}

fn live_market_probability_band_bonus(probability_yes: Option<f64>) -> i64 {
    match probability_yes {
        Some(p) if p.is_finite() => {
            let p = p.clamp(0.0, 1.0);
            let balance = 1.0 - ((p - 0.5).abs() / 0.5);
            let balance_bonus = (balance * 10.0).round() as i64;
            let band_bonus = if (0.01..0.99).contains(&p) { 4 } else { 1 };
            balance_bonus + band_bonus
        }
        None => 0,
        Some(_) => 0,
    }
}

fn live_close_time_bonus(close_time: Option<&str>) -> i64 {
    let Some(value) = close_time.map(str::trim).filter(|value| !value.is_empty()) else {
        return 0;
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) else {
        return 0;
    };
    let close_time_utc = parsed.with_timezone(&chrono::Utc);
    let seconds_until_close = (close_time_utc - chrono::Utc::now()).num_seconds();
    if seconds_until_close < 0 {
        return -10;
    }
    match seconds_until_close {
        0..=21_600 => 22,
        21_601..=86_400 => 18,
        86_401..=604_800 => 12,
        604_801..=2_592_000 => 8,
        2_592_001..=10_368_000 => 4,
        10_368_001..=31_536_000 => 1,
        _ => 0,
    }
}

fn live_kalshi_market_probability_yes(market: &LiveKalshiMarket) -> Option<f64> {
    let yes_bid = live_parse_decimal(market.yes_bid_dollars.as_deref());
    let yes_ask = live_parse_decimal(market.yes_ask_dollars.as_deref());
    if let (Some(bid), Some(ask)) = (yes_bid, yes_ask) {
        if bid.is_finite() && ask.is_finite() && (bid > 0.0 || ask > 0.0) {
            return Some(((bid + ask) / 2.0).clamp(0.0, 1.0));
        }
    }
    live_parse_decimal(market.last_price_dollars.as_deref())
        .or(yes_bid)
        .or(yes_ask)
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 1.0))
}

fn live_kalshi_market_display_title(
    event_title: &str,
    title: &str,
    yes_sub_title: Option<&str>,
    subtitle: Option<&str>,
) -> String {
    let event_title = live_normalize_whitespace(event_title.trim());
    let title = live_normalize_whitespace(title.trim());
    let mut base = title;
    let base_prefix = base.trim_end_matches('?').trim();
    if !event_title.is_empty()
        && !base_prefix.is_empty()
        && event_title
            .to_ascii_lowercase()
            .starts_with(&base_prefix.to_ascii_lowercase())
        && event_title.len() > base.len() + 4
    {
        base = event_title;
    }
    let detail = yes_sub_title.or(subtitle).unwrap_or_default().trim();
    if base.is_empty() {
        return detail.to_string();
    }
    if detail.is_empty() {
        return base.to_string();
    }
    if base
        .to_ascii_lowercase()
        .contains(&detail.to_ascii_lowercase())
    {
        return base.to_string();
    }
    format!("{base} ({detail})")
}

fn live_market_sort_key(row: &serde_json::Value) -> (i32, i64, i32, i64) {
    let direct_match = live_market_has_direct_match(row);
    let score = row
        .get("match_score")
        .and_then(|v| v.as_i64())
        .unwrap_or_default();
    let active = is_market_active(row.get("status").and_then(|v| v.as_str()));
    let volume_key = row
        .get("volume_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or_default()
        .round() as i64;
    (
        if direct_match { 1 } else { 0 },
        score,
        if active { 1 } else { 0 },
        volume_key,
    )
}

fn sort_live_markets(markets: &mut Vec<serde_json::Value>) {
    markets.sort_by(|a, b| live_market_sort_key(b).cmp(&live_market_sort_key(a)));
}

fn live_market_row_key(row: &serde_json::Value) -> String {
    let source = row
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let ticker = row
        .get("ticker")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    format!("{source}::{ticker}")
}

fn live_market_has_direct_match(row: &serde_json::Value) -> bool {
    row.get("match_terms")
        .and_then(|v| v.as_array())
        .is_some_and(|terms| !terms.is_empty())
}

fn live_market_diversity_key(row: &serde_json::Value) -> String {
    let source = row
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if let Some(event_ticker) = row
        .get("event_ticker")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return format!("{source}::event::{event_ticker}");
    }
    if let Some(series_ticker) = row
        .get("series_ticker")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return format!("{source}::series::{series_ticker}");
    }
    let title = row
        .get("title")
        .and_then(|v| v.as_str())
        .map(live_normalize_whitespace)
        .unwrap_or_default()
        .to_ascii_lowercase();
    format!("{source}::title::{title}")
}

fn select_diverse_live_markets(
    ranked_live_markets: &[serde_json::Value],
    final_limit: usize,
) -> Vec<serde_json::Value> {
    let mut selected = Vec::new();
    let mut selected_rows = std::collections::HashSet::new();
    let mut diversity_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for require_direct_match in [true, false] {
        for enforce_diversity in [true, false] {
            for row in ranked_live_markets {
                if selected.len() >= final_limit {
                    break;
                }
                if require_direct_match && !live_market_has_direct_match(row) {
                    continue;
                }
                let row_key = live_market_row_key(row);
                if selected_rows.contains(&row_key) {
                    continue;
                }
                let diversity_key = live_market_diversity_key(row);
                if enforce_diversity
                    && diversity_counts
                        .get(&diversity_key)
                        .copied()
                        .unwrap_or_default()
                        >= LIVE_EVENT_FIRST_PASS_CAP
                {
                    continue;
                }
                selected_rows.insert(row_key);
                *diversity_counts.entry(diversity_key).or_insert(0) += 1;
                selected.push(row.clone());
            }
        }
    }

    selected
}

async fn live_fetch_json<T>(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, String)],
    label: &str,
) -> std::result::Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    let mut attempt = 0usize;
    loop {
        let resp = client
            .get(url)
            .query(query)
            .send()
            .await
            .map_err(|e| format!("{label} failed: {e}"))?;

        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<T>()
                .await
                .map_err(|e| format!("{label} parse failed: {e}"));
        }

        if (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            && attempt < 2
        {
            attempt += 1;
            tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64)).await;
            continue;
        }

        return Err(format!("{label} failed: http {status}"));
    }
}

fn live_discover_kalshi_tags(
    query_phrase: &str,
    query_terms: &[String],
    tags_by_categories: &std::collections::BTreeMap<String, Option<Vec<String>>>,
) -> Vec<String> {
    let mut ranked = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for tags in tags_by_categories.values() {
        for tag in tags.clone().unwrap_or_default() {
            let normalized = live_normalize_whitespace(tag.trim());
            if normalized.is_empty() {
                continue;
            }
            let normalized_key = normalized.to_ascii_lowercase();
            if !seen.insert(normalized_key) {
                continue;
            }
            let tag_tokens = live_ascii_tokens(&normalized);
            let strong_match = normalized.eq_ignore_ascii_case(query_phrase)
                || query_terms
                    .iter()
                    .any(|term| term.eq_ignore_ascii_case(&normalized))
                || (!tag_tokens.is_empty()
                    && tag_tokens
                        .iter()
                        .all(|token| query_terms.iter().any(|term| term == token)));
            if !strong_match {
                continue;
            }
            let (score, matched_terms) =
                live_scored_hits(&normalized, query_phrase, query_terms, 18, 10, 28);
            if matched_terms.is_empty() && score <= 0 {
                continue;
            }
            ranked.push((normalized, score));
        }
    }

    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .take(LIVE_KALSHI_TAG_FETCH_LIMIT)
        .map(|(tag, _)| tag)
        .collect()
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

fn live_series_score(
    series: &LiveKalshiSeries,
    query_phrase: &str,
    query_terms: &[String],
    expansion_terms: &[String],
    profile_applied: SearchProfile,
    policy: &eli_core::finance::policy::ResolvedPolicy,
) -> i64 {
    let title = series.title.as_str();
    let ticker = series.ticker.as_str();
    let category = series.category.as_deref().unwrap_or_default();
    let tags = series.tags.clone().unwrap_or_default().join(" ");
    let mut matched_terms = Vec::new();

    let (title_score, title_terms) = live_scored_hits(title, query_phrase, query_terms, 18, 10, 30);
    let (ticker_score, ticker_terms) =
        live_scored_hits(ticker, query_phrase, query_terms, 10, 6, 12);
    let (category_score, category_terms) =
        live_scored_hits(category, query_phrase, query_terms, 6, 4, 0);
    let (tag_score, tag_terms) = live_scored_hits(&tags, query_phrase, query_terms, 9, 5, 0);

    live_extend_unique(&mut matched_terms, &title_terms);
    live_extend_unique(&mut matched_terms, &ticker_terms);
    live_extend_unique(&mut matched_terms, &category_terms);
    live_extend_unique(&mut matched_terms, &tag_terms);

    let mut score = title_score + ticker_score + category_score + tag_score;
    score += live_scored_hits(title, "", expansion_terms, 8, 4, 0).0;
    score += live_scored_hits(ticker, "", expansion_terms, 5, 3, 0).0;
    score += live_scored_hits(category, "", expansion_terms, 4, 2, 0).0;
    score += live_scored_hits(&tags, "", expansion_terms, 5, 3, 0).0;

    if matched_terms.is_empty() && score <= 0 {
        return 0;
    }

    let volume_usd = live_parse_decimal(series.volume_fp.as_deref()).unwrap_or(0.0);
    score += live_volume_score(volume_usd);
    score += live_frequency_boost(series.frequency.as_deref());

    if category.eq_ignore_ascii_case("mentions") || ticker.contains("MENTION") {
        score -= 120;
    }

    if profile_applied == SearchProfile::Macro {
        score += macro_relevance_score(title, category, "", &[], &matched_terms, policy);
    }

    score
}

fn live_market_score(
    title: &str,
    event_title: &str,
    category: &str,
    tag_text: &str,
    query_phrase: &str,
    query_terms: &[String],
    expansion_terms: &[String],
    volume_usd: f64,
    volume_24h_usd: f64,
    open_interest: f64,
    close_time: Option<&str>,
    status: Option<&str>,
    probability_yes: Option<f64>,
    base_score: i64,
    profile_applied: SearchProfile,
    policy: &eli_core::finance::policy::ResolvedPolicy,
) -> (i64, Vec<String>) {
    let mut matched_terms = Vec::new();
    let (title_score, title_terms) = live_scored_hits(title, query_phrase, query_terms, 20, 12, 40);
    let (event_score, event_terms) =
        live_scored_hits(event_title, query_phrase, query_terms, 10, 6, 18);
    let (category_score, category_terms) =
        live_scored_hits(category, query_phrase, query_terms, 5, 3, 0);
    let (tag_score, tag_terms) = live_scored_hits(tag_text, query_phrase, query_terms, 4, 2, 0);
    live_extend_unique(&mut matched_terms, &title_terms);
    live_extend_unique(&mut matched_terms, &event_terms);
    live_extend_unique(&mut matched_terms, &category_terms);
    live_extend_unique(&mut matched_terms, &tag_terms);

    let mut score = base_score + title_score + event_score + category_score + tag_score;
    score += live_scored_hits(title, "", expansion_terms, 7, 4, 0).0;
    score += live_scored_hits(event_title, "", expansion_terms, 5, 3, 0).0;
    score += live_scored_hits(category, "", expansion_terms, 3, 2, 0).0;
    score += live_scored_hits(tag_text, "", expansion_terms, 2, 1, 0).0;
    score += live_volume_score(volume_usd);
    score += live_recent_volume_score(volume_24h_usd);
    score += live_open_interest_score(open_interest);
    score += live_market_probability_band_bonus(probability_yes);
    score += live_close_time_bonus(close_time);
    score += live_month_specificity_score(title, event_title, query_terms, close_time);
    score += live_direction_specificity_score(title, event_title, query_terms);

    if live_is_open_status(status) {
        score += 25;
    } else {
        score -= 200;
    }

    if profile_applied == SearchProfile::Macro {
        score += macro_relevance_score(title, category, category, &[], &matched_terms, policy);
    }

    (score, matched_terms)
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
    let query_params = vec![
        ("q", query.to_string()),
        ("limit_per_type", LIVE_POLY_LIMIT_PER_TYPE.to_string()),
        ("page", "1".to_string()),
        ("events_status", "active".to_string()),
        ("search_tags", "true".to_string()),
        ("search_profiles", "false".to_string()),
        ("optimized", "false".to_string()),
    ];
    let url = "https://gamma-api.polymarket.com/public-search";
    let body: LivePolymarketSearchResp =
        live_fetch_json(client, url, &query_params, "polymarket public-search").await?;

    let exact_tag_slug = live_pick_exact_polymarket_tag(&body.tags, query_phrase, query_terms);
    let mut supplemental_tag_events = Vec::new();
    let mut api_errors = Vec::new();
    if let Some(tag_slug) = exact_tag_slug.as_deref() {
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
            Ok(events) => supplemental_tag_events = events,
            Err(error) => api_errors.push(serde_json::json!({
                "phase": "event_discovery",
                "source": "polymarket_tag_events",
                "tag_slug": tag_slug,
                "error": error,
            })),
        }
    }

    let mut combined_events: Vec<(LivePolymarketEvent, bool)> = body
        .events
        .into_iter()
        .map(|event| (event, false))
        .collect();
    let mut seen_polymarket_event_keys = std::collections::HashSet::new();
    for (event, _) in &combined_events {
        let event_key = event
            .slug
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| live_json_value_to_string(&event.id));
        seen_polymarket_event_keys.insert(event_key);
    }
    for event in supplemental_tag_events {
        let event_key = event
            .slug
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| live_json_value_to_string(&event.id));
        if seen_polymarket_event_keys.insert(event_key) {
            combined_events.push((event, true));
        }
    }

    let expansion_seed_events: Vec<LivePolymarketEvent> = combined_events
        .iter()
        .map(|(event, _)| event.clone())
        .collect();
    let expansion_terms = live_extract_expansion_terms(query_terms, &expansion_seed_events);
    let mut events = Vec::new();
    let mut markets = Vec::new();
    let mut seen_events = std::collections::HashSet::new();
    let mut seen_markets = std::collections::HashSet::new();

    for (event_rank, (event, exact_tag_grounded)) in combined_events.into_iter().enumerate() {
        let event_ticker = event
            .ticker
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| live_json_value_to_string(&event.id));
        let title = event.title.clone().unwrap_or_else(|| event_ticker.clone());
        let event_title = title.clone();
        let event_ticker_value = event_ticker.clone();
        let category = event.category.clone().unwrap_or_default();
        let tag_text = live_polymarket_event_tags_text(&event);
        let event_volume = event
            .volume_24hr
            .as_ref()
            .and_then(live_parse_decimal_value)
            .or_else(|| event.volume.as_ref().and_then(live_parse_decimal_value))
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
            + (LIVE_POLY_LIMIT_PER_TYPE.saturating_sub(event_rank) as i64) * 3;
        event_score += live_scored_hits(&tag_text, "", &expansion_terms, 2, 1, 0).0;
        if exact_tag_grounded {
            event_score += 18;
        }
        if event_open {
            event_score += 15;
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

        if !seen_events.insert(format!("polymarket::{event_ticker}")) {
            continue;
        }
        events.push(serde_json::json!({
            "source": "polymarket",
            "event_ticker": event_ticker,
            "title": title,
            "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
            "slug": event.slug,
            "match_score": event_score,
        }));

        for (market_rank, market) in event.markets.into_iter().enumerate() {
            let market_title = market.question.clone().unwrap_or_else(|| title.clone());
            let market_slug = market.slug.clone();
            let outcomes = live_jsonish_string_array(&market.outcomes);
            let outcome_prices = live_jsonish_string_array(&market.outcome_prices);
            let probability_yes = live_probability_yes_from_outcomes(&outcomes, &outcome_prices);
            let volume_usd = market
                .volume_24hr
                .as_ref()
                .and_then(live_parse_decimal_value)
                .or_else(|| market.volume.as_ref().and_then(live_parse_decimal_value))
                .unwrap_or(event_volume);
            let status = if market.active.unwrap_or(event_open) && !market.closed.unwrap_or(false) {
                Some("open".to_string())
            } else {
                Some("closed".to_string())
            };
            if !live_is_open_status(status.as_deref()) {
                continue;
            }
            if !category_matches_filter(&category, opts.category_filter.as_deref()) {
                continue;
            }

            let base_score =
                event_score + (LIVE_POLY_LIMIT_PER_TYPE.saturating_sub(market_rank) as i64);
            let (match_score, matched_terms) = live_market_score(
                &market_title,
                &event_title,
                &category,
                &tag_text,
                query_phrase,
                query_terms,
                &expansion_terms,
                volume_usd,
                volume_usd,
                0.0,
                None,
                status.as_deref(),
                probability_yes,
                base_score,
                profile_applied,
                &opts.policy,
            );

            let ticker = live_json_value_to_string(&market.id);
            if !seen_markets.insert(format!("polymarket::{ticker}")) {
                continue;
            }
            let clob_token_ids = live_jsonish_string_array(&market.clob_token_ids);

            let mut market_json = serde_json::json!({
                "source": "polymarket",
                "ticker": ticker,
                "title": market_title,
                "event_ticker": event_ticker_value,
                "yes_price": probability_yes,
                "last_trade_price": market.last_trade_price,
                "yes_bid": market.best_bid,
                "yes_ask": market.best_ask,
                "volume": live_volume_to_cents(volume_usd),
                "volume_usd": volume_usd,
                "status": status,
                "probability_yes": probability_yes,
                "slug": market_slug,
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
            attach_market_delta(&mut market_json, "polymarket", &ticker, None);
            markets.push(market_json);
        }
    }

    if events.is_empty() && markets.is_empty() {
        api_errors.push(serde_json::json!({
            "phase": "event_discovery",
            "source": "polymarket",
            "error": "public-search returned no active events",
        }));
    }

    Ok((events, markets, expansion_terms, api_errors, exact_tag_slug))
}

async fn live_fetch_kalshi_tags_by_categories(
    client: &reqwest::Client,
) -> std::result::Result<LiveKalshiTagsByCategoryResp, String> {
    let url = "https://api.elections.kalshi.com/trade-api/v2/search/tags_by_categories";
    live_fetch_json(client, url, &[], "kalshi tags_by_categories").await
}

async fn live_fetch_kalshi_series_catalog_filtered(
    client: &reqwest::Client,
    tag: Option<&str>,
) -> std::result::Result<Vec<LiveKalshiSeries>, String> {
    let mut cursor: Option<String> = None;
    let mut page = 0usize;
    let mut all_series = Vec::new();

    while page < LIVE_KALSHI_SERIES_MAX_PAGES {
        let mut query = vec![
            ("limit", LIVE_KALSHI_SERIES_PAGE_LIMIT.to_string()),
            ("include_volume", "true".to_string()),
        ];
        if let Some(tag) = tag {
            query.push(("tags", tag.to_string()));
        }
        if let Some(ref c) = cursor {
            if !c.trim().is_empty() {
                query.push(("cursor", c.clone()));
            }
        }
        let url = "https://api.elections.kalshi.com/trade-api/v2/series";
        let body: LiveKalshiSeriesResp =
            live_fetch_json(client, url, &query, "kalshi series list").await?;
        let page_len = body.series.len();
        all_series.extend(body.series);
        cursor = body.cursor.filter(|c| !c.trim().is_empty());
        page += 1;
        if cursor.is_none() || page_len == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Ok(all_series)
}

async fn live_discover_kalshi_series_for_query(
    client: &reqwest::Client,
    query_phrase: &str,
    query_terms: &[String],
) -> LiveKalshiSeriesDiscovery {
    let mut discovery = LiveKalshiSeriesDiscovery {
        mode: "catalog",
        ..LiveKalshiSeriesDiscovery::default()
    };

    match live_fetch_kalshi_tags_by_categories(client).await {
        Ok(resp) => {
            let matched_tags =
                live_discover_kalshi_tags(query_phrase, query_terms, &resp.tags_by_categories);
            if !matched_tags.is_empty() {
                let mut tag_filtered_series = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for tag in &matched_tags {
                    match live_fetch_kalshi_series_catalog_filtered(client, Some(tag)).await {
                        Ok(series) => {
                            for row in series {
                                if seen.insert(row.ticker.clone()) {
                                    tag_filtered_series.push(row);
                                }
                            }
                        }
                        Err(error) => discovery.api_errors.push(serde_json::json!({
                            "phase": "event_discovery",
                            "source": "kalshi_series_tag",
                            "tag": tag,
                            "error": error,
                        })),
                    }
                }
                if tag_filtered_series.len() >= LIVE_KALSHI_TAG_SERIES_MIN {
                    discovery.mode = "tag_filtered";
                    discovery.matched_tags = matched_tags;
                    discovery.series = tag_filtered_series;
                    return discovery;
                }
                discovery.api_errors.push(serde_json::json!({
                    "phase": "event_discovery",
                    "source": "kalshi_series_tag",
                    "tags": matched_tags,
                    "error": format!(
                        "tag-filtered series yielded {} rows; falling back to catalog",
                        tag_filtered_series.len()
                    ),
                }));
            }
        }
        Err(error) => discovery.api_errors.push(serde_json::json!({
            "phase": "event_discovery",
            "source": "kalshi_tags",
            "error": error,
        })),
    }

    match live_fetch_kalshi_series_catalog_filtered(client, None).await {
        Ok(series) => {
            discovery.series = series;
            discovery
        }
        Err(error) => {
            discovery.api_errors.push(serde_json::json!({
                "phase": "event_discovery",
                "source": "kalshi_series",
                "error": error,
            }));
            discovery
        }
    }
}

async fn live_fetch_kalshi_series_markets(
    client: &reqwest::Client,
    series_ticker: &str,
) -> std::result::Result<LiveKalshiEventsResp, String> {
    let url = "https://api.elections.kalshi.com/trade-api/v2/events";
    let mut events = Vec::new();
    let mut cursor: Option<String> = None;
    let mut page = 0usize;

    while page < LIVE_KALSHI_SERIES_EVENT_MAX_PAGES {
        let mut query = vec![
            ("series_ticker", series_ticker.to_string()),
            ("status", "open".to_string()),
            ("with_nested_markets", "true".to_string()),
            ("limit", LIVE_KALSHI_SERIES_EVENT_LIMIT.to_string()),
        ];
        if let Some(ref c) = cursor {
            if !c.trim().is_empty() {
                query.push(("cursor", c.clone()));
            }
        }
        let body: LiveKalshiEventsResp =
            live_fetch_json(client, url, &query, "kalshi open events").await?;
        let page_len = body.events.len();
        events.extend(body.events);
        cursor = body.cursor.filter(|c| !c.trim().is_empty());
        page += 1;
        if cursor.is_none() || page_len == 0 {
            break;
        }
    }

    Ok(LiveKalshiEventsResp {
        events,
        cursor: None,
    })
}

/// No CSV available — fall back to live API: search Kalshi events, then fetch markets
/// for matched events. Also queries Polymarket. Returns combined results.
async fn cmd_finance_odds_search_live_no_csv(
    query: &str,
    limit: Option<usize>,
    top: Option<usize>,
    out_path: Option<&std::path::Path>,
    opts: &CsvSearchOptions,
    provider: Option<&str>,
    orderbook_depth: Option<usize>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let generated_at = chrono::Utc::now();
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
    let profile_applied =
        opts.profile
            .resolve(query, opts.category_filter.as_deref(), &opts.policy);
    let query_phrase = query.to_ascii_lowercase();
    let query_terms = live_query_terms(query);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build live odds client")?;

    let fetch_polymarket = !matches!(provider, Some("kalshi"));
    let fetch_kalshi = !matches!(provider, Some("polymarket"));

    let (poly_result, kalshi_discovery) = tokio::join!(
        async {
            if fetch_polymarket {
                live_fetch_polymarket_results(
                    &client,
                    query,
                    &query_phrase,
                    &query_terms,
                    opts,
                    profile_applied,
                )
                .await
            } else {
                Ok((Vec::new(), Vec::new(), Vec::new(), Vec::new(), None))
            }
        },
        async {
            if fetch_kalshi {
                live_discover_kalshi_series_for_query(&client, &query_phrase, &query_terms).await
            } else {
                LiveKalshiSeriesDiscovery::default()
            }
        },
    );

    let mut api_errors: Vec<serde_json::Value> = Vec::new();
    let mut polymarket_exact_tag = None;
    let (mut all_events, mut live_markets, expansion_terms) = match poly_result {
        Ok((events, markets, expansion_terms, mut poly_errors, exact_tag)) => {
            api_errors.append(&mut poly_errors);
            polymarket_exact_tag = exact_tag;
            (events, markets, expansion_terms)
        }
        Err(error) => {
            api_errors.push(serde_json::json!({
                "phase": "event_discovery",
                "source": "polymarket",
                "error": error,
            }));
            (Vec::new(), Vec::new(), Vec::new())
        }
    };

    api_errors.extend(kalshi_discovery.api_errors);
    let kalshi_discovery_mode = kalshi_discovery.mode;
    let kalshi_matched_tags = kalshi_discovery.matched_tags;

    let mut ranked_series: Vec<RankedKalshiSeries> = kalshi_discovery
        .series
        .into_iter()
        .filter(|series| {
            opts.include_mentions
                || !series
                    .category
                    .as_deref()
                    .unwrap_or_default()
                    .eq_ignore_ascii_case("mentions")
        })
        .filter_map(|series| {
            let score = live_series_score(
                &series,
                &query_phrase,
                &query_terms,
                &expansion_terms,
                profile_applied,
                &opts.policy,
            );
            if score <= 0 {
                return None;
            }
            Some(RankedKalshiSeries {
                ticker: series.ticker,
                title: series.title,
                category: series.category,
                frequency: series.frequency,
                volume_fp: series.volume_fp,
                score,
            })
        })
        .collect();

    ranked_series.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| {
                let b_volume = live_parse_decimal(b.volume_fp.as_deref()).unwrap_or(0.0);
                let a_volume = live_parse_decimal(a.volume_fp.as_deref()).unwrap_or(0.0);
                b_volume
                    .partial_cmp(&a_volume)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.ticker.cmp(&b.ticker))
    });

    let mut seen_events: std::collections::HashSet<String> = all_events
        .iter()
        .filter_map(|row| {
            Some(format!(
                "{}::{}",
                row.get("source")?.as_str()?,
                row.get("event_ticker")?.as_str()?
            ))
        })
        .collect();
    let mut seen_markets: std::collections::HashSet<String> = live_markets
        .iter()
        .filter_map(|row| {
            Some(format!(
                "{}::{}",
                row.get("source")?.as_str()?,
                row.get("ticker")?.as_str()?
            ))
        })
        .collect();

    let mut kalshi_series_with_live_markets = 0usize;
    for (idx, series) in ranked_series
        .iter()
        .take(LIVE_KALSHI_SERIES_FETCH_BUDGET)
        .enumerate()
    {
        if idx > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        let series_resp = match live_fetch_kalshi_series_markets(&client, &series.ticker).await {
            Ok(resp) => resp,
            Err(error) => {
                api_errors.push(serde_json::json!({
                    "phase": "series_drill_down",
                    "source": "kalshi",
                    "series_ticker": series.ticker,
                    "error": error,
                }));
                continue;
            }
        };

        let mut series_had_live_markets = false;
        for event in series_resp.events {
            if !opts.include_mentions && is_mention_event_ticker(&event.event_ticker) {
                continue;
            }
            let category = event
                .category
                .clone()
                .or_else(|| series.category.clone())
                .unwrap_or_default();
            if !category_matches_filter(&category, opts.category_filter.as_deref()) {
                continue;
            }

            let active_markets: Vec<LiveKalshiMarket> = event
                .markets
                .into_iter()
                .filter(|market| live_is_open_status(market.status.as_deref()))
                .collect();
            if active_markets.is_empty() {
                continue;
            }
            series_had_live_markets = true;

            let event_title = event.title.clone();
            let event_ticker = event.event_ticker.clone();
            let event_score = series.score
                + live_scored_hits(&event.title, &query_phrase, &query_terms, 12, 7, 18).0
                + live_scored_hits(&event.title, "", &expansion_terms, 4, 2, 0).0;
            let event_key = format!("kalshi::{event_ticker}");
            if seen_events.insert(event_key) {
                all_events.push(serde_json::json!({
                    "source": "kalshi",
                    "event_ticker": event_ticker,
                    "title": event_title.clone(),
                    "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
                    "series_ticker": event.series_ticker.or_else(|| Some(series.ticker.clone())),
                    "match_score": event_score,
                }));
            }

            for market in active_markets {
                let volume_usd = live_parse_decimal(market.volume_fp.as_deref()).unwrap_or(0.0);
                let volume_24h_usd =
                    live_parse_decimal(market.volume_24h_fp.as_deref()).unwrap_or(0.0);
                let open_interest =
                    live_parse_decimal(market.open_interest_fp.as_deref()).unwrap_or(0.0);
                let probability_yes = live_kalshi_market_probability_yes(&market);
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
                    "",
                    &query_phrase,
                    &query_terms,
                    &expansion_terms,
                    volume_usd,
                    volume_24h_usd,
                    open_interest,
                    market.close_time.as_deref(),
                    market.status.as_deref(),
                    probability_yes,
                    series.score,
                    profile_applied,
                    &opts.policy,
                );
                let market_key = format!("kalshi::{}", market.ticker);
                if !seen_markets.insert(market_key) {
                    continue;
                }
                let mut market_json = serde_json::json!({
                    "source": "kalshi",
                    "ticker": market.ticker,
                    "title": display_title,
                    "event_ticker": market.event_ticker,
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
                    "series_ticker": series.ticker,
                    "match_score": match_score,
                    "match_terms": matched_terms,
                    "score_components": {
                        "series_score": series.score,
                        "series_frequency": series.frequency,
                        "volume_usd": volume_usd,
                        "volume_24h_usd": volume_24h_usd,
                        "open_interest": open_interest,
                    },
                });
                let market_ticker = market_json
                    .get("ticker")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                attach_market_delta(
                    &mut market_json,
                    "kalshi",
                    &market_ticker,
                    delta_lookup.as_ref(),
                );
                live_markets.push(market_json);
            }
        }

        if series_had_live_markets {
            kalshi_series_with_live_markets += 1;
        }
    }

    if profile_applied == SearchProfile::Macro
        && matches!(opts.policy.mode, eli_core::finance::PolicyMode::Enforce)
    {
        all_events.retain(|event| {
            let title = event
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let category = event
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            macro_relevance_score(title, category, category, &[], &query_terms, &opts.policy)
                > opts.policy.policy.filtering.macro_profile_min_relevance
        });
        live_markets.retain(|market| {
            let title = market
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let category = market
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            macro_relevance_score(title, category, category, &[], &query_terms, &opts.policy)
                > opts.policy.policy.filtering.macro_profile_min_relevance
        });
    }

    all_events.sort_by(|a, b| {
        b.get("match_score")
            .and_then(|v| v.as_i64())
            .unwrap_or_default()
            .cmp(
                &a.get("match_score")
                    .and_then(|v| v.as_i64())
                    .unwrap_or_default(),
            )
            .then_with(|| {
                let b_vol = b
                    .get("volume_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or_default();
                let a_vol = a
                    .get("volume_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or_default();
                b_vol
                    .partial_cmp(&a_vol)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    live_markets.retain(|market| !is_dead_market(market));
    sort_live_markets(&mut live_markets);
    let total_events_found = all_events.len();
    let total_markets_found = live_markets.len();
    let final_limit = resolve_live_market_limit(top, limit);
    let ranked_live_markets = live_markets.clone();
    write_back_live_markets_to_db(&ranked_live_markets);
    all_events.truncate(final_limit.max(8));
    live_markets = select_diverse_live_markets(&ranked_live_markets, final_limit);
    for source in ["kalshi", "polymarket"] {
        let source_available = ranked_live_markets.iter().any(|row| {
            row.get("source")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == source)
        });
        let source_selected = live_markets.iter().any(|row| {
            row.get("source")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == source)
        });
        if !source_available || source_selected {
            continue;
        }
        if let Some(candidate) = ranked_live_markets.iter().find(|row| {
            row.get("source")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == source)
        }) {
            if live_markets.len() >= final_limit && !live_markets.is_empty() {
                live_markets.pop();
            }
            live_markets.push(candidate.clone());
        }
    }

    // --orderbook: same as the FTS path — only attach to the slice the user sees.
    if let Some(depth) = orderbook_depth {
        attach_polymarket_orderbooks_to_live_markets(&mut live_markets, depth, &mut api_errors)
            .await;
    }

    let mut decision_trace = vec![
        "policy_driven_live_search=true".to_string(),
        "csv_cache_search=false".to_string(),
        format!(
            "polymarket_exact_tag={}",
            polymarket_exact_tag.clone().unwrap_or_else(|| "-".to_string())
        ),
        format!("kalshi_series_discovery={kalshi_discovery_mode}"),
        "direct_match_priority=true".to_string(),
        "event_diversification=true".to_string(),
        format!("returned={}", total_markets_found.min(final_limit)),
        format!("orderbook={}", orderbook_depth.map(|d| d.to_string()).unwrap_or_else(|| "off".to_string())),
    ];
    decision_trace.extend(live_search_provider_trace(
        provider,
        fetch_kalshi,
        fetch_polymarket,
    ));

    let resp = serde_json::json!({
        "schema_version": "finance.odds.search_live.v3",
        "query": query,
        "generated_at": generated_at,
        "freshness_summary": odds_search_freshness_summary(generated_at, &live_markets),
        "applied_policy": {
            "mode": opts.policy.mode,
            "sources": opts.policy.sources,
        },
        "run_meta": {
            "latency_ms": started.elapsed().as_millis() as u64,
            "stdout_chars": 0,
            "stored_bytes": 0,
            "coverage_counts": {
                "events": total_events_found,
                "returned_markets": live_markets.len(),
                "markets_found": total_markets_found,
                "kalshi_series_ranked": ranked_series.len(),
                "kalshi_series_examined": ranked_series.len().min(LIVE_KALSHI_SERIES_FETCH_BUDGET),
                "kalshi_series_with_live_markets": kalshi_series_with_live_markets,
            },
            "token_efficiency": serde_json::Value::Null,
        },
        "source": "live_api",
        "note": "no local CSV cache; results fetched from live Kalshi + Polymarket APIs using Polymarket ranked search and Kalshi open-event hydration",
        "profile_requested": opts.profile.as_str(),
        "profile_applied": profile_applied.as_str(),
        "events_found": total_events_found,
        "events": all_events,
        "markets": live_markets,
        "total_markets": total_markets_found,
        "decision_trace": decision_trace,
        "api_errors": api_errors,
        "delta_context": delta_context,
        "search_debug": {
            "query_terms": query_terms,
            "expansion_terms": expansion_terms,
            "polymarket_exact_tag": polymarket_exact_tag,
            "kalshi_matched_tags": kalshi_matched_tags,
            "kalshi_series_top": ranked_series
                .iter()
                .take(8)
                .map(|series| serde_json::json!({
                    "ticker": series.ticker,
                    "title": series.title,
                    "category": series.category,
                    "frequency": series.frequency,
                    "volume_fp": series.volume_fp,
                    "score": series.score,
                }))
                .collect::<Vec<_>>(),
        },
    });

    emit_odds_search_response(
        &resp,
        out_path,
        "finance.odds.search_live",
        &[format!("query={query}"), "source=live_api".to_string()],
    )
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
        ok: bool,
        cache_dir: String,
        markets_db: String,
        kalshi_csv_dir: String,
        polymarket_csv_dir: String,
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
        ok: true,
        cache_dir: cache_dir.display().to_string(),
        markets_db: cache_dir.join("markets.db").display().to_string(),
        // Both providers' CSVs live in the same `cache_dir`; per-provider
        // paths are exposed as `*_csv_path` below. The directory keys are
        // for callers that want to glob (e.g. delta archives next to the CSV).
        kalshi_csv_dir: cache_dir.display().to_string(),
        polymarket_csv_dir: cache_dir.display().to_string(),
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

#[cfg(test)]
mod odds_live_tests {
    use super::*;

    fn test_policy() -> eli_core::finance::policy::ResolvedPolicy {
        eli_core::finance::policy::load_policy(None, eli_core::finance::PolicyMode::Observe)
            .expect("load default policy")
    }

    #[test]
    fn live_token_matching_is_boundary_aware() {
        assert_eq!(live_token_match_kind("iran", "miran"), None);
        assert_eq!(live_token_match_kind("iran", "iranian"), Some(false));
        assert_eq!(live_token_match_kind("recession", "recession"), Some(true));
    }

    #[test]
    fn live_series_score_prefers_liquid_current_bitcoin_series() {
        let policy = test_policy();
        let query = "bitcoin";
        let query_terms = live_query_terms(query);
        let query_phrase = query.to_string();

        let liquid_series = LiveKalshiSeries {
            ticker: "KXBTCD".to_string(),
            title: "Bitcoin price Above/below".to_string(),
            category: Some("Crypto".to_string()),
            frequency: Some("hourly".to_string()),
            tags: Some(vec!["BTC".to_string(), "Hourly".to_string()]),
            volume_fp: Some("1349022103.00".to_string()),
        };
        let stale_one_off = LiveKalshiSeries {
            ticker: "KXBTC2025100".to_string(),
            title: "Will Bitcoin reach 100k again this year?".to_string(),
            category: Some("Crypto".to_string()),
            frequency: Some("one_off".to_string()),
            tags: Some(vec!["BTC".to_string()]),
            volume_fp: Some("10016515.00".to_string()),
        };

        let liquid_score = live_series_score(
            &liquid_series,
            &query_phrase,
            &query_terms,
            &[],
            SearchProfile::Broad,
            &policy,
        );
        let stale_score = live_series_score(
            &stale_one_off,
            &query_phrase,
            &query_terms,
            &[],
            SearchProfile::Broad,
            &policy,
        );

        assert!(liquid_score > stale_score);
    }

    #[test]
    fn live_market_score_penalizes_closed_markets() {
        let policy = test_policy();
        let query = "recession";
        let query_terms = live_query_terms(query);
        let query_phrase = query.to_string();

        let (open_score, _) = live_market_score(
            "Will there be a recession in 2026?",
            "Recession this year?",
            "Economics",
            "",
            &query_phrase,
            &query_terms,
            &[],
            600_000.0,
            25_000.0,
            8_000.0,
            Some("2099-03-20T18:00:00Z"),
            Some("active"),
            Some(0.34),
            20,
            SearchProfile::Macro,
            &policy,
        );
        let (closed_score, _) = live_market_score(
            "Will there be a recession in 2026?",
            "Recession this year?",
            "Economics",
            "",
            &query_phrase,
            &query_terms,
            &[],
            600_000.0,
            25_000.0,
            8_000.0,
            Some("2099-03-20T18:00:00Z"),
            Some("closed"),
            Some(0.34),
            20,
            SearchProfile::Macro,
            &policy,
        );

        assert!(open_score > closed_score);
    }

    #[test]
    fn live_kalshi_market_display_title_uses_subtitle_when_needed() {
        let title = live_kalshi_market_display_title(
            "Bitcoin price on Mar 13, 2026 at 5pm EDT?",
            "Bitcoin price on Mar 13, 2026?",
            Some("$68,500 or above"),
            None,
        );
        assert_eq!(
            title,
            "Bitcoin price on Mar 13, 2026 at 5pm EDT? ($68,500 or above)"
        );

        let unchanged = live_kalshi_market_display_title(
            "Inflation in Mar 2026 (CPI YoY)",
            "Will the rate of CPI inflation be above 2.8% for the year ending in March 2026?",
            Some("Above 2.8%"),
            None,
        );
        assert_eq!(
            unchanged,
            "Will the rate of CPI inflation be above 2.8% for the year ending in March 2026?"
        );
    }

    #[test]
    fn live_kalshi_probability_prefers_quotes_when_available() {
        let market = LiveKalshiMarket {
            ticker: "KXBTCD-26MAR1317-T68499.99".to_string(),
            title: "Bitcoin price on Mar 13, 2026?".to_string(),
            event_ticker: "KXBTCD-26MAR1317".to_string(),
            status: Some("active".to_string()),
            close_time: Some("2099-03-13T21:00:00Z".to_string()),
            subtitle: None,
            yes_sub_title: Some("$68,500 or above".to_string()),
            no_sub_title: None,
            last_price_dollars: Some("0.9500".to_string()),
            yes_bid_dollars: Some("0.9400".to_string()),
            yes_ask_dollars: Some("0.9500".to_string()),
            volume_24h_fp: Some("11880.00".to_string()),
            open_interest_fp: Some("16748.00".to_string()),
            volume_fp: Some("89617.00".to_string()),
        };

        let probability = live_kalshi_market_probability_yes(&market).expect("probability");
        assert!((probability - 0.945).abs() < f64::EPSILON);
    }

    #[test]
    fn live_discover_kalshi_tags_matches_query_terms() {
        let tags = std::collections::BTreeMap::from([
            (
                "Economics".to_string(),
                Some(vec![
                    "Inflation".to_string(),
                    "Oil and energy".to_string(),
                    "Jobs & Economy".to_string(),
                ]),
            ),
            (
                "Politics".to_string(),
                Some(vec!["Iran".to_string(), "International".to_string()]),
            ),
        ]);

        assert_eq!(
            live_discover_kalshi_tags("inflation", &live_query_terms("inflation"), &tags),
            vec!["Inflation".to_string()]
        );
        assert_eq!(
            live_discover_kalshi_tags("oil", &live_query_terms("oil"), &tags),
            Vec::<String>::new()
        );
        assert_eq!(
            live_discover_kalshi_tags("oil and energy", &live_query_terms("oil and energy"), &tags),
            vec!["Oil and energy".to_string()]
        );
        assert_eq!(
            live_discover_kalshi_tags("iran", &live_query_terms("iran"), &tags),
            vec!["Iran".to_string()]
        );
    }

    #[test]
    fn live_query_terms_expand_fomc_and_oil_aliases() {
        let fomc_terms = live_query_terms("fomc");
        assert!(fomc_terms.contains(&"fomc".to_string()));
        assert!(fomc_terms.contains(&"fed".to_string()));
        assert!(fomc_terms.contains(&"reserve".to_string()));

        let oil_terms = live_query_terms("oil");
        assert!(oil_terms.contains(&"oil".to_string()));
        assert!(oil_terms.contains(&"wti".to_string()));
        assert!(oil_terms.contains(&"brent".to_string()));
    }

    #[test]
    fn live_search_fallback_queries_cover_fomc_aliases() {
        let fallbacks = live_search_fallback_queries("fomc");
        assert!(fallbacks.contains(&"federal reserve".to_string()));
        assert!(fallbacks.contains(&"fed decision".to_string()));
    }

    #[test]
    fn live_query_kalshi_series_hints_cover_macro_queries() {
        let fed_hints = live_query_kalshi_series_hints(&live_query_terms("fed rate cut june"));
        assert!(fed_hints.contains(&"KXFEDDECISION".to_string()));
        assert!(!fed_hints.contains(&"KXRATECUT".to_string()));

        let recession_hints = live_query_kalshi_series_hints(&live_query_terms("recession 2026"));
        assert!(recession_hints.contains(&"KXRECSSNBER".to_string()));
        assert!(recession_hints.contains(&"KXNBERRECESSQ".to_string()));

        let tariff_hints = live_query_kalshi_series_hints(&live_query_terms("trump tariffs"));
        assert!(tariff_hints.contains(&"KXNEWTARIFFS".to_string()));
    }

    #[test]
    fn compact_odds_search_stdout_payload_preserves_ranked_search_order() {
        let resp = serde_json::json!({
            "source": "fts5_live",
            "markets": [
                {
                    "source": "kalshi",
                    "ticker": "TOP",
                    "title": "Fed rate cut by June 2026 meeting",
                    "probability_yes": 0.019,
                    "volume_usd": 1200.0,
                    "status": "active",
                    "match_score": 420,
                    "match_terms": ["fed", "june"],
                },
                {
                    "source": "kalshi",
                    "ticker": "SECOND",
                    "title": "Fed rate cut by December 2026 meeting",
                    "probability_yes": 0.65,
                    "volume_usd": 50000.0,
                    "status": "active",
                    "match_score": 315,
                    "match_terms": ["fed"],
                }
            ],
            "events": []
        });

        let compact = compact_odds_search_stdout_payload(&resp);
        let markets = compact["markets"].as_array().expect("markets array");
        assert_eq!(markets[0]["ticker"], "TOP");
        assert_eq!(markets[1]["ticker"], "SECOND");
        assert_eq!(compact["stdout_compaction"]["ranking_preserved"], true);
    }

    #[test]
    fn compact_odds_search_stdout_payload_preserves_volume_fields() {
        let resp = serde_json::json!({
            "source": "fts5_live",
            "markets": [
                {
                    "source": "kalshi",
                    "ticker": "TOP",
                    "title": "Recession by end of 2026",
                    "probability_yes": 0.34,
                    "volume": 82065200,
                    "volume_usd": 820652.0,
                    "status": "active",
                    "match_score": 420,
                    "match_terms": ["recession"],
                }
            ],
            "events": []
        });

        let compact = compact_odds_search_stdout_payload(&resp);
        let market = &compact["markets"].as_array().expect("markets array")[0];
        assert_eq!(market["volume"], 82065200);
        assert_eq!(market["volume_usd"], 820652.0);
    }

    #[test]
    fn resolve_live_market_limit_respects_small_requested_limits() {
        assert_eq!(resolve_live_market_limit(Some(3), None), 3);
        assert_eq!(resolve_live_market_limit(None, Some(2)), 2);
        assert_eq!(resolve_live_market_limit(None, None), 200);
        assert_eq!(resolve_live_market_limit(Some(99), None), 99);
    }

    #[test]
    fn live_direction_specificity_prefers_cut_markets_for_cut_queries() {
        let query_terms = live_query_terms("fed rate cut june");
        let cut_score = live_direction_specificity_score(
            "Will the Federal Reserve Cut rates by 25bps at their June 2026 meeting?",
            "June 2026 Fed decision",
            &query_terms,
        );
        let hold_score = live_direction_specificity_score(
            "Will the Federal Reserve Hike rates by 0bps at their June 2026 meeting? (Fed maintains rate)",
            "June 2026 Fed decision",
            &query_terms,
        );
        assert!(cut_score > hold_score);
    }

    #[test]
    fn live_month_specificity_prefers_matching_nearer_month() {
        let query_terms = live_query_terms("fed rate cut june");
        let june_2026 = live_month_specificity_score(
            "Fed rate cut by June 2026 meeting",
            "June 2026 Fed decision",
            &query_terms,
            Some("2026-06-17T17:59:00Z"),
        );
        let april_2027 = live_month_specificity_score(
            "Fed rate decision by April 2027 meeting",
            "April 2027 Fed decision",
            &query_terms,
            Some("2027-04-28T17:59:00Z"),
        );
        assert!(june_2026 > april_2027);
    }

    #[test]
    fn live_pick_exact_polymarket_tag_prefers_exact_live_tag() {
        let tags = vec![
            LivePolymarketSearchTag {
                label: Some("Russia Capture".to_string()),
                slug: Some("russia-capture".to_string()),
            },
            LivePolymarketSearchTag {
                label: Some("russia".to_string()),
                slug: Some("russia".to_string()),
            },
        ];

        assert_eq!(
            live_pick_exact_polymarket_tag(&tags, "russia", &live_query_terms("russia")),
            Some("russia".to_string())
        );
        assert_eq!(
            live_pick_exact_polymarket_tag(&tags, "oil", &live_query_terms("oil")),
            None
        );
    }

    #[test]
    fn select_diverse_live_markets_prefers_distinct_events_first() {
        let ranked = vec![
            serde_json::json!({
                "source": "polymarket",
                "ticker": "m1",
                "event_ticker": "event-a",
                "title": "A 1",
                "match_score": 100,
                "match_terms": ["oil"],
                "status": "open",
                "volume_usd": 1000.0
            }),
            serde_json::json!({
                "source": "polymarket",
                "ticker": "m2",
                "event_ticker": "event-a",
                "title": "A 2",
                "match_score": 99,
                "match_terms": ["oil"],
                "status": "open",
                "volume_usd": 900.0
            }),
            serde_json::json!({
                "source": "kalshi",
                "ticker": "m3",
                "event_ticker": "event-b",
                "title": "B 1",
                "match_score": 98,
                "match_terms": ["oil"],
                "status": "active",
                "volume_usd": 800.0
            }),
            serde_json::json!({
                "source": "kalshi",
                "ticker": "m4",
                "event_ticker": "event-c",
                "title": "C 1",
                "match_score": 97,
                "match_terms": ["oil"],
                "status": "active",
                "volume_usd": 700.0
            }),
        ];

        let selected = select_diverse_live_markets(&ranked, 3);
        let tickers: Vec<String> = selected
            .iter()
            .filter_map(|row| {
                row.get("ticker")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();

        assert_eq!(tickers, vec!["m1", "m3", "m4"]);
    }

    #[test]
    fn select_diverse_live_markets_prefers_direct_matches_before_liquidity_fallback() {
        let ranked = vec![
            serde_json::json!({
                "source": "kalshi",
                "ticker": "liquid-fallback",
                "event_ticker": "event-a",
                "title": "High liquidity but unrelated",
                "match_score": 190,
                "match_terms": [],
                "status": "active",
                "volume_usd": 1_000_000.0
            }),
            serde_json::json!({
                "source": "polymarket",
                "ticker": "relevant-1",
                "event_ticker": "event-b",
                "title": "Recession by end of 2026?",
                "match_score": 150,
                "match_terms": ["recession"],
                "status": "open",
                "volume_usd": 10_000.0
            }),
            serde_json::json!({
                "source": "kalshi",
                "ticker": "relevant-2",
                "event_ticker": "event-c",
                "title": "Will there be a recession in 2026?",
                "match_score": 149,
                "match_terms": ["recession"],
                "status": "active",
                "volume_usd": 9_000.0
            }),
        ];

        let selected = select_diverse_live_markets(&ranked, 2);
        let tickers: Vec<String> = selected
            .iter()
            .filter_map(|row| {
                row.get("ticker")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();

        assert_eq!(tickers, vec!["relevant-1", "relevant-2"]);
    }
}
