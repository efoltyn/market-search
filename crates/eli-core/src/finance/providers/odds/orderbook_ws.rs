async fn fetch_polymarket_books_ws(
    token_ids: &[String],
    timeout_ms: u64,
) -> Result<std::collections::HashMap<String, PolyBookSnapshot>> {
    use tokio_tungstenite::tungstenite::Message;
    let mut out: std::collections::HashMap<String, PolyBookSnapshot> =
        std::collections::HashMap::new();
    if token_ids.is_empty() {
        return Ok(out);
    }

    let (mut ws, _) = connect_async("wss://ws-subscriptions-clob.polymarket.com/ws/market")
        .await
        .map_err(|e| Error::Provider(format!("polymarket ws connect failed: {e}")))?;

    let subscribe = serde_json::json!({
        "type": "market",
        "assets_ids": token_ids,
    });
    ws.send(Message::Text(subscribe.to_string()))
        .await
        .map_err(|e| Error::Provider(format!("polymarket ws subscribe failed: {e}")))?;

    let deadline = tokio::time::Instant::now() + TokioDuration::from_millis(timeout_ms.max(1));
    while out.len() < token_ids.len() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        let next = tokio::time::timeout(remaining, ws.next()).await;
        let Some(msg) = next.ok().and_then(|v| v.transpose().ok()).flatten() else {
            break;
        };
        if let Message::Text(text) = msg {
            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg: PolyBookMessage = match serde_json::from_value(parsed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg.event_type != "book" {
                continue;
            }
            let Some(asset_id) = msg.asset_id.clone() else {
                continue;
            };
            let bids = msg.bids.or(msg.buys).unwrap_or_default();
            let asks = msg.asks.or(msg.sells).unwrap_or_default();
            let best_bid = bids.first().map(|b| b.price.clone());
            let best_ask = asks.first().map(|a| a.price.clone());
            out.insert(
                asset_id,
                PolyBookSnapshot {
                    best_bid,
                    best_ask,
                    timestamp: msg.timestamp.clone(),
                },
            );
        }
    }

    let _ = ws.close(None).await;
    Ok(out)
}

/// Public, depth-aware orderbook level for a Polymarket outcome (single side).
#[derive(Clone, Debug, serde::Serialize)]
pub struct PolymarketBookLevel {
    pub price: String,
    pub size: String,
}

/// Public Polymarket orderbook snapshot for one CLOB asset (one outcome of a market).
/// Bids descend, asks ascend; both arrays are truncated to the caller's `depth`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PolymarketOrderbook {
    pub asset_id: String,
    pub timestamp: Option<String>,
    pub bids: Vec<PolymarketBookLevel>,
    pub asks: Vec<PolymarketBookLevel>,
}

/// Batch-fetch Polymarket orderbooks for an arbitrary set of CLOB token (asset) ids.
///
/// Uses the public REST `/books` endpoint (no auth required) instead of the
/// WebSocket snapshot path, because REST returns full multi-level depth in a
/// single round-trip and never blocks waiting for snapshots that may never
/// arrive when a market is dormant.
///
/// `depth` truncates each side independently. `0` is clamped to `1`.
pub async fn fetch_polymarket_orderbooks(
    token_ids: &[String],
    depth: usize,
) -> Result<std::collections::HashMap<String, PolymarketOrderbook>> {
    let mut out: std::collections::HashMap<String, PolymarketOrderbook> =
        std::collections::HashMap::new();
    if token_ids.is_empty() {
        return Ok(out);
    }
    let depth = depth.max(1);

    #[derive(serde::Deserialize)]
    struct BookResp {
        #[serde(default)]
        asset_id: Option<String>,
        #[serde(default)]
        timestamp: Option<String>,
        #[serde(default)]
        bids: Vec<PolyBookLevel>,
        #[serde(default)]
        asks: Vec<PolyBookLevel>,
    }

    let body: Vec<serde_json::Value> = token_ids
        .iter()
        .map(|t| serde_json::json!({ "token_id": t }))
        .collect();

    let client = &*crate::finance::shared_client::GENERAL;
    let resp = client
        .post("https://clob.polymarket.com/books")
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("polymarket books fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "polymarket books fetch failed: http {}",
            resp.status()
        )));
    }
    let books: Vec<BookResp> = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("polymarket books parse failed: {e}")))?;

    // REST `/books` returns sorted ladders (bids high→low, asks low→high). Truncate
    // each side to `depth` so the per-outcome payload size scales with the user's
    // request, not the full book.
    for book in books {
        let Some(asset_id) = book.asset_id else { continue };
        let bids = book
            .bids
            .into_iter()
            .rev() // REST returns bids low→high; reverse so depth=N picks the best N.
            .take(depth)
            .map(|lvl| PolymarketBookLevel {
                price: lvl.price,
                size: lvl.size,
            })
            .collect();
        let asks = book
            .asks
            .into_iter()
            .rev() // REST returns asks high→low; reverse so depth=N picks the best N.
            .take(depth)
            .map(|lvl| PolymarketBookLevel {
                price: lvl.price,
                size: lvl.size,
            })
            .collect();
        out.insert(
            asset_id.clone(),
            PolymarketOrderbook {
                asset_id,
                timestamp: book.timestamp,
                bids,
                asks,
            },
        );
    }

    Ok(out)
}
