use super::super::timeseries::fetch::{
    build_snapshot_analytics, fetch_yahoo_snapshots, generate_mock_snapshots,
};
use super::super::*;

pub async fn fetch_snapshot(req: SnapshotRequest) -> Result<SnapshotResponse> {
    let started = std::time::Instant::now();
    let tickers = normalize_tickers(&req.tickers);
    if tickers.is_empty() {
        return Err(Error::InvalidInput(
            "at least one ticker is required".to_string(),
        ));
    }

    let generated_at = Utc::now();
    let resolved_policy = crate::finance::policy::load_policy(None, PolicyMode::Observe)?;
    let snapshots = match req.provider {
        ProviderKind::Mock => generate_mock_snapshots(&tickers),
        ProviderKind::Yahoo => fetch_yahoo_snapshots(
            &tickers,
            generated_at,
            &resolved_policy.policy.freshness,
        )
        .await?,
        ProviderKind::Fred => {
            return Err(Error::InvalidInput(
                "fred provider does not support snapshot (use timeseries for macro series IDs)"
                    .to_string(),
            ))
        }
    };
    let analytics = build_snapshot_analytics(&snapshots);
    let data_as_of = snapshots.iter().map(|s| s.freshness.observed_at).max();
    let max_age_seconds = snapshots.iter().map(|s| s.freshness.age_seconds).max();
    let stale_count = snapshots
        .iter()
        .filter(|s| matches!(s.freshness.state, FreshnessState::Stale))
        .count();
    let snapshot_count = snapshots.len();

    Ok(SnapshotResponse {
        provider: req.provider,
        tickers,
        generated_at,
        snapshots,
        schema_version: "finance.snapshot.v2".to_string(),
        freshness_summary: FreshnessSummary {
            data_as_of,
            max_age_seconds,
            stale_count,
        },
        applied_policy: AppliedPolicy {
            mode: resolved_policy.mode,
            sources: resolved_policy.sources,
        },
        decision_trace: vec!["policy_driven_freshness=true".to_string()],
        run_meta: RunMeta {
            latency_ms: started.elapsed().as_millis() as u64,
            stdout_chars: 0,
            stored_bytes: 0,
            coverage_counts: std::collections::BTreeMap::from([(
                "snapshots".to_string(),
                snapshot_count,
            )]),
            token_efficiency: None,
        },
        analytics: Some(analytics),
        trailing_returns: None,
    })
}
