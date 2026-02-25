use super::super::providers::{sync_kalshi_events, sync_polymarket_events};
use super::super::{
    default_odds_field_semantics, OddsListedEvent, OddsListedMarket, OddsSyncBaselineQuality,
    OddsSyncCoverage, OddsSyncRequest, OddsSyncResponse, OddsSyncSourceDelta, OddsSyncSourceResult,
    OddsSyncStatus, RateLimiter,
};
use super::analysis::{build_sync_analysis, build_sync_source_analytics, SyncAnalysisInput};
use super::csv_cache_writer::{merge_markets_csv, write_markets_csv};
use super::delta::{
    apply_source_delta_baseline_reset, build_delta_index, build_overall_delta, build_source_delta,
    load_sync_state, write_delta_index, write_sync_state, OddsSyncMarketState, OddsSyncSourceState,
    SourceBaselineQuality,
};
use crate::Result;
use base64::Engine;
use chrono::{Duration as ChronoDuration, Utc};
use futures::{SinkExt, StreamExt};
use rsa::{pkcs1::DecodeRsaPrivateKey, pkcs8::DecodePrivateKey, Pss, RsaPrivateKey};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use tokio::time::Duration as TokioDuration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const DEFAULT_STREAM_REFRESH_HEARTBEAT_HOURS: i64 = 6;
const DEFAULT_STREAM_REFRESH_TIMEOUT_SECS: u64 = 300;
const STREAM_REFRESH_IDLE_TIMEOUT_MS: u64 = 1200;
const STREAM_REFRESH_MAX_MESSAGES_PER_SOCKET: usize = 300;
const POLYMARKET_WS_SUBSCRIPTION_CHUNK: usize = 800;
const POLYMARKET_WS_STREAM_CONCURRENCY: usize = 8;

/// Sync prediction markets from Kalshi and Polymarket in parallel.
/// Writes per-source CSVs and an optional merged CSV for local discovery/search.
pub async fn sync_odds(req: OddsSyncRequest) -> Result<OddsSyncResponse> {
    let cache_dir = req.cache_dir.map(PathBuf::from).unwrap_or_else(|| {
        directories::ProjectDirs::from("", "", "eli")
            .map(|d| d.cache_dir().join("odds"))
            .unwrap_or_else(|| std::env::temp_dir().join("eli-odds-cache"))
    });
    let sync_state_path = cache_dir.join("sync_state.json");
    let sync_delta_index_path = cache_dir.join("sync_last_delta.json");
    let mut sync_state = load_sync_state(&sync_state_path);
    let previous_sync_at = sync_state.last_sync_at;
    let current_sync_at = Utc::now();

    let max_pages = req.max_pages.filter(|p| *p > 0);

    let sources = req
        .sources
        .unwrap_or_else(|| vec!["kalshi".to_string(), "polymarket".to_string()]);
    let do_kalshi = sources.iter().any(|s| s.eq_ignore_ascii_case("kalshi"));
    let do_polymarket = sources.iter().any(|s| s.eq_ignore_ascii_case("polymarket"));
    let include_sports = req.include_sports;
    let include_historical = req.include_historical;
    let heartbeat_hours = req
        .refresh_heartbeat_hours
        .map(|h| (h as i64).max(1))
        .unwrap_or(DEFAULT_STREAM_REFRESH_HEARTBEAT_HOURS);
    let heartbeat_window = ChronoDuration::hours(heartbeat_hours);
    let stream_refresh_timeout_secs = req
        .stream_refresh_timeout_secs
        .unwrap_or(DEFAULT_STREAM_REFRESH_TIMEOUT_SECS)
        .clamp(1, 3600);
    let stream_refresh_timeout_ms = stream_refresh_timeout_secs.saturating_mul(1000);
    // Stream refresh is opt-in; default path remains strict REST anchor sync.
    let stream_refresh_requested = req.stream_refresh;

    let kalshi_stream_seed =
        if stream_refresh_requested && do_kalshi && max_pages.is_none() && !include_historical {
            let key = source_profile_state_key("kalshi", include_sports);
            match sync_state.sources.get(&key).cloned() {
                Some(seed)
                    if seed.baseline_quality.is_trusted()
                        && !seed.markets.is_empty()
                        && source_seed_is_fresh(&seed, current_sync_at, heartbeat_window) =>
                {
                    Some(seed)
                }
                Some(seed) => {
                    log_stream_seed_skip("kalshi", &seed, current_sync_at, heartbeat_window);
                    None
                }
                None => None,
            }
        } else {
            None
        };

    let polymarket_stream_seed = if stream_refresh_requested
        && do_polymarket
        && max_pages.is_none()
        && !include_historical
    {
        let key = source_profile_state_key("polymarket", include_sports);
        match sync_state.sources.get(&key).cloned() {
            Some(seed)
                if seed.baseline_quality.is_trusted()
                    && !seed.markets.is_empty()
                    && source_seed_is_fresh(&seed, current_sync_at, heartbeat_window) =>
            {
                Some(seed)
            }
            Some(seed) => {
                log_stream_seed_skip("polymarket", &seed, current_sync_at, heartbeat_window);
                None
            }
            None => None,
        }
    } else {
        None
    };

    // Default cadence is intentionally light, then adaptive backoff increases on 429/5xx.
    let kalshi_limiter = RateLimiter::new(12, 4000);
    let poly_limiter = RateLimiter::new(15, 4000);

    let mut source_results = Vec::new();
    let mut csv_paths = Vec::new();
    let mut analysis_inputs: Vec<SyncAnalysisInput> = Vec::new();
    let mut source_deltas: Vec<OddsSyncSourceDelta> = Vec::new();
    let mut market_deltas = Vec::new();

    struct SourceSyncPayload {
        result: OddsSyncSourceResult,
        events: Vec<OddsListedEvent>,
        markets: Vec<OddsListedMarket>,
    }

    let (kalshi_result, poly_result) = tokio::join!(
        async {
            if do_kalshi {
                let start = std::time::Instant::now();
                if let Some(seed) = kalshi_stream_seed.as_ref() {
                    if seed.baseline_quality.is_trusted() && !seed.markets.is_empty() {
                        match try_kalshi_stream_refresh(seed, stream_refresh_timeout_ms).await {
                            Ok((events, markets, coverage, updates)) => {
                                eprintln!(
                                    "[kalshi] stream refresh applied updates to {} markets",
                                    updates
                                );
                                let duration_ms = start.elapsed().as_millis() as u64;
                                let csv_path =
                                    write_markets_csv(&markets, "kalshi", &cache_dir).ok();
                                let analytics = Some(build_sync_source_analytics(&markets));
                                return Some(SourceSyncPayload {
                                    result: OddsSyncSourceResult {
                                        source: "kalshi".to_string(),
                                        ok: true,
                                        events_count: events.len(),
                                        markets_count: markets.len(),
                                        event_count: Some(events.len()),
                                        market_count: Some(markets.len()),
                                        duration_ms,
                                        error: None,
                                        csv_path: csv_path.map(|p| p.to_string_lossy().to_string()),
                                        analytics,
                                        coverage: Some(coverage),
                                        delta: None,
                                    },
                                    events,
                                    markets,
                                });
                            }
                            Err(e) => {
                                eprintln!("[kalshi] stream refresh failed, falling back to REST sync: {e}");
                            }
                        }
                    }
                }
                match sync_kalshi_events(
                    &kalshi_limiter,
                    max_pages,
                    include_sports,
                    include_historical,
                )
                .await
                {
                    Ok((events, markets, coverage)) => {
                        let duration_ms = start.elapsed().as_millis() as u64;
                        let csv_path = write_markets_csv(&markets, "kalshi", &cache_dir).ok();
                        let analytics = Some(build_sync_source_analytics(&markets));
                        Some(SourceSyncPayload {
                            result: OddsSyncSourceResult {
                                source: "kalshi".to_string(),
                                ok: true,
                                events_count: events.len(),
                                markets_count: markets.len(),
                                event_count: Some(events.len()),
                                market_count: Some(markets.len()),
                                duration_ms,
                                error: None,
                                csv_path: csv_path.map(|p| p.to_string_lossy().to_string()),
                                analytics,
                                coverage: Some(coverage),
                                delta: None,
                            },
                            events,
                            markets,
                        })
                    }
                    Err(e) => Some(SourceSyncPayload {
                        result: OddsSyncSourceResult {
                            source: "kalshi".to_string(),
                            ok: false,
                            events_count: 0,
                            markets_count: 0,
                            event_count: Some(0),
                            market_count: Some(0),
                            duration_ms: start.elapsed().as_millis() as u64,
                            error: Some(e),
                            csv_path: None,
                            analytics: None,
                            coverage: None,
                            delta: None,
                        },
                        events: Vec::new(),
                        markets: Vec::new(),
                    }),
                }
            } else {
                None
            }
        },
        async {
            if do_polymarket {
                let start = std::time::Instant::now();
                if let Some(seed) = polymarket_stream_seed.as_ref() {
                    if seed.baseline_quality.is_trusted() && !seed.markets.is_empty() {
                        match try_polymarket_stream_refresh(seed, stream_refresh_timeout_ms).await {
                            Ok((events, markets, coverage, updates)) => {
                                eprintln!(
                                    "[polymarket] stream refresh applied updates to {} markets",
                                    updates
                                );
                                let duration_ms = start.elapsed().as_millis() as u64;
                                let csv_path =
                                    write_markets_csv(&markets, "polymarket", &cache_dir).ok();
                                let analytics = Some(build_sync_source_analytics(&markets));
                                return Some(SourceSyncPayload {
                                    result: OddsSyncSourceResult {
                                        source: "polymarket".to_string(),
                                        ok: true,
                                        events_count: events.len(),
                                        markets_count: markets.len(),
                                        event_count: Some(events.len()),
                                        market_count: Some(markets.len()),
                                        duration_ms,
                                        error: None,
                                        csv_path: csv_path.map(|p| p.to_string_lossy().to_string()),
                                        analytics,
                                        coverage: Some(coverage),
                                        delta: None,
                                    },
                                    events,
                                    markets,
                                });
                            }
                            Err(e) => {
                                eprintln!(
                                    "[polymarket] stream refresh failed, falling back to REST sync: {e}"
                                );
                            }
                        }
                    }
                }
                match sync_polymarket_events(&poly_limiter, max_pages, include_sports).await {
                    Ok((events, markets, coverage)) => {
                        let duration_ms = start.elapsed().as_millis() as u64;
                        let csv_path = write_markets_csv(&markets, "polymarket", &cache_dir).ok();
                        let analytics = Some(build_sync_source_analytics(&markets));
                        Some(SourceSyncPayload {
                            result: OddsSyncSourceResult {
                                source: "polymarket".to_string(),
                                ok: true,
                                events_count: events.len(),
                                markets_count: markets.len(),
                                event_count: Some(events.len()),
                                market_count: Some(markets.len()),
                                duration_ms,
                                error: None,
                                csv_path: csv_path.map(|p| p.to_string_lossy().to_string()),
                                analytics,
                                coverage: Some(coverage),
                                delta: None,
                            },
                            events,
                            markets,
                        })
                    }
                    Err(e) => Some(SourceSyncPayload {
                        result: OddsSyncSourceResult {
                            source: "polymarket".to_string(),
                            ok: false,
                            events_count: 0,
                            markets_count: 0,
                            event_count: Some(0),
                            market_count: Some(0),
                            duration_ms: start.elapsed().as_millis() as u64,
                            error: Some(e),
                            csv_path: None,
                            analytics: None,
                            coverage: None,
                            delta: None,
                        },
                        events: Vec::new(),
                        markets: Vec::new(),
                    }),
                }
            } else {
                None
            }
        }
    );

    for mut payload in [kalshi_result, poly_result].into_iter().flatten() {
        if let Some(ref p) = payload.result.csv_path {
            csv_paths.push(PathBuf::from(p));
        }
        if payload.result.ok {
            let source = payload.result.source.clone();
            let source_state_key = source_profile_state_key(&source, include_sports);
            let previous_state = sync_state.sources.get(&source_state_key);
            let previous_markets = previous_state.map(|s| s.markets.len()).unwrap_or(0);
            let previous_is_trusted = previous_state
                .map(|s| s.baseline_quality.is_trusted() && !s.markets.is_empty())
                .unwrap_or(false);
            let trusted_previous = if previous_is_trusted {
                previous_state
            } else {
                None
            };

            let (current_quality, current_quality_reason) =
                classify_source_quality(payload.result.coverage.as_ref());
            let mut reset_reason = None;
            if !previous_is_trusted {
                let previous_reason = previous_state
                    .and_then(|s| s.baseline_quality_reason.clone())
                    .unwrap_or_else(|| "no trusted baseline available".to_string());
                reset_reason = Some(format!("previous baseline not trusted: {previous_reason}"));
            }
            if !current_quality.is_trusted() {
                let reason = current_quality_reason
                    .clone()
                    .unwrap_or_else(|| "current sync coverage not strict-pass".to_string());
                reset_reason = Some(reason);
            }

            let compare_baseline = if reset_reason.is_some() {
                None
            } else {
                trusted_previous
            };
            let mut delta_build =
                build_source_delta(&source, &payload.markets, compare_baseline, current_sync_at);
            if let Some(reason) = reset_reason {
                apply_source_delta_baseline_reset(
                    &mut delta_build.source_delta,
                    &mut delta_build.market_deltas,
                    previous_markets,
                    Some(reason),
                );
            } else {
                delta_build.source_delta.baseline_quality = current_quality.to_public_quality();
                delta_build.source_delta.baseline_reset_applied = false;
                delta_build.source_delta.baseline_reset_reason = None;
            }
            log_source_delta_preview(&delta_build.source_delta);
            payload.result.delta = Some(delta_build.source_delta.clone());
            source_deltas.push(delta_build.source_delta);
            market_deltas.extend(delta_build.market_deltas.into_iter());
            let preserve_existing_trusted = previous_state
                .map(|s| s.baseline_quality.is_trusted())
                .unwrap_or(false);
            if current_quality.is_trusted() || !preserve_existing_trusted {
                let mut next_state = delta_build.next_state;
                next_state.baseline_quality = current_quality;
                next_state.baseline_quality_reason = current_quality_reason.clone();
                sync_state
                    .sources
                    .insert(source_state_key.clone(), next_state);
                // Remove legacy source-only key once profile-keyed state exists.
                sync_state.sources.remove(&source);
            }
            analysis_inputs.push(SyncAnalysisInput {
                source,
                events: payload.events,
                markets: payload.markets,
            });
        }
        source_results.push(payload.result);
    }

    if req.strict {
        let mut strict_failures = Vec::new();
        for src in &source_results {
            if !src.ok {
                strict_failures.push(format!(
                    "{}: source sync failed ({})",
                    src.source,
                    src.error
                        .clone()
                        .unwrap_or_else(|| "unknown error".to_string())
                ));
                continue;
            }
            if let Some(cov) = &src.coverage {
                if !cov.strict_pass {
                    if cov.strict_fail_reasons.is_empty() {
                        strict_failures.push(format!(
                            "{}: strict coverage check failed without reason",
                            src.source
                        ));
                    } else {
                        for reason in &cov.strict_fail_reasons {
                            strict_failures.push(format!("{}: {}", src.source, reason));
                        }
                    }
                }
            } else {
                strict_failures.push(format!(
                    "{}: strict coverage check missing diagnostics",
                    src.source
                ));
            }
        }
        if !strict_failures.is_empty() {
            return Err(crate::error::Error::Other(format!(
                "strict sync failed: {}",
                strict_failures.join("; ")
            )));
        }
    }

    let merged_path: Option<String> = if csv_paths.len() > 1 {
        merge_markets_csv(&csv_paths, &cache_dir)
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    } else if csv_paths.len() == 1 {
        Some(csv_paths[0].to_string_lossy().to_string())
    } else {
        None
    };

    // Auto-emit .meta.json sidecar for the merged CSV (schema for agent discovery).
    if let Some(ref csv_path_str) = merged_path {
        let csv_path = std::path::Path::new(csv_path_str);
        if csv_path.exists() {
            if let Ok(meta) = crate::meta::build_csv_meta(
                csv_path,
                crate::meta::ProbeOptions {
                    sample_rows: 200,
                    sample_bytes: 500_000,
                    max_depth: 4,
                },
            ) {
                let _ = crate::meta::write_sidecar(&meta, csv_path);
            }
        }
    }

    let total_events: usize = source_results.iter().map(|r| r.events_count).sum();
    let total_markets: usize = source_results.iter().map(|r| r.markets_count).sum();
    let sync_status = if source_results.is_empty() {
        OddsSyncStatus::Partial
    } else if source_results.iter().all(|src| {
        src.ok
            && src
                .coverage
                .as_ref()
                .map(|cov| cov.strict_pass)
                .unwrap_or(false)
    }) {
        OddsSyncStatus::Complete
    } else {
        OddsSyncStatus::Partial
    };
    let analysis = if analysis_inputs.is_empty() {
        None
    } else {
        Some(build_sync_analysis(&analysis_inputs))
    };
    let delta = if source_deltas.is_empty() {
        None
    } else {
        Some(build_overall_delta(
            previous_sync_at,
            current_sync_at,
            &source_deltas,
            &market_deltas,
        ))
    };
    if let Some(ref summary) = delta {
        eprintln!(
            "[sync-delta] overall: previous_markets={} current_markets={} changed={} updated={} churn={} unchanged={} new={} removed={}",
            summary.previous_markets,
            summary.current_markets,
            summary.changed_markets,
            summary.updated_markets,
            summary.churn_markets,
            summary.unchanged_markets,
            summary.new_markets,
            summary.removed_markets
        );
    }

    let mut persisted_sync_state_path = None;
    let mut persisted_delta_index_path = None;
    if !source_deltas.is_empty() {
        sync_state.last_sync_at = Some(current_sync_at);
        match write_sync_state(&sync_state_path, &sync_state) {
            Ok(()) => {
                persisted_sync_state_path = Some(sync_state_path.to_string_lossy().to_string());
            }
            Err(e) => {
                eprintln!("[sync-delta] failed to persist sync state: {e}");
            }
        }

        if let Some(ref summary) = delta {
            let delta_index = build_delta_index(summary, &source_deltas, &market_deltas);
            match write_delta_index(&sync_delta_index_path, &delta_index) {
                Ok(()) => {
                    persisted_delta_index_path =
                        Some(sync_delta_index_path.to_string_lossy().to_string());
                }
                Err(e) => {
                    eprintln!("[sync-delta] failed to persist sync delta index: {e}");
                }
            }
        }
    }

    Ok(OddsSyncResponse {
        generated_at: current_sync_at,
        sync_status,
        sources: source_results,
        total_events,
        total_markets,
        merged_csv_path: merged_path,
        analysis,
        delta,
        sync_state_path: persisted_sync_state_path,
        sync_delta_index_path: persisted_delta_index_path,
        field_semantics: default_odds_field_semantics(),
    })
}

fn classify_source_quality(
    coverage: Option<&OddsSyncCoverage>,
) -> (SourceBaselineQuality, Option<String>) {
    match coverage {
        Some(cov) if cov.strict_pass => (SourceBaselineQuality::Trusted, None),
        Some(cov) if !cov.strict_fail_reasons.is_empty() => (
            SourceBaselineQuality::Untrusted,
            Some(cov.strict_fail_reasons.join("; ")),
        ),
        Some(_) => (
            SourceBaselineQuality::Untrusted,
            Some("coverage marked non-strict-pass".to_string()),
        ),
        None => (
            SourceBaselineQuality::Untrusted,
            Some("coverage diagnostics missing".to_string()),
        ),
    }
}

trait SourceBaselineQualityExt {
    fn to_public_quality(&self) -> OddsSyncBaselineQuality;
}

impl SourceBaselineQualityExt for SourceBaselineQuality {
    fn to_public_quality(&self) -> OddsSyncBaselineQuality {
        match self {
            SourceBaselineQuality::Trusted => OddsSyncBaselineQuality::Trusted,
            SourceBaselineQuality::Untrusted => OddsSyncBaselineQuality::Untrusted,
            SourceBaselineQuality::Unknown => OddsSyncBaselineQuality::Reset,
        }
    }
}

fn log_source_delta_preview(delta: &OddsSyncSourceDelta) {
    eprintln!(
        "[sync-delta] {}: previous={} current={} changed={} updated={} churn={} unchanged={} new={} removed={}",
        delta.source,
        delta.previous_markets,
        delta.current_markets,
        delta.changed_markets,
        delta.updated_markets,
        delta.churn_markets,
        delta.unchanged_markets,
        delta.new_markets,
        delta.removed_markets
    );
    if !delta.top_probability_moves.is_empty() {
        let preview = delta
            .top_probability_moves
            .iter()
            .take(3)
            .map(|m| {
                format!(
                    "{} {:+.2}pp",
                    m.ticker,
                    m.probability_delta_pct_points.unwrap_or(0.0)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[sync-delta] {} top_probability_moves: {preview}",
            delta.source
        );
    }
    if !delta.top_volume_moves.is_empty() {
        let preview = delta
            .top_volume_moves
            .iter()
            .take(3)
            .map(|m| format!("{} {:+}", m.ticker, m.volume_delta.unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[sync-delta] {} top_volume_moves_cents: {preview}",
            delta.source
        );
    }
}

fn source_seed_is_fresh(
    seed: &OddsSyncSourceState,
    now: chrono::DateTime<Utc>,
    heartbeat_window: ChronoDuration,
) -> bool {
    let age = now.signed_duration_since(seed.synced_at);
    age <= heartbeat_window
}

fn log_stream_seed_skip(
    source: &str,
    seed: &OddsSyncSourceState,
    now: chrono::DateTime<Utc>,
    heartbeat_window: ChronoDuration,
) {
    let age = now.signed_duration_since(seed.synced_at);
    let reason = if !seed.baseline_quality.is_trusted() {
        "baseline not trusted".to_string()
    } else if seed.markets.is_empty() {
        "baseline empty".to_string()
    } else if age > heartbeat_window {
        format!(
            "baseline stale (age={}m > heartbeat={}m)",
            age.num_minutes(),
            heartbeat_window.num_minutes()
        )
    } else {
        "unknown baseline eligibility failure".to_string()
    };
    eprintln!("[{source}] stream refresh skipped: {reason}");
}

async fn try_kalshi_stream_refresh(
    seed: &OddsSyncSourceState,
    timeout_ms: u64,
) -> std::result::Result<
    (
        Vec<OddsListedEvent>,
        Vec<OddsListedMarket>,
        OddsSyncCoverage,
        usize,
    ),
    String,
> {
    let mut markets: Vec<OddsListedMarket> = seed
        .markets
        .values()
        .map(|state| sync_state_market_to_listed_market("kalshi", state))
        .collect();
    markets.sort_by(|a, b| a.ticker.cmp(&b.ticker));

    let updates = fetch_kalshi_ticker_updates_ws(timeout_ms).await?;
    let mut updated_markets = 0usize;
    for market in &mut markets {
        if let Some(probability_yes) = updates.get(&market.ticker).copied() {
            market.probability_yes = Some(probability_yes);
            market.yes_price = Some((probability_yes * 100.0).round() as i64);
            updated_markets = updated_markets.saturating_add(1);
        }
    }

    let mut events_by_ticker: BTreeMap<String, OddsListedEvent> = BTreeMap::new();
    for market in &markets {
        if market.event_ticker.trim().is_empty() {
            continue;
        }
        events_by_ticker
            .entry(market.event_ticker.clone())
            .or_insert_with(|| OddsListedEvent {
                ticker: market.event_ticker.clone(),
                title: market.event_ticker.clone(),
                category: market.category.clone(),
                series_ticker: None,
                source: Some("kalshi".to_string()),
                event_id: None,
                slug: None,
                tags: None,
            });
    }
    let events: Vec<OddsListedEvent> = events_by_ticker.into_values().collect();

    let coverage = OddsSyncCoverage {
        requested_max_pages: None,
        events_pages_fetched: 0,
        events_exhausted: true,
        markets_pages_fetched: 0,
        markets_exhausted: true,
        events_requests: 0,
        markets_requests: 1,
        retry_count_429: 0,
        retry_count_5xx: 0,
        series_backfill_calls: None,
        series_backfill_cap: None,
        series_backfill_truncated: None,
        strict_pass: true,
        strict_fail_reasons: Vec::new(),
    };

    Ok((events, markets, coverage, updated_markets))
}

async fn try_polymarket_stream_refresh(
    seed: &OddsSyncSourceState,
    timeout_ms: u64,
) -> std::result::Result<
    (
        Vec<OddsListedEvent>,
        Vec<OddsListedMarket>,
        OddsSyncCoverage,
        usize,
    ),
    String,
> {
    let mut markets: Vec<OddsListedMarket> = seed
        .markets
        .values()
        .map(|state| sync_state_market_to_listed_market("polymarket", state))
        .collect();
    markets.sort_by(|a, b| a.ticker.cmp(&b.ticker));

    let mut dedupe = HashSet::new();
    let token_ids: Vec<String> = markets
        .iter()
        .filter_map(|market| {
            market
                .clob_token_ids
                .as_ref()
                .and_then(|ids| ids.first())
                .map(|id| id.as_str())
        })
        .filter_map(|id| {
            let trimmed = id.trim();
            if trimmed.is_empty() || !dedupe.insert(trimmed.to_string()) {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect();

    if token_ids.is_empty() {
        return Err("polymarket stream refresh baseline missing token ids".to_string());
    }

    let ws_refresh = fetch_polymarket_token_updates_ws(&token_ids, timeout_ms).await?;
    let mut updated_markets = 0usize;
    for market in &mut markets {
        let Some(tokens) = market.clob_token_ids.as_ref() else {
            continue;
        };
        if tokens.is_empty() {
            continue;
        }

        let mut touched = false;
        let yes_token = tokens.first().map(|id| id.as_str()).unwrap_or_default();
        if let Some(probability_yes) = ws_refresh.prices_by_asset.get(yes_token).copied() {
            market.probability_yes = Some(probability_yes);
            market.yes_price = Some((probability_yes * 100.0).round() as i64);
            touched = true;
        }
        if tokens.iter().any(|token| ws_refresh.resolved_assets.contains(token)) {
            market.status = Some("resolved".to_string());
            touched = true;
        }
        if touched {
            updated_markets = updated_markets.saturating_add(1);
        }
    }

    let mut events_by_ticker: BTreeMap<String, OddsListedEvent> = BTreeMap::new();
    for market in &markets {
        if market.event_ticker.trim().is_empty() {
            continue;
        }
        events_by_ticker
            .entry(market.event_ticker.clone())
            .or_insert_with(|| OddsListedEvent {
                ticker: market.event_ticker.clone(),
                title: market.event_ticker.clone(),
                category: market.category.clone(),
                series_ticker: None,
                source: Some("polymarket".to_string()),
                event_id: Some(market.event_ticker.clone()),
                slug: None,
                tags: None,
            });
    }
    let events: Vec<OddsListedEvent> = events_by_ticker.into_values().collect();

    let coverage = OddsSyncCoverage {
        requested_max_pages: None,
        events_pages_fetched: 0,
        events_exhausted: true,
        markets_pages_fetched: 0,
        markets_exhausted: true,
        events_requests: 0,
        markets_requests: ws_refresh.connections.max(1),
        retry_count_429: 0,
        retry_count_5xx: 0,
        series_backfill_calls: None,
        series_backfill_cap: None,
        series_backfill_truncated: None,
        strict_pass: true,
        strict_fail_reasons: Vec::new(),
    };

    Ok((events, markets, coverage, updated_markets))
}

struct PolymarketWsRefreshResult {
    prices_by_asset: HashMap<String, f64>,
    resolved_assets: HashSet<String>,
    connections: usize,
}

async fn fetch_polymarket_token_updates_ws(
    token_ids: &[String],
    timeout_ms: u64,
) -> std::result::Result<PolymarketWsRefreshResult, String> {
    if token_ids.is_empty() {
        return Ok(PolymarketWsRefreshResult {
            prices_by_asset: HashMap::new(),
            resolved_assets: HashSet::new(),
            connections: 0,
        });
    }

    let chunks: Vec<Vec<String>> = token_ids
        .chunks(POLYMARKET_WS_SUBSCRIPTION_CHUNK.max(1))
        .map(|chunk| chunk.to_vec())
        .collect();
    let connections = chunks.len();

    let chunk_results = futures::stream::iter(chunks.into_iter().map(|chunk| async move {
        fetch_polymarket_token_updates_ws_chunk(chunk, timeout_ms).await
    }))
    .buffer_unordered(POLYMARKET_WS_STREAM_CONCURRENCY.max(1))
    .collect::<Vec<_>>()
    .await;

    let mut prices_by_asset: HashMap<String, f64> = HashMap::new();
    let mut resolved_assets: HashSet<String> = HashSet::new();
    for result in chunk_results {
        let chunk = result?;
        for (asset_id, probability) in chunk.prices_by_asset {
            prices_by_asset.insert(asset_id, probability);
        }
        for asset_id in chunk.resolved_assets {
            resolved_assets.insert(asset_id);
        }
    }

    Ok(PolymarketWsRefreshResult {
        prices_by_asset,
        resolved_assets,
        connections,
    })
}

struct PolymarketWsChunkRefresh {
    prices_by_asset: HashMap<String, f64>,
    resolved_assets: HashSet<String>,
}

async fn fetch_polymarket_token_updates_ws_chunk(
    chunk: Vec<String>,
    timeout_ms: u64,
) -> std::result::Result<PolymarketWsChunkRefresh, String> {
    let (mut ws, _) = connect_async("wss://ws-subscriptions-clob.polymarket.com/ws/market")
        .await
        .map_err(|e| format!("polymarket ws connect failed: {e}"))?;

    let subscribe = serde_json::json!({
        "assets_ids": chunk,
        "type": "market",
        "custom_feature_enabled": true
    });
    ws.send(Message::Text(subscribe.to_string()))
        .await
        .map_err(|e| format!("polymarket ws subscribe failed: {e}"))?;

    let mut prices_by_asset: HashMap<String, f64> = HashMap::new();
    let mut resolved_assets: HashSet<String> = HashSet::new();
    let deadline = tokio::time::Instant::now() + TokioDuration::from_millis(timeout_ms.max(1));
    let idle_timeout = TokioDuration::from_millis(STREAM_REFRESH_IDLE_TIMEOUT_MS.max(1));
    let mut message_count = 0usize;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait_for = remaining.min(idle_timeout);
        let next = tokio::time::timeout(wait_for, ws.next()).await;
        let msg = match next {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        };

        let Message::Text(text) = msg else {
            continue;
        };
        message_count = message_count.saturating_add(1);
        if message_count >= STREAM_REFRESH_MAX_MESSAGES_PER_SOCKET {
            break;
        }

        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };

        if let Some(items) = parsed.as_array() {
            for item in items {
                apply_polymarket_market_ws_message(
                    item,
                    &mut prices_by_asset,
                    &mut resolved_assets,
                );
            }
        } else {
            apply_polymarket_market_ws_message(&parsed, &mut prices_by_asset, &mut resolved_assets);
        }
    }

    let _ = ws.close(None).await;
    Ok(PolymarketWsChunkRefresh {
        prices_by_asset,
        resolved_assets,
    })
}

fn apply_polymarket_market_ws_message(
    msg: &serde_json::Value,
    prices_by_asset: &mut HashMap<String, f64>,
    resolved_assets: &mut HashSet<String>,
) {
    let Some(event_type) = msg
        .get("event_type")
        .or_else(|| msg.get("type"))
        .and_then(|v| v.as_str())
    else {
        return;
    };

    match event_type {
        "best_bid_ask" => {
            let Some(asset_id) = msg
                .get("asset_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return;
            };
            let bid = polymarket_json_to_f64(msg.get("best_bid"));
            let ask = polymarket_json_to_f64(msg.get("best_ask"));
            if let Some(prob) = normalized_midpoint_probability(bid, ask) {
                prices_by_asset.insert(asset_id.to_string(), prob);
            }
        }
        "last_trade_price" => {
            let Some(asset_id) = msg
                .get("asset_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return;
            };
            if let Some(prob) = polymarket_json_to_f64(msg.get("price")).map(normalize_probability)
            {
                prices_by_asset.insert(asset_id.to_string(), prob);
            }
        }
        "book" => {
            let Some(asset_id) = msg
                .get("asset_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return;
            };
            let best_bid = msg
                .get("bids")
                .and_then(|v| v.as_array())
                .and_then(|levels| {
                    levels
                        .iter()
                        .filter_map(|level| polymarket_json_to_f64(level.get("price")))
                        .max_by(|a, b| a.total_cmp(b))
                });
            let best_ask = msg
                .get("asks")
                .and_then(|v| v.as_array())
                .and_then(|levels| {
                    levels
                        .iter()
                        .filter_map(|level| polymarket_json_to_f64(level.get("price")))
                        .min_by(|a, b| a.total_cmp(b))
                });
            if let Some(prob) = normalized_midpoint_probability(best_bid, best_ask) {
                prices_by_asset.insert(asset_id.to_string(), prob);
            }
        }
        "price_change" => {
            let Some(changes) = msg.get("price_changes").and_then(|v| v.as_array()) else {
                return;
            };
            for change in changes {
                let Some(asset_id) = change
                    .get("asset_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let best_bid = polymarket_json_to_f64(change.get("best_bid"));
                let best_ask = polymarket_json_to_f64(change.get("best_ask"));
                let probability =
                    normalized_midpoint_probability(best_bid, best_ask).or_else(|| {
                        polymarket_json_to_f64(change.get("price")).map(normalize_probability)
                    });
                if let Some(prob) = probability {
                    prices_by_asset.insert(asset_id.to_string(), prob);
                }
            }
        }
        "market_resolved" => {
            let winning_asset = msg
                .get("winning_asset_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            if let Some(winner) = winning_asset.as_ref() {
                resolved_assets.insert(winner.clone());
                prices_by_asset.insert(winner.clone(), 1.0);
            }

            if let Some(asset_ids) = msg.get("assets_ids").and_then(|v| v.as_array()) {
                for asset in asset_ids
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    resolved_assets.insert(asset.to_string());
                    if let Some(winner) = winning_asset.as_ref() {
                        if asset != winner {
                            prices_by_asset.insert(asset.to_string(), 0.0);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn polymarket_json_to_f64(value: Option<&serde_json::Value>) -> Option<f64> {
    match value {
        Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

fn normalize_probability(value: f64) -> f64 {
    if value > 1.0 {
        (value / 100.0).clamp(0.0, 1.0)
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn normalized_midpoint_probability(bid: Option<f64>, ask: Option<f64>) -> Option<f64> {
    match (
        bid.map(normalize_probability),
        ask.map(normalize_probability),
    ) {
        (Some(b), Some(a)) => Some(((b + a) / 2.0).clamp(0.0, 1.0)),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

async fn fetch_kalshi_ticker_updates_ws(
    timeout_ms: u64,
) -> std::result::Result<HashMap<String, f64>, String> {
    let (mut ws, _) = connect_kalshi_ws_with_optional_auth().await?;

    let subscribe = serde_json::json!({
        "id": 1,
        "cmd": "subscribe",
        "params": { "channels": ["ticker"] }
    });
    ws.send(Message::Text(subscribe.to_string()))
        .await
        .map_err(|e| format!("kalshi ws subscribe failed: {e}"))?;

    let mut updates: HashMap<String, f64> = HashMap::new();
    let deadline = tokio::time::Instant::now() + TokioDuration::from_millis(timeout_ms.max(1));
    let idle_timeout = TokioDuration::from_millis(STREAM_REFRESH_IDLE_TIMEOUT_MS.max(1));
    let mut message_count = 0usize;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait_for = remaining.min(idle_timeout);
        let next = tokio::time::timeout(wait_for, ws.next()).await;
        let msg = match next {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        };

        let Message::Text(text) = msg else {
            continue;
        };
        message_count = message_count.saturating_add(1);
        if message_count >= STREAM_REFRESH_MAX_MESSAGES_PER_SOCKET {
            break;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if parsed
            .get("type")
            .and_then(|v| v.as_str())
            .map(|v| v != "ticker")
            .unwrap_or(true)
        {
            continue;
        }
        let Some(msg_obj) = parsed.get("msg").and_then(|v| v.as_object()) else {
            continue;
        };
        let Some(ticker) = msg_obj
            .get("market_ticker")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if let Some(probability_yes) = kalshi_ticker_probability(msg_obj) {
            updates.insert(ticker.to_string(), probability_yes);
        }
    }

    let _ = ws.close(None).await;
    Ok(updates)
}

async fn connect_kalshi_ws_with_optional_auth() -> std::result::Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    String,
> {
    let ws_urls = [
        "wss://api.elections.kalshi.com/trade-api/ws/v2",
        "wss://api.elections.kalshi.com",
    ];

    let mut last_err: Option<String> = None;
    for ws_url in ws_urls {
        match connect_async(ws_url).await {
            Ok(ok) => return Ok(ok),
            Err(err) => {
                last_err = Some(format!("{ws_url}: {err}"));
            }
        }
    }

    for ws_url in ws_urls {
        let sign_path = kalshi_ws_signature_path(ws_url);
        let request = match build_kalshi_ws_auth_request(ws_url, &sign_path) {
            Ok(req) => req,
            Err(err) => {
                last_err = Some(format!("{ws_url}: {err}"));
                continue;
            }
        };
        match connect_async(request).await {
            Ok(ok) => return Ok(ok),
            Err(err) => {
                last_err = Some(format!("{ws_url}: {err}"));
            }
        }
    }

    Err(format!(
        "kalshi ws connect failed (unauth + auth attempts): {}",
        last_err.unwrap_or_else(|| "unknown error".to_string())
    ))
}

fn build_kalshi_ws_auth_request(
    ws_url: &str,
    path_to_sign: &str,
) -> std::result::Result<tokio_tungstenite::tungstenite::http::Request<()>, String> {
    let creds = crate::finance::credentials::resolve_kalshi_ws_credentials()?;

    let timestamp_ms = chrono::Utc::now().timestamp_millis().to_string();
    let message = format!("{timestamp_ms}GET{path_to_sign}");
    let signature = kalshi_sign_pss_base64(&creds.private_key_pem, message.as_bytes())?;

    let mut request = ws_url
        .into_client_request()
        .map_err(|e| format!("build kalshi ws request failed: {e}"))?;
    request.headers_mut().insert(
        "KALSHI-ACCESS-KEY",
        HeaderValue::from_str(&creds.key_id)
            .map_err(|e| format!("invalid KALSHI-ACCESS-KEY header: {e}"))?,
    );
    request.headers_mut().insert(
        "KALSHI-ACCESS-SIGNATURE",
        HeaderValue::from_str(&signature)
            .map_err(|e| format!("invalid KALSHI-ACCESS-SIGNATURE header: {e}"))?,
    );
    request.headers_mut().insert(
        "KALSHI-ACCESS-TIMESTAMP",
        HeaderValue::from_str(&timestamp_ms)
            .map_err(|e| format!("invalid KALSHI-ACCESS-TIMESTAMP header: {e}"))?,
    );
    Ok(request)
}

fn kalshi_ws_signature_path(ws_url: &str) -> String {
    let no_scheme = ws_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(ws_url);
    let path = no_scheme
        .split_once('/')
        .map(|(_, path)| format!("/{}", path.trim_start_matches('/')))
        .unwrap_or_else(|| "/".to_string());
    if path.trim().is_empty() {
        "/".to_string()
    } else {
        path
    }
}

fn kalshi_sign_pss_base64(pem: &str, payload: &[u8]) -> std::result::Result<String, String> {
    let key = RsaPrivateKey::from_pkcs1_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs8_pem(pem))
        .map_err(|e| format!("parse Kalshi private key failed: {e}"))?;
    let mut rng = rand::thread_rng();
    let digest = Sha256::digest(payload);
    let signature = key
        .sign_with_rng(&mut rng, Pss::new::<Sha256>(), digest.as_slice())
        .map_err(|e| format!("kalshi signature failed: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(signature))
}

fn kalshi_ticker_probability(msg: &serde_json::Map<String, serde_json::Value>) -> Option<f64> {
    fn json_to_f64(v: Option<&serde_json::Value>) -> Option<f64> {
        match v {
            Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
            Some(serde_json::Value::Number(n)) => n.as_f64(),
            _ => None,
        }
    }
    fn normalize_prob(v: f64) -> f64 {
        if v > 1.0 {
            (v / 100.0).clamp(0.0, 1.0)
        } else {
            v.clamp(0.0, 1.0)
        }
    }

    if let Some(v) = json_to_f64(msg.get("last_price_dollars")) {
        return Some(normalize_prob(v));
    }
    if let Some(v) = json_to_f64(msg.get("last_price")) {
        return Some(normalize_prob(v));
    }

    let bid = json_to_f64(msg.get("yes_bid_dollars"))
        .or_else(|| json_to_f64(msg.get("yes_bid")))
        .map(normalize_prob);
    let ask = json_to_f64(msg.get("yes_ask_dollars"))
        .or_else(|| json_to_f64(msg.get("yes_ask")))
        .map(normalize_prob);
    match (bid, ask) {
        (Some(b), Some(a)) => Some(((b + a) / 2.0).clamp(0.0, 1.0)),
        (Some(v), None) | (None, Some(v)) => Some(v),
        _ => None,
    }
}

fn sync_state_market_to_listed_market(
    source: &str,
    state: &OddsSyncMarketState,
) -> OddsListedMarket {
    OddsListedMarket {
        ticker: state.ticker.clone(),
        title: state.title.clone(),
        event_ticker: state.event_ticker.clone(),
        yes_price: state.yes_price,
        volume: state.volume,
        status: state.status.clone(),
        source: Some(source.to_string()),
        market_id: if source.eq_ignore_ascii_case("polymarket") {
            Some(state.ticker.clone())
        } else {
            None
        },
        event_id: if source.eq_ignore_ascii_case("polymarket")
            && !state.event_ticker.trim().is_empty()
        {
            Some(state.event_ticker.clone())
        } else {
            None
        },
        slug: None,
        outcomes: None,
        outcome_prices: None,
        clob_token_ids: state.clob_token_ids.clone(),
        probability_yes: state.probability_yes,
        category: state.category.clone(),
    }
}

fn source_profile_state_key(source: &str, include_sports: bool) -> String {
    let profile = if include_sports { "all" } else { "non_sports" };
    format!("{source}::{profile}")
}
