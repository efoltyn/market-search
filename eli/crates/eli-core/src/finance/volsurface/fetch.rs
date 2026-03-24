use eli_finance_types::{
    VolSurfaceDataPoint, VolSurfaceIndex, VolSurfaceRequest, VolSurfaceResponse,
};
use futures::stream::{self, StreamExt};

const CBOE_BASE: &str = "https://cdn.cboe.com/api/global/us_indices/daily_prices/";
const DEFAULT_SYMBOLS: &[&str] = &[
    "VIX", "VIX9D", "VIX3M", "VIX6M", "VIX1Y", "VVIX", "OVX", "GVZ", "SKEW",
];

/// Parse a single CSV row into a VolSurfaceDataPoint.
/// Handles two formats:
///   5-col: MM/DD/YYYY,open,high,low,close  (VIX, VIX9D, VIX3M, etc.)
///   2-col: MM/DD/YYYY,value                 (VVIX, SKEW)
fn parse_csv_row(line: &str) -> Option<VolSurfaceDataPoint> {
    let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    if parts.len() < 2 {
        return None;
    }

    // Parse MM/DD/YYYY → YYYY-MM-DD
    let date_parts: Vec<&str> = parts[0].split('/').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let (mm, dd, yyyy) = (date_parts[0], date_parts[1], date_parts[2]);
    let date = format!("{yyyy}-{mm}-{dd}");

    if parts.len() >= 5 {
        // 5-column OHLC format
        let open = parts[1].parse::<f64>().ok()?;
        let high = parts[2].parse::<f64>().ok()?;
        let low = parts[3].parse::<f64>().ok()?;
        let close = parts[4].parse::<f64>().ok()?;
        Some(VolSurfaceDataPoint { date, open, high, low, close })
    } else {
        // 2-column single-value format (VVIX, SKEW)
        let val = parts[1].parse::<f64>().ok()?;
        Some(VolSurfaceDataPoint { date, open: val, high: val, low: val, close: val })
    }
}

/// Fetch a single symbol's CSV from CBOE and parse into a VolSurfaceIndex.
async fn fetch_one(
    client: &reqwest::Client,
    symbol: &str,
    history_count: usize,
) -> Result<VolSurfaceIndex> {
    let url = format!("{CBOE_BASE}{symbol}_History.csv");
    let resp = client.get(&url).send().await.map_err(|e| {
        Error::Provider(format!("CBOE fetch failed for {symbol}: {e}"))
    })?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "CBOE returned {} for {symbol}",
            resp.status()
        )));
    }

    let body = resp.text().await.map_err(|e| {
        Error::Provider(format!("CBOE body read failed for {symbol}: {e}"))
    })?;

    // Parse CSV: skip header, skip empty lines
    let rows: Vec<VolSurfaceDataPoint> = body
        .lines()
        .skip(1) // header row
        .filter(|line| !line.trim().is_empty())
        .filter_map(parse_csv_row)
        .collect();

    if rows.is_empty() {
        return Err(Error::Provider(format!(
            "CBOE returned no valid data rows for {symbol}"
        )));
    }

    // Last row is the most recent
    let latest = rows.last().unwrap().clone();

    // History: the N rows before the latest (most recent first)
    let history = if history_count > 0 && rows.len() > 1 {
        let start = if rows.len() - 1 > history_count {
            rows.len() - 1 - history_count
        } else {
            0
        };
        rows[start..rows.len() - 1].iter().rev().cloned().collect()
    } else {
        Vec::new()
    };

    Ok(VolSurfaceIndex {
        symbol: symbol.to_string(),
        latest,
        history,
    })
}

pub async fn fetch_volsurface(req: VolSurfaceRequest) -> Result<VolSurfaceResponse> {
    let client = &*crate::finance::shared_client::GENERAL;
    let symbols: Vec<String> = req
        .symbols
        .unwrap_or_else(|| DEFAULT_SYMBOLS.iter().map(|s| s.to_string()).collect());
    let history_count = req.history.unwrap_or(0);

    let mut indices: Vec<VolSurfaceIndex> = stream::iter(symbols.iter().enumerate())
        .map(|(i, sym)| {
            let sym = sym.clone();
            async move {
                // 50ms delay between batches of 3 to be polite
                if i > 0 && i % 3 == 0 {
                    tokio::time::sleep(StdDuration::from_millis(50)).await;
                }
                match fetch_one(client, &sym, history_count).await {
                    Ok(idx) => Some(idx),
                    Err(e) => {
                        warn!("volsurface: skipping {sym}: {e}");
                        None
                    }
                }
            }
        })
        .buffer_unordered(3)
        .filter_map(|x| async { x })
        .collect()
        .await;

    indices.sort_by_key(|idx| {
        symbols
            .iter()
            .position(|symbol| symbol == &idx.symbol)
            .unwrap_or(usize::MAX)
    });

    let count = indices.len();
    Ok(VolSurfaceResponse {
        generated_at: Utc::now(),
        indices,
        count,
        note: Some(
            "Returns CBOE volatility indices and term-structure data, not a per-underlying strike/expiry implied-vol surface."
                .to_string(),
        ),
    })
}
