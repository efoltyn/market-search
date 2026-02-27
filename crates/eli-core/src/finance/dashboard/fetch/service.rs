pub async fn fetch_dashboard(req: DashboardRequest) -> Result<DashboardResponse> {
    let preset = req.preset.trim().to_ascii_lowercase();
    match preset.as_str() {
        "recession" => fetch_dashboard_recession(req.max_ms).await,
        "tech_megacap" => fetch_dashboard_generic(
            "tech_megacap",
            req.max_ms,
            &[
                ("snapshot_megacap", vec!["NVDA", "AAPL", "MSFT", "GOOGL", "META", "AMZN", "TSLA"]),
                ("snapshot_semis",   vec!["AMD", "INTC", "AVGO", "QCOM", "MU"]),
            ],
            &["AI chips", "semiconductor tariff", "big tech earnings"],
        ).await,
        _ => Err(Error::InvalidInput(format!(
            "unsupported --preset '{preset}' (supported: recession, tech_megacap)"
        ))),
    }
}

async fn fetch_dashboard_recession(max_ms: Option<u64>) -> Result<DashboardResponse> {

    let macro_fut = async {
        fetch_macro(MacroRequest {
            range: None,
            compare_to: None,
            policy_file: None,
            policy_mode: None,
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
        if max_ms.unwrap_or(0) > 0 && max_ms.unwrap_or(0) <= 5_000 {
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
        if max_ms.unwrap_or(0) > 0 && max_ms.unwrap_or(0) <= 5_000 {
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

    let macro_ms = section_budget_ms(max_ms, 15_000);
    let snapshot_ms = section_budget_ms(max_ms, 12_000);
    let odds_ms = section_budget_ms(max_ms, 10_000);
    let options_ms = section_budget_ms(max_ms, 20_000);
    let rate_ms = section_budget_ms(max_ms, 20_000);

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
        preset: "recession".to_string(),
        generated_at: now,
        as_of,
        age_seconds,
        macro_data,
        snapshots,
        odds,
        options,
        rate_path,
        sections: BTreeMap::new(),
        section_health: Some(health),
        warnings,
    })
}

// Generic preset runner: runs snapshot groups + odds searches in parallel, returns
// everything in `sections`. Adding a new preset is one new match arm in fetch_dashboard
// — no type changes to DashboardResponse needed.
async fn fetch_dashboard_generic(
    preset_name: &str,
    max_ms: Option<u64>,
    snapshot_groups: &[(&str, Vec<&str>)],
    odds_queries: &[&str],
) -> Result<DashboardResponse> {
    use futures::future::join_all;

    let now = Utc::now();
    let budget_ms = max_ms.unwrap_or(30_000);
    let mut sections: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut health: BTreeMap<String, SectionHealth> = BTreeMap::new();

    // Snapshot groups — run all in parallel.
    let snap_futs: Vec<_> = snapshot_groups
        .iter()
        .map(|(name, tickers)| {
            let tickers: Vec<String> = tickers.iter().map(|t| t.to_string()).collect();
            let name = name.to_string();
            async move {
                let result = timeout(
                    TokioDuration::from_millis(budget_ms.min(15_000)),
                    fetch_snapshot(SnapshotRequest {
                        tickers,
                        provider: ProviderKind::Yahoo,
                    }),
                )
                .await;
                (name, result)
            }
        })
        .collect();

    for (name, result) in join_all(snap_futs).await {
        match result {
            Ok(Ok(v)) => {
                let n = v.snapshots.len();
                health.insert(name.clone(), SectionHealth {
                    available: true,
                    coverage_ratio: (n as f64 / 5.0).clamp(0.0, 1.0),
                    confidence: 0.85,
                    as_of: Some(v.generated_at),
                    age_seconds: Some((now - v.generated_at).num_seconds().max(0)),
                    notes: Vec::new(),
                });
                let value = serde_json::to_value(&v).unwrap_or(serde_json::Value::Null);
                sections.insert(name, value);
            }
            Ok(Err(e)) => {
                warnings.push(format!("{name}: {e}"));
                health.insert(name.clone(), SectionHealth { available: false, coverage_ratio: 0.0, confidence: 0.0, as_of: None, age_seconds: None, notes: vec![e.to_string()] });
            }
            Err(_) => {
                warnings.push(format!("{name}: timed out"));
                health.insert(name.clone(), SectionHealth { available: false, coverage_ratio: 0.0, confidence: 0.0, as_of: None, age_seconds: None, notes: vec!["timed out".to_string()] });
            }
        }
    }

    // Odds searches — run all in parallel.
    let odds_futs: Vec<_> = odds_queries
        .iter()
        .map(|q| {
            let q = q.to_string();
            async move {
                let result = timeout(
                    TokioDuration::from_millis(budget_ms.min(10_000)),
                    async { search_odds_csv(&q, 20) },
                )
                .await;
                (q, result)
            }
        })
        .collect();

    let mut odds_out: Vec<serde_json::Value> = Vec::new();
    for (q, result) in join_all(odds_futs).await {
        match result {
            Ok(Ok(v)) => odds_out.push(serde_json::to_value(&v).unwrap_or(serde_json::Value::Null)),
            Ok(Err(e)) => warnings.push(format!("odds({q}): {e}")),
            Err(_) => warnings.push(format!("odds({q}): timed out")),
        }
    }
    if !odds_out.is_empty() {
        sections.insert("odds".to_string(), serde_json::Value::Array(odds_out));
    }

    if sections.is_empty() {
        return Err(Error::Provider(format!("all sections failed for preset {preset_name}")));
    }

    let as_of = health.values().filter_map(|h| h.as_of).max().unwrap_or(now);
    let age_seconds = (now - as_of).num_seconds().max(0);
    Ok(DashboardResponse {
        preset: preset_name.to_string(),
        generated_at: now,
        as_of,
        age_seconds,
        macro_data: None,
        snapshots: None,
        odds: None,
        options: None,
        rate_path: None,
        sections,
        section_health: Some(health),
        warnings,
    })
}
