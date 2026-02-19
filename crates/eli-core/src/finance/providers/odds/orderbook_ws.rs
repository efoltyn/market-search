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

