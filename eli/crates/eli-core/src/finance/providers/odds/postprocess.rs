fn postprocess_odds_response(mut resp: OddsResponse, req: &OddsRequest) -> OddsResponse {
    let search = req
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(search_raw) = search else {
        return resp;
    };
    let terms = search_terms(search_raw);
    if terms.is_empty() {
        return resp;
    }

    if let Some(events) = resp.available_events.as_mut() {
        events.sort_by(|a, b| {
            score_listed_event(b, &terms)
                .cmp(&score_listed_event(a, &terms))
                .then_with(|| a.title.cmp(&b.title))
        });
    }

    if let Some(markets) = resp.available_markets.as_mut() {
        markets.sort_by(|a, b| {
            score_listed_market(b, &terms)
                .cmp(&score_listed_market(a, &terms))
                .then_with(|| a.title.cmp(&b.title))
        });
    }

    if !resp.markets.is_empty() {
        resp.markets.sort_by(|a, b| {
            score_market(b, &terms)
                .cmp(&score_market(a, &terms))
                .then_with(|| a.title.cmp(&b.title))
        });
    }

    if resp.available_markets.is_some() {
        resp.analytics = resp
            .available_markets
            .as_ref()
            .and_then(|m| build_odds_analytics_from_listed(m));
    } else if !resp.markets.is_empty() {
        resp.analytics = build_odds_analytics(&resp.markets);
    }

    resp
}
