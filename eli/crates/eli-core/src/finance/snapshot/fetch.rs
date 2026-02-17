use super::super::timeseries::fetch::{
    build_snapshot_analytics, fetch_yahoo_snapshots, generate_mock_snapshots,
};
use super::super::*;

pub async fn fetch_snapshot(req: SnapshotRequest) -> Result<SnapshotResponse> {
    let tickers = normalize_tickers(&req.tickers);
    if tickers.is_empty() {
        return Err(Error::InvalidInput(
            "at least one ticker is required".to_string(),
        ));
    }

    let generated_at = Utc::now();
    let snapshots = match req.provider {
        ProviderKind::Mock => generate_mock_snapshots(&tickers),
        ProviderKind::Yahoo => fetch_yahoo_snapshots(&tickers).await?,
        ProviderKind::Fred => {
            return Err(Error::InvalidInput(
                "fred provider does not support snapshot (use timeseries for macro series IDs)"
                    .to_string(),
            ))
        }
    };
    let analytics = build_snapshot_analytics(&snapshots);

    Ok(SnapshotResponse {
        provider: req.provider,
        tickers,
        generated_at,
        snapshots,
        analytics: Some(analytics),
        trailing_returns: None,
    })
}
