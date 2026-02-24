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
    load_sync_state, write_delta_index, write_sync_state, SourceBaselineQuality,
};
use crate::Result;
use chrono::Utc;
use std::path::PathBuf;

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
                match sync_kalshi_events(&kalshi_limiter, max_pages, include_sports).await {
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
            let previous_state = sync_state.sources.get(&source);
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
                sync_state.sources.insert(source.clone(), next_state);
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
            "[sync-delta] overall: previous_markets={} current_markets={} changed={} unchanged={} new={} removed={}",
            summary.previous_markets,
            summary.current_markets,
            summary.changed_markets,
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
        "[sync-delta] {}: previous={} current={} changed={} unchanged={} new={} removed={}",
        delta.source,
        delta.previous_markets,
        delta.current_markets,
        delta.changed_markets,
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
