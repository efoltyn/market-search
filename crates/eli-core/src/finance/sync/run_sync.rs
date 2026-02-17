use super::super::providers::{sync_kalshi_events, sync_polymarket_events};
use super::super::{
    default_odds_field_semantics, OddsListedEvent, OddsListedMarket, OddsSyncRequest,
    OddsSyncResponse, OddsSyncSourceResult, RateLimiter,
};
use super::analysis::{build_sync_analysis, build_sync_source_analytics, SyncAnalysisInput};
use super::csv_cache_writer::{merge_markets_csv, write_markets_csv};
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

    let max_pages = req.max_pages.unwrap_or(10);

    let sources = req
        .sources
        .unwrap_or_else(|| vec!["kalshi".to_string(), "polymarket".to_string()]);
    let do_kalshi = sources.iter().any(|s| s.eq_ignore_ascii_case("kalshi"));
    let do_polymarket = sources.iter().any(|s| s.eq_ignore_ascii_case("polymarket"));

    let kalshi_limiter = RateLimiter::new(250, 4000);
    let poly_limiter = RateLimiter::new(200, 4000);

    let mut source_results = Vec::new();
    let mut csv_paths = Vec::new();
    let mut analysis_inputs: Vec<SyncAnalysisInput> = Vec::new();

    struct SourceSyncPayload {
        result: OddsSyncSourceResult,
        events: Vec<OddsListedEvent>,
        markets: Vec<OddsListedMarket>,
    }

    let (kalshi_result, poly_result) = tokio::join!(
        async {
            if do_kalshi {
                let start = std::time::Instant::now();
                match sync_kalshi_events(&kalshi_limiter, max_pages).await {
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
                match sync_polymarket_events(&poly_limiter, max_pages).await {
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

    if let Some(payload) = kalshi_result {
        if let Some(ref p) = payload.result.csv_path {
            csv_paths.push(PathBuf::from(p));
        }
        if payload.result.ok {
            analysis_inputs.push(SyncAnalysisInput {
                source: payload.result.source.clone(),
                events: payload.events,
                markets: payload.markets,
            });
        }
        source_results.push(payload.result);
    }
    if let Some(payload) = poly_result {
        if let Some(ref p) = payload.result.csv_path {
            csv_paths.push(PathBuf::from(p));
        }
        if payload.result.ok {
            analysis_inputs.push(SyncAnalysisInput {
                source: payload.result.source.clone(),
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
    let analysis = if analysis_inputs.is_empty() {
        None
    } else {
        Some(build_sync_analysis(&analysis_inputs))
    };

    Ok(OddsSyncResponse {
        generated_at: Utc::now(),
        sources: source_results,
        total_events,
        total_markets,
        merged_csv_path: merged_path,
        analysis,
        field_semantics: default_odds_field_semantics(),
    })
}
