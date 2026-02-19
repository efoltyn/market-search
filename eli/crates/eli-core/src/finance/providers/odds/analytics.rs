async fn fetch_kalshi_tags_by_categories(
    client: &reqwest::Client,
) -> Result<HashMap<String, Vec<String>>> {
    #[derive(Deserialize)]
    struct TagsByCategoriesResp {
        #[serde(default)]
        tags_by_categories: HashMap<String, Option<Vec<String>>>,
    }

    let url = format!("{}/search/tags_by_categories", KALSHI_BASE_URL);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("kalshi tags_by_categories fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "kalshi tags_by_categories fetch failed: http {}",
            resp.status()
        )));
    }

    let body: TagsByCategoriesResp = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("kalshi tags_by_categories parse failed: {e}")))?;

    Ok(body
        .tags_by_categories
        .into_iter()
        .map(|(category, tags)| (category, tags.unwrap_or_default()))
        .collect())
}

fn derive_kalshi_categories_for_query(
    query: &str,
    tags_by_categories: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let query_lc = query.trim().to_ascii_lowercase();
    if query_lc.is_empty() {
        return Vec::new();
    }
    let terms = search_terms(&query_lc);

    let mut scored: Vec<(String, i32)> = tags_by_categories
        .iter()
        .filter_map(|(category, tags)| {
            let mut score = 0i32;
            if category.to_ascii_lowercase().contains(&query_lc) {
                score += 4;
            }
            if tags
                .iter()
                .any(|tag| tag.to_ascii_lowercase().contains(&query_lc))
            {
                score += 3;
            }
            if contains_any_term(category, &terms) {
                score += 2;
            }
            if tags.iter().any(|tag| contains_any_term(tag, &terms)) {
                score += 1;
            }
            (score > 0).then_some((category.clone(), score))
        })
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored
        .into_iter()
        .take(6)
        .map(|(category, _)| category)
        .collect()
}

pub(crate) fn probability_yes_from_outcomes(outcomes: &[String], prices: &[String]) -> Option<f64> {
    let mut idx = None;
    for (i, o) in outcomes.iter().enumerate() {
        if o.trim().eq_ignore_ascii_case("yes") {
            idx = Some(i);
            break;
        }
    }
    let i = idx?;
    prices.get(i).and_then(|p| parse_probability(p))
}

fn build_odds_analytics(markets: &[OddsMarket]) -> Option<OddsAnalytics> {
    if markets.is_empty() {
        return None;
    }

    let markets_total = markets.len();
    let mut open_markets = 0usize;
    let mut active_markets = 0usize;
    let mut initialized_markets = 0usize;
    let mut markets_with_volume = 0usize;
    let mut volume_sum: i64 = 0;
    let mut prob_count = 0usize;
    let mut prob_sum = 0.0f64;
    let mut spread_count = 0usize;
    let mut spread_sum = 0.0f64;

    for m in markets {
        if let Some(status) = m.status.as_deref() {
            let status = status.to_ascii_lowercase();
            if status == "open" {
                open_markets += 1;
            }
            if status == "active" {
                active_markets += 1;
            }
            if status == "initialized" {
                initialized_markets += 1;
            }
        }

        if let Some(v) = m.volume {
            if v > 0 {
                markets_with_volume += 1;
                volume_sum = volume_sum.saturating_add(v);
            }
        }

        if let Some(p) = m.probability_yes {
            prob_count += 1;
            prob_sum += p;
        }

        if let (Some(ask), Some(bid)) = (m.yes_ask, m.yes_bid) {
            if ask >= bid {
                spread_count += 1;
                spread_sum += (ask - bid) as f64;
            }
        }
    }

    Some(OddsAnalytics {
        markets_total,
        open_markets,
        active_markets,
        initialized_markets,
        markets_with_volume,
        total_volume: Some(volume_sum),
        average_probability_yes: (prob_count > 0).then_some(prob_sum / prob_count as f64),
        average_spread_cents: (spread_count > 0).then_some(spread_sum / spread_count as f64),
    })
}

pub(crate) fn build_odds_analytics_from_listed(
    markets: &[OddsListedMarket],
) -> Option<OddsAnalytics> {
    if markets.is_empty() {
        return None;
    }
    let converted: Vec<OddsMarket> = markets
        .iter()
        .map(|m| OddsMarket {
            ticker: m.ticker.clone(),
            title: m.title.clone(),
            event_ticker: m.event_ticker.clone(),
            status: m.status.clone(),
            yes_price: m.yes_price,
            yes_bid: None,
            yes_ask: None,
            volume: m.volume,
            source: m.source.clone(),
            market_id: m.market_id.clone(),
            event_id: m.event_id.clone(),
            slug: m.slug.clone(),
            outcomes: m.outcomes.clone(),
            outcome_prices: m.outcome_prices.clone(),
            clob_token_ids: m.clob_token_ids.clone(),
            probability_yes: m.probability_yes,
            outcome_best_bids: None,
            outcome_best_asks: None,
            orderbook_timestamp: None,
        })
        .collect();
    build_odds_analytics(&converted)
}

#[derive(Deserialize)]
struct PolyBookLevel {
    price: String,
    size: String,
}

#[derive(Deserialize)]
struct PolyBookMessage {
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    asset_id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    bids: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    asks: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    buys: Option<Vec<PolyBookLevel>>,
    #[serde(default)]
    sells: Option<Vec<PolyBookLevel>>,
}

struct PolyBookSnapshot {
    best_bid: Option<String>,
    best_ask: Option<String>,
    timestamp: Option<String>,
}

