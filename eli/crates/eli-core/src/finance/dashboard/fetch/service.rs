pub async fn fetch_dashboard(req: DashboardRequest) -> Result<DashboardResponse> {
    let preset = req.preset.trim().to_ascii_lowercase();
    if preset != "recession" {
        return Err(Error::InvalidInput(
            "unsupported --preset (v1 supports: recession)".to_string(),
        ));
    }

    let macro_fut = async {
        fetch_macro(MacroRequest {
            range: None,
            compare_to: None,
        })
        .await
    };
    let snapshot_fut = async {
        fetch_snapshot(SnapshotRequest {
            tickers: vec![
                "SPY".to_string(),
                "TLT".to_string(),
                "HYG".to_string(),
                "GLD".to_string(),
                "UUP".to_string(),
            ],
            provider: ProviderKind::Yahoo,
        })
        .await
    };
    let odds_fut = async {
        if req.max_ms.unwrap_or(0) > 0 && req.max_ms.unwrap_or(0) <= 5_000 {
            return Err(Error::Provider(
                "skipped due tight max-ms budget".to_string(),
            ));
        }
        let queries = ["recession", "unemployment", "federal reserve"];
        let mut out = Vec::new();
        for q in queries {
            out.push(search_odds_csv(q, 25)?);
        }
        Ok::<Vec<DashboardOddsSearch>, Error>(out)
    };
    let options_fut = async {
        if req.max_ms.unwrap_or(0) > 0 && req.max_ms.unwrap_or(0) <= 5_000 {
            return Err(Error::Provider(
                "skipped due tight max-ms budget".to_string(),
            ));
        }
        fetch_options(OptionsRequest {
            ticker: "SPY".to_string(),
            expiry: None,
            option_type: None,
            near_money_pct: None,
            summary_only: true,
            list_expirations: false,
            multi_expiry: false,
            num_expiries: None,
        })
        .await
    };
    let rate_path_fut = async {
        fetch_rate_path(RatePathRequest {
            cache_dir: None,
            source_mode: Some(RatePathSourceMode::Auto),
        })
        .await
    };

    fn section_budget_ms(global_ms: Option<u64>, default_cap_ms: u64) -> Option<u64> {
        match global_ms {
            Some(ms) if ms > 0 => Some(ms.min(default_cap_ms)),
            Some(_) => None,
            None => Some(default_cap_ms),
        }
    }

    let macro_ms = section_budget_ms(req.max_ms, 15_000);
    let snapshot_ms = section_budget_ms(req.max_ms, 12_000);
    let odds_ms = section_budget_ms(req.max_ms, 10_000);
    let options_ms = section_budget_ms(req.max_ms, 20_000);
    let rate_ms = section_budget_ms(req.max_ms, 20_000);

    let macro_fut = async {
        if let Some(ms) = macro_ms {
            timeout(TokioDuration::from_millis(ms), macro_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            macro_fut.await
        }
    };
    let snapshot_fut = async {
        if let Some(ms) = snapshot_ms {
            timeout(TokioDuration::from_millis(ms), snapshot_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            snapshot_fut.await
        }
    };
    let odds_fut = async {
        if let Some(ms) = odds_ms {
            timeout(TokioDuration::from_millis(ms), odds_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            odds_fut.await
        }
    };
    let options_fut = async {
        if let Some(ms) = options_ms {
            timeout(TokioDuration::from_millis(ms), options_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            options_fut.await
        }
    };
    let rate_path_fut = async {
        if let Some(ms) = rate_ms {
            timeout(TokioDuration::from_millis(ms), rate_path_fut)
                .await
                .map_err(|_| Error::Provider(format!("timed out after {ms}ms")))?
        } else {
            rate_path_fut.await
        }
    };

    let (macro_r, snap_r, odds_r, options_r, rate_r) = tokio::join!(
        macro_fut,
        snapshot_fut,
        odds_fut,
        options_fut,
        rate_path_fut
    );

    let mut warnings = Vec::new();
    let mut health: BTreeMap<String, SectionHealth> = BTreeMap::new();
    let now = Utc::now();
    let macro_data = match macro_r {
        Ok(v) => {
            let coverage = (v.indicators.len() as f64 / 32.0).clamp(0.0, 1.0);
            health.insert(
                "macro".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: coverage,
                    confidence: (0.55 + (coverage * 0.35)).clamp(0.0, 0.99),
                    as_of: Some(v.generated_at),
                    age_seconds: Some((now - v.generated_at).num_seconds().max(0)),
                    notes: Vec::new(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("macro: {e}"));
            health.insert(
                "macro".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };
    let snapshots = match snap_r {
        Ok(v) => {
            let coverage = (v.snapshots.len() as f64 / 5.0).clamp(0.0, 1.0);
            health.insert(
                "snapshot".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: coverage,
                    confidence: (0.55 + (coverage * 0.35)).clamp(0.0, 0.99),
                    as_of: Some(v.generated_at),
                    age_seconds: Some((now - v.generated_at).num_seconds().max(0)),
                    notes: Vec::new(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("snapshot: {e}"));
            health.insert(
                "snapshot".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };
    let odds = match odds_r {
        Ok(v) => {
            let covered = v.iter().filter(|s| !s.markets.is_empty()).count();
            let coverage = (covered as f64 / 3.0).clamp(0.0, 1.0);
            health.insert(
                "odds".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: coverage,
                    confidence: (0.5 + (coverage * 0.35)).clamp(0.0, 0.95),
                    as_of: Some(now),
                    age_seconds: Some(0),
                    notes: Vec::new(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("odds: {e}"));
            health.insert(
                "odds".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };
    let options = match options_r {
        Ok(v) => {
            health.insert(
                "options".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: 1.0,
                    confidence: 0.75,
                    as_of: Some(v.generated_at),
                    age_seconds: Some((now - v.generated_at).num_seconds().max(0)),
                    notes: Vec::new(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("options: {e}"));
            health.insert(
                "options".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };
    let rate_path = match rate_r {
        Ok(v) => {
            health.insert(
                "rate_path".to_string(),
                SectionHealth {
                    available: true,
                    coverage_ratio: v.coverage_ratio,
                    confidence: v.confidence,
                    as_of: Some(v.as_of),
                    age_seconds: Some(v.age_seconds),
                    notes: v.warnings.clone(),
                },
            );
            Some(v)
        }
        Err(e) => {
            warnings.push(format!("rate_path: {e}"));
            health.insert(
                "rate_path".to_string(),
                SectionHealth {
                    available: false,
                    coverage_ratio: 0.0,
                    confidence: 0.0,
                    as_of: None,
                    age_seconds: None,
                    notes: vec![e.to_string()],
                },
            );
            None
        }
    };

    if macro_data.is_none()
        && snapshots.is_none()
        && odds.is_none()
        && options.is_none()
        && rate_path.is_none()
    {
        return Err(Error::Provider(
            "all dashboard sections failed for preset recession".to_string(),
        ));
    }

    let as_of = health.values().filter_map(|h| h.as_of).max().unwrap_or(now);
    let age_seconds = (now - as_of).num_seconds().max(0);
    Ok(DashboardResponse {
        preset,
        generated_at: now,
        as_of,
        age_seconds,
        macro_data,
        snapshots,
        odds,
        options,
        rate_path,
        section_health: Some(health),
        warnings,
    })
}
