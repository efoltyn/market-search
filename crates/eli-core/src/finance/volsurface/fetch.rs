use eli_finance_types::{
    VolSurfaceDataPoint, VolSurfaceIndex, VolSurfaceRequest, VolSurfaceResponse,
};
use futures::stream::{self, StreamExt};

const CBOE_BASE: &str = "https://cdn.cboe.com/api/global/us_indices/daily_prices/";
const DEFAULT_SYMBOLS: &[&str] = &[
    "VIX", "VIX9D", "VIX3M", "VIX6M", "VIX1Y", "VVIX", "OVX", "GVZ", "SKEW",
];

/// Parse a single CSV row into a (VolSurfaceDataPoint, is_point) tuple.
/// Handles two formats:
///   5-col: MM/DD/YYYY,open,high,low,close  (VIX, VIX9D, VIX3M, etc.) — is_point=false
///   2-col: MM/DD/YYYY,value                 (VVIX, OVX, GVZ, SKEW)   — is_point=true
fn parse_csv_row(line: &str) -> Option<(VolSurfaceDataPoint, bool)> {
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
        Some((
            VolSurfaceDataPoint { date, open, high, low, close, kind: None },
            false,
        ))
    } else {
        // 2-column single-value format (VVIX, OVX, GVZ, SKEW).
        // OHLC fields are filled with the same value; we flag the row with
        // kind:"point" so downstream code knows there is no wick to render.
        let val = parts[1].parse::<f64>().ok()?;
        Some((
            VolSurfaceDataPoint {
                date,
                open: val,
                high: val,
                low: val,
                close: val,
                kind: Some("point".to_string()),
            },
            true,
        ))
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

    // Parse CSV: skip header, skip empty lines.
    // Each row carries a flag indicating whether CBOE shipped 2-col point data
    // (VVIX/OVX/GVZ/SKEW) versus full OHLC (VIX/VIX9D/VIX3M/etc.).
    let parsed: Vec<(VolSurfaceDataPoint, bool)> = body
        .lines()
        .skip(1) // header row
        .filter(|line| !line.trim().is_empty())
        .filter_map(parse_csv_row)
        .collect();

    if parsed.is_empty() {
        return Err(Error::Provider(format!(
            "CBOE returned no valid data rows for {symbol}"
        )));
    }

    // If any row was point-format, the symbol is a point series.
    let is_point = parsed.iter().any(|(_, p)| *p);
    let rows: Vec<VolSurfaceDataPoint> = parsed.into_iter().map(|(p, _)| p).collect();

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
        kind: if is_point { Some("point".to_string()) } else { None },
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
        // The MCP tool description already covers scope; per-response advisory removed.
        note: None,
    })
}
