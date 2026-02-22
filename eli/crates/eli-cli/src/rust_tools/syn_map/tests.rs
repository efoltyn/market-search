#[cfg(test)]
fn detect_source_kind_for_path(path: &Path) -> eli_core::meta::SourceKind {
    use eli_core::meta::SourceKind;

    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("json") => return SourceKind::Json,
        Some("csv") => return SourceKind::Csv,
        Some("ndjson") | Some("jsonl") => return SourceKind::Ndjson,
        _ => {}
    }

    let Ok(raw) = std::fs::read_to_string(path) else {
        return SourceKind::Unknown;
    };
    let text = raw.trim_start();
    if text.starts_with('{') || text.starts_with('[') {
        return SourceKind::Json;
    }
    if text.lines().all(|line| {
        let line = line.trim();
        line.is_empty() || serde_json::from_str::<serde_json::Value>(line).is_ok()
    }) {
        return SourceKind::Ndjson;
    }
    if text.contains(',') && text.lines().count() >= 2 {
        return SourceKind::Csv;
    }

    SourceKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn mk_temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn suppressed_summary_includes_schema_pattern_lines() {
        let value = serde_json::json!({
            "provider": "yahoo",
            "tickers": ["SPY"],
            "series": [{"ticker":"SPY","candles":[{"c":1.0},{"c":2.0}]}]
        });
        let summary = format_suppressed_summary("eli finance timeseries", &value, 12, 200);
        assert!(summary.contains("schema_root="));
        assert!(summary.contains("schema_paths="));
        assert!(summary.contains("nullable_fields="));
    }

    #[test]
    fn data_sidecar_gate_detects_missing_meta() {
        let dir = mk_temp_dir("eli_cli_gate");
        let data = dir.join("payload.json");
        std::fs::write(&data, "{\"x\":1}").expect("write data");
        std::fs::write(dir.join("notes.txt"), "ignore me").expect("write notes");

        let missing_first = missing_data_sidecars(&dir).expect("missing sidecars");
        assert_eq!(missing_first.len(), 1);
        assert_eq!(missing_first[0], data);

        let sidecar = eli_core::meta::sidecar_path_for(&data);
        std::fs::write(&sidecar, "{}").expect("write sidecar");
        assert!(missing_data_sidecars(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_source_kind_sniffs_json_when_extension_unknown() {
        let dir = mk_temp_dir("eli_cli_probe");
        let path = dir.join("mystery.bin");
        std::fs::write(&path, "{\"alpha\":1,\"beta\":2}").expect("write probe sample");
        let kind = detect_source_kind_for_path(&path);
        assert!(matches!(kind, eli_core::meta::SourceKind::Json));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_json_out_with_meta_writes_sidecar() {
        let dir = mk_temp_dir("eli_cli_out_meta");
        let out = dir.join("payload.json");
        let payload = serde_json::json!({"x": 1, "y": [1,2,3]});
        let wr =
            write_json_out_with_meta(out.clone(), &payload, "test.tool", &["arg=a".to_string()])
                .expect("write out+meta");
        assert!(wr.out_path.exists());
        assert!(wr.meta_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_json_out_with_meta_odds_sidecar_has_units_and_scale_hints() {
        let dir = mk_temp_dir("eli_cli_odds_meta");
        let out = dir.join("odds.json");
        let payload = serde_json::json!({
            "markets": [
                {"probability_yes": 0.23, "yes_price": 23, "volume": 223483}
            ]
        });
        let wr = write_json_out_with_meta(
            out,
            &payload,
            "finance.odds",
            &["provider=polymarket".to_string()],
        )
        .expect("write odds out+meta");
        let raw = std::fs::read_to_string(&wr.meta_path).expect("read sidecar");
        let meta: serde_json::Value = serde_json::from_str(&raw).expect("parse sidecar");
        let paths = meta
            .get("path_index")
            .and_then(|v| v.as_array())
            .expect("path_index");
        let prob = paths
            .iter()
            .find(|e| {
                e.get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| p == "$.markets[].probability_yes")
                    .unwrap_or(false)
            })
            .expect("probability entry");
        assert_eq!(
            prob.get("probability_scale").and_then(|v| v.as_str()),
            Some("0_to_1")
        );
        let volume = paths
            .iter()
            .find(|e| {
                e.get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| p == "$.markets[].volume")
                    .unwrap_or(false)
            })
            .expect("volume entry");
        assert_eq!(volume.get("units").and_then(|v| v.as_str()), Some("cents"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forex_command_digest_includes_usd_summary_fields() {
        let payload = serde_json::json!({
            "basket": "usd_filtered",
            "pair_count": 2,
            "summary": {
                "usd_strength_score_pct": -1.23,
                "strongest_usd_pair": "USD/JPY",
                "weakest_usd_pair": "EUR/USD"
            },
            "event_window": {
                "event_at": "2026-02-20T20:00:00Z",
                "shift_usd_strength_pct": 0.42,
                "session_attribution": [
                    {"session":"us","move_count":14}
                ]
            },
            "comparison_deltas": [
                {
                    "as_of": "2026-01-31T23:59:59Z",
                    "delta_usd_strength_pct": 0.31,
                    "delta_usd_pairs_up": 1,
                    "delta_usd_pairs_down": -1
                }
            ],
            "delta_context": {
                "previous_as_of": "2026-02-20T13:00:00Z",
                "current_as_of": "2026-02-20T21:00:00Z",
                "compared_pairs": 2,
                "changed_pairs": 1,
                "top_pair_deltas": [
                    {
                        "pair": "USD/JPY",
                        "previous_usd_change_pct": 0.7,
                        "current_usd_change_pct": 1.9,
                        "delta_usd_change_pct": 1.2
                    }
                ]
            },
            "usd_benchmark": {
                "source": "fred",
                "symbol": "DTWEXBGS",
                "as_of": "2026-02-20T00:00:00Z",
                "change_pct": 2.1
            },
            "biggest_daily_usd_moves": [
                {"pair": "EUR/USD", "date": "2025-04-10", "usd_impact_pct": -2.8}
            ]
        });

        let digest = digest_from_json_for_command("eli finance forex", &payload, 512);
        assert!(digest.contains("basket=usd_filtered"));
        assert!(digest.contains("pairs=2"));
        assert!(digest.contains("usd_strength=-1.23%"));
        assert!(digest.contains("strongest_usd=USD/JPY"));
        assert!(digest.contains("weakest_usd=EUR/USD"));
        assert!(digest.contains("comparisons=1"));
        assert!(digest.contains("pair_delta=1/2"));
        assert!(digest.contains("top_pair_delta=USD/JPY:1.20%"));
        assert!(digest.contains("DTWEXBGS=2.10%"));
    }

    #[test]
    fn shared_manifest_context_is_prepended() {
        let task = "Compute recession probability.";
        let enriched =
            prepend_shared_manifest_context(task, Path::new("/tmp/shared_manifest.json"));
        assert!(enriched.contains("/tmp/shared_manifest.json"));
        assert!(enriched.contains("artifact paths + sidecars"));
        assert!(enriched.ends_with(task));
    }

    #[test]
    fn auto_out_path_uses_dimensional_timeseries_name() {
        let dir = mk_temp_dir("eli_cli_auto_name");
        let out = dir.join("auto.json");
        let payload = serde_json::json!({
            "provider": "yahoo",
            "tickers": ["NVDA","INTC","AMD"],
            "series": []
        });
        let wr = write_json_out_with_meta(
            out,
            &payload,
            "finance.timeseries",
            &["range=1y".to_string(), "granularity=5min".to_string()],
        )
        .expect("write auto out+meta");
        let name = wr
            .out_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        assert!(name.starts_with("TIMESERIES_AMD_INTC_NVDA_1YR_5MIN_YAHOO_"));
        assert!(name.ends_with(".json"));
        assert!(!name.contains("step001"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shadow_pipeline_writes_meta_for_saved_outputs() {
        let dir = mk_temp_dir("eli_cli_shadow");
        let candles = (0..256)
            .map(|i| serde_json::json!({"t": i, "c": i as f64 + 100.0, "v": i + 1}))
            .collect::<Vec<_>>();
        let payload = serde_json::json!({
            "provider": "mock",
            "series": [{"ticker":"SPY","candles": candles}]
        });
        let stdout = serde_json::to_string_pretty(&payload).expect("serialize payload");
        assert!(stdout.len() > 2048, "payload should trigger suppression");

        let result = CommandResult {
            command: "eli finance timeseries --tickers SPY --range 1d --granularity 5min"
                .to_string(),
            returncode: 0,
            stdout,
            stderr: String::new(),
            duration_ms: 1,
            allowed: true,
            deny_reason: None,
        };
        let out = shadow_large_tool_outputs(&dir, "sess_1", 1, &[result]);
        assert_eq!(out.len(), 1);
        assert!(out[0].stdout.contains("[OUTPUT SUPPRESSED]"));
        assert!(out[0].stdout.contains("schema_root="));

        let last = dir.join("eli_research/data/.last_tool_output.json");
        assert!(last.exists());
        assert!(eli_core::meta::sidecar_path_for(&last).exists());

        let archive_dir = dir.join("eli_research/data/tool_outputs/sess_1");
        let mut archive_jsons = std::fs::read_dir(&archive_dir)
            .expect("read archive dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("json")
                    && !p.display().to_string().ends_with(".meta.json")
            })
            .collect::<Vec<_>>();
        archive_jsons.sort();
        assert!(!archive_jsons.is_empty(), "expected archived json output");
        let archive = archive_jsons[0].clone();
        let archive_name = archive
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        assert!(archive_name.starts_with("TIMESERIES_SPY_1D_5MIN_MOCK_"));
        assert!(eli_core::meta::sidecar_path_for(&archive).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunk_text_for_swarm_respects_requested_chunks() {
        let text = "abcdefghijklmnopqrstuvwxyz0123456789";
        let chunks = chunk_text_for_swarm(text, Some(3), 10, 0, 10);
        assert_eq!(chunks.len(), 3);
        let combined = chunks.join("");
        assert_eq!(combined, text);
    }

    #[test]
    fn chunk_text_for_swarm_respects_requested_chunks_with_overlap() {
        let text = "abcdefghijklmnopqrstuvwxyz0123456789";
        let chunks = chunk_text_for_swarm(text, Some(4), 10, 2, 10);
        assert_eq!(chunks.len(), 4);
    }

    #[test]
    fn chunk_text_for_swarm_applies_overlap() {
        let text = "abcdefghij1234567890";
        let chunks = chunk_text_for_swarm(text, None, 10, 2, 10);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0], "abcdefghij");
        assert!(chunks[1].starts_with("ij"));
    }

    #[test]
    fn web_search_summary_digest_uses_new_schema() {
        let payload = serde_json::json!({
            "query": "fed decision",
            "mode": "news",
            "items": [
                {
                    "rank": 1,
                    "title": "Fed Officials Signal Patience",
                    "url": "https://www.reuters.com/world/us/fed-officials-2026-02-21/",
                    "domain": "reuters.com"
                }
            ],
            "stats": {
                "probed_items": 1
            },
            "run_delta": {
                "new_urls": ["https://www.reuters.com/world/us/fed-officials-2026-02-21/"],
                "dropped_urls": []
            }
        });
        let digest = digest_from_json_for_command("eli web search --query fed", &payload, 256);
        assert!(digest.contains("items=1"));
        assert!(digest.contains("mode=news"));
        assert!(digest.contains("top_domain=reuters.com"));
        assert!(digest.contains("delta=+1/-0"));
    }

    #[test]
    fn web_read_summary_digest_handles_batch_schema() {
        let payload = serde_json::json!({
            "mode": "batch",
            "requested": 3,
            "deduped": 2,
            "completed": 2,
            "success_count": 1,
            "partial_count": 0,
            "blocked_count": 1,
            "error_count": 0,
            "results": []
        });
        let digest = digest_from_json_for_command("eli web read --url a,b", &payload, 256);
        assert!(digest.contains("completed=2"));
        assert!(digest.contains("success=1"));
        assert!(digest.contains("blocked=1"));
    }
}
