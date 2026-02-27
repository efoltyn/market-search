pub async fn fetch_macro(req: MacroRequest) -> Result<MacroResponse> {
    let started = std::time::Instant::now();
    let now = Utc::now();
    let policy_mode = req.policy_mode.unwrap_or_default();
    let policy_file = req
        .policy_file
        .as_deref()
        .map(std::path::Path::new);
    let resolved_policy = crate::finance::policy::load_policy(policy_file, policy_mode)?;
    let indicators = resolved_policy.policy.macro_catalog.indicators.clone();
    let range = req.range.unwrap_or(Span {
        n: 1,
        unit: SpanUnit::Year,
    });
    let end = now;
    let mut start = end - range.approx_duration() - Duration::days(400); // extra for 1y change
    let compare_to_dt = req.compare_to.and_then(|d| {
        d.and_hms_opt(23, 59, 59)
            .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
    });
    if let Some(cmp) = compare_to_dt {
        let compare_start = cmp - Duration::days(400);
        if compare_start < start {
            start = compare_start;
        }
    }

    // Fetch indicators with bounded concurrency (8 at a time) to avoid FRED rate limits.
    use futures::stream::{self, StreamExt};
    let granularity = Span { n: 1, unit: SpanUnit::Month };
    let quarterly = Span { n: 3, unit: SpanUnit::Month };
    let freshness_policy = resolved_policy.policy.freshness.clone();
    let out: Vec<MacroIndicator> =
        stream::iter(indicators.iter().map(|indicator| {
        let id = indicator.id.clone();
        let name = indicator.name.clone();
        let category = indicator.category.clone();
        let compare_to_dt = compare_to_dt.clone();
        let freshness_policy = freshness_policy.clone();
        let collected_at = now;
        async move {
            // Try monthly first; fall back to quarterly for GDP-type series.
            let series = fetch_fred_series(&[id.clone()], start, end, granularity).await;
            let candles = match series {
                Ok((mut svec, _)) => svec.pop().and_then(|s| {
                    if s.candles.is_empty() { None } else { Some(s.candles) }
                }),
                Err(_) => None,
            };
            let candles = match candles {
                Some(c) => c,
                None => {
                    // Retry with quarterly granularity (e.g. GDPC1)
                    match fetch_fred_series(&[id.clone()], start, end, quarterly).await {
                        Ok((mut svec, _)) => svec.pop().map(|s| s.candles).unwrap_or_default(),
                        Err(_) => return None,
                    }
                }
            };
            if let Some(latest) = candles.last() {
                let mut change_1y = None;
                let lookback = if candles.len() > 12 { 13 } else if candles.len() > 4 { 5 } else { 0 };
                if lookback > 0 {
                    let ago = &candles[candles.len().saturating_sub(lookback)];
                    if ago.c != 0.0 {
                        change_1y = Some((latest.c - ago.c) / ago.c * 100.0);
                    }
                }
                let mut compare_value = None;
                let mut delta_abs = None;
                let mut delta_pct = None;
                if let Some(compare_ts) = compare_to_dt {
                    if let Some(anchor) = candles.iter().rev().find(|c| c.t <= compare_ts) {
                        compare_value = Some(anchor.c);
                        let dabs = latest.c - anchor.c;
                        delta_abs = Some(dabs);
                        if anchor.c != 0.0 {
                            delta_pct = Some((dabs / anchor.c) * 100.0);
                        }
                    }
                }
                let freshness = crate::finance::policy::freshness_from_observed(
                    latest.t,
                    collected_at,
                    &freshness_policy,
                    FreshnessOrigin::ProviderTimestamp,
                    FreshnessQuality::Exact,
                );
                return Some(MacroIndicator {
                    symbol: id,
                    name,
                    category,
                    current_value: latest.c,
                    change_1y,
                    freshness,
                    compare_value,
                    delta_abs,
                    delta_pct,
                });
            }
            None
        }
    }))
    .buffer_unordered(8)
    .filter_map(|x| async { x })
    .collect()
    .await;

    let data_as_of = out.iter().map(|i| i.freshness.observed_at).max();
    let max_age_seconds = out.iter().map(|i| i.freshness.age_seconds).max();
    let stale_count = out
        .iter()
        .filter(|i| matches!(i.freshness.state, FreshnessState::Stale))
        .count();
    let decision_trace = vec![
        format!("catalog_indicators={}", indicators.len()),
        format!("fetched_indicators={}", out.len()),
        "policy_driven_catalog=true".to_string(),
    ];

    Ok(MacroResponse {
        generated_at: now,
        schema_version: "finance.macro.v2".to_string(),
        freshness_summary: FreshnessSummary {
            data_as_of,
            max_age_seconds,
            stale_count,
        },
        applied_policy: AppliedPolicy {
            mode: resolved_policy.mode,
            sources: resolved_policy.sources,
        },
        decision_trace,
        run_meta: RunMeta {
            latency_ms: started.elapsed().as_millis() as u64,
            stdout_chars: 0,
            stored_bytes: 0,
            coverage_counts: std::collections::BTreeMap::from([(
                "indicators".to_string(),
                out.len(),
            )]),
            token_efficiency: None,
        },
        indicators: out,
    })
}
