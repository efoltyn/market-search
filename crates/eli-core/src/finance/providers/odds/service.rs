pub async fn fetch_odds(req: OddsRequest) -> Result<OddsResponse> {
    let mut provider = req
        .provider
        .as_deref()
        .unwrap_or("kalshi")
        .trim()
        .to_ascii_lowercase();

    if req.list_tags {
        if provider == "kalshi" {
            return Err(Error::InvalidInput(
                "list_tags is only supported for polymarket (use --provider polymarket or auto)"
                    .to_string(),
            ));
        }
        if provider == "auto" {
            provider = "polymarket".to_string();
        }
    }

    if req.disable_kalshi {
        if req.list_series {
            return Err(Error::InvalidInput(
                "list_series requires kalshi, but kalshi is disabled".to_string(),
            ));
        }
        if provider == "kalshi" || provider == "auto" {
            provider = "polymarket".to_string();
        }
    }

    if req.list_series {
        if provider == "polymarket" {
            return Err(Error::InvalidInput(
                "list_series is only supported for kalshi (use --provider kalshi or omit --provider)".to_string(),
            ));
        }
        let mut resp = fetch_odds_kalshi(req.clone()).await?;
        resp.sources = Some(vec![OddsSourceInfo {
            source: "kalshi".to_string(),
            base_url: KALSHI_BASE_URL.to_string(),
            ok: true,
            error: None,
        }]);
        return Ok(postprocess_odds_response(resp, &req));
    }

    if !req.list_events
        && !req.list_markets
        && !req.list_tags
        && req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
        && req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(Error::InvalidInput(
            "use --list-events, --list-markets, or provide series/event/market ticker".to_string(),
        ));
    }

    if req.include_orderbook && req.market_ticker.as_deref().unwrap_or("").trim().is_empty() {
        return Err(Error::InvalidInput(
            "market_ticker is required when include_orderbook is true".to_string(),
        ));
    }

    if provider == "polymarket" {
        let mut poly = fetch_odds_polymarket(&req).await?;
        poly.sources = Some(vec![OddsSourceInfo {
            source: "polymarket".to_string(),
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            ok: true,
            error: None,
        }]);
        return Ok(postprocess_odds_response(poly, &req));
    }

    if provider == "auto" {
        let mut sources = Vec::new();
        let kalshi_result = fetch_odds_kalshi(req.clone()).await;
        match kalshi_result {
            Ok(mut kalshi) => {
                sources.push(OddsSourceInfo {
                    source: "kalshi".to_string(),
                    base_url: KALSHI_BASE_URL.to_string(),
                    ok: true,
                    error: None,
                });
                let has_events = kalshi
                    .available_events
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
                    || !kalshi.events.is_empty();
                let has_markets = kalshi
                    .available_markets
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
                    || !kalshi.markets.is_empty();
                let has_series = kalshi.series.is_some();

                let found = if req.list_events {
                    has_events
                } else if req.list_markets {
                    has_markets
                } else if req.market_ticker.as_deref().unwrap_or("").trim().is_empty()
                    && req.event_ticker.as_deref().unwrap_or("").trim().is_empty()
                    && req.series_ticker.as_deref().unwrap_or("").trim().is_empty()
                {
                    has_events || has_markets
                } else if !req.market_ticker.as_deref().unwrap_or("").trim().is_empty() {
                    has_markets || kalshi.orderbook.is_some()
                } else if !req.event_ticker.as_deref().unwrap_or("").trim().is_empty() {
                    has_events || has_markets
                } else {
                    has_series || has_markets
                };

                if found {
                    kalshi.sources = Some(sources);
                    return Ok(postprocess_odds_response(kalshi, &req));
                }

                let mut poly = fetch_odds_polymarket(&req).await?;
                sources.push(OddsSourceInfo {
                    source: "polymarket".to_string(),
                    base_url: POLYMARKET_GAMMA_URL.to_string(),
                    ok: true,
                    error: None,
                });
                poly.sources = Some(sources);
                return Ok(postprocess_odds_response(poly, &req));
            }
            Err(e) => {
                let msg = e.to_string();
                sources.push(OddsSourceInfo {
                    source: "kalshi".to_string(),
                    base_url: KALSHI_BASE_URL.to_string(),
                    ok: false,
                    error: Some(msg),
                });
            }
        }

        let mut poly = fetch_odds_polymarket(&req).await?;
        sources.push(OddsSourceInfo {
            source: "polymarket".to_string(),
            base_url: POLYMARKET_GAMMA_URL.to_string(),
            ok: true,
            error: None,
        });
        poly.sources = Some(sources);
        return Ok(postprocess_odds_response(poly, &req));
    }

    let mut kalshi = fetch_odds_kalshi(req.clone()).await?;
    kalshi.sources = Some(vec![OddsSourceInfo {
        source: "kalshi".to_string(),
        base_url: KALSHI_BASE_URL.to_string(),
        ok: true,
        error: None,
    }]);
    Ok(postprocess_odds_response(kalshi, &req))
}
