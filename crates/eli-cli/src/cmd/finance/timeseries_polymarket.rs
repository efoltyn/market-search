// Polymarket price history for finance_timeseries.
//
// Two upstreams, chosen per window:
// - Data API v2 /v2/prices-history (the documented route) for RECENT
//   windows. It serves fixed native tiers with graded retention (minimums,
//   per the API docs): 1m >= 7 days, 5m >= 60 days, 30m >= 90 days. We read
//   the finest tier still retained and no coarser than the candle.
// - The legacy CLOB /prices-history for windows older than that. Its
//   explicit startTs/endTs form (<=15 days per request) still returns
//   ~1-minute observations a year back, while v2's "permanent" 3h/12h tiers
//   are patchy (3 points in 9 days of April 2026 on the Senate-control
//   market, where the CLOB has 12,891). Its interval=max form floors spacing
//   at ~10 minutes whatever fidelity asks, which is why a 5m request used to
//   come back at 16:30, 16:40, 16:50; we never use that form.
// If both fail for a window, v2's 3h tier is the last resort and a
// granularity_downgrade notice says the candles are coarser than asked.
//
// Candles are built from the observations: o/h/l/c = first/max/min/last
// observed price in the bucket, stamped at bucket START. Buckets with no
// observation are left out, never carried forward. A resolved market's v2
// series ends with the on-chain settlement point (1 won, 0 lost).

const POLYMARKET_PRICES_HISTORY_V2_URL: &str = "https://data-api.polymarket.com/v2/prices-history";
const POLYMARKET_CLOB_HISTORY_URL: &str = "https://clob.polymarket.com/prices-history";
/// (bucket seconds, guaranteed retention in days; None = permanent).
const POLY_HISTORY_TIERS: [(i64, Option<i64>); 5] = [
    (60, Some(7)),
    (300, Some(60)),
    (1800, Some(90)),
    (10_800, None),
    (43_200, None),
];
/// v2 caps explicit start/end windows at 15 days.
const POLY_HISTORY_MAX_SPAN_SECS: i64 = 15 * 86_400;
/// Native points fetched per series at most (keeps request count small).
const POLY_HISTORY_POINT_BUDGET: i64 = 30_000;
/// Aim for at most this many native points per candle; finer is wasted work.
const POLY_HISTORY_POINTS_PER_CANDLE: i64 = 50;
const POLY_HISTORY_MAX_PAGES: usize = 8;
/// Legacy CLOB fidelity values (minutes) we ask for, and its window cap.
const POLY_CLOB_FIDELITIES: [i64; 5] = [1, 5, 10, 30, 60];
const POLY_CLOB_MAX_SPAN_SECS: i64 = 15 * 86_400 - 60;
/// Upper bound on CLOB observations per series (~1 per fidelity minute).
const POLY_CLOB_POINT_BUDGET: i64 = 150_000;
/// Concurrent CLOB window requests per series.
const POLY_CLOB_CONCURRENCY: usize = 6;
/// Legs fetched for one POLYMARKET:EVENT: ticker (largest by volume).
const POLY_EVENT_EXPAND_MAX_LEGS: usize = 30;

fn poly_tier_label(secs: i64) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Pick the native tier for a window. Prefers tiers no coarser than the
/// requested candle, still retained at the window's start, inside the point
/// budget, and not absurdly fine for the candle size. Falls back to the
/// finest retained tier (coarser than requested) when nothing fits.
fn poly_history_tier(step_secs: i64, start_ts: i64, end_ts: i64, now_ts: i64) -> i64 {
    let span = (end_ts - start_ts).max(1);
    let age = (now_ts - start_ts).max(0);
    let feasible = |tier: i64, retention_days: Option<i64>| {
        retention_days.map_or(true, |d| age <= d * 86_400) && span / tier <= POLY_HISTORY_POINT_BUDGET
    };
    let fitting: Vec<i64> = POLY_HISTORY_TIERS
        .iter()
        .filter(|(tier, ret)| *tier <= step_secs && feasible(*tier, *ret))
        .map(|(tier, _)| *tier)
        .collect();
    if let Some(tier) = fitting
        .iter()
        .copied()
        .find(|tier| tier * POLY_HISTORY_POINTS_PER_CANDLE >= step_secs)
    {
        return tier;
    }
    if let Some(tier) = fitting.last() {
        return *tier;
    }
    POLY_HISTORY_TIERS
        .iter()
        .find(|(tier, ret)| feasible(*tier, *ret))
        .map(|(tier, _)| *tier)
        .unwrap_or(43_200)
}

/// Query windows for one series. Permanent tiers read the whole life in one
/// `interval=max` walk (filtered client-side); retained tiers walk the window
/// in <=15-day start/end slices (start inclusive, end exclusive).
fn poly_history_windows(tier: i64, start_ts: i64, end_ts: i64) -> Vec<Vec<(&'static str, String)>> {
    if tier >= 10_800 {
        return vec![vec![
            ("interval", "max".to_string()),
            ("bucket_seconds", tier.to_string()),
        ]];
    }
    let mut out = Vec::new();
    let mut s = start_ts;
    let end_exclusive = end_ts + 1;
    while s < end_exclusive {
        let e = (s + POLY_HISTORY_MAX_SPAN_SECS).min(end_exclusive);
        out.push(vec![
            ("start", s.to_string()),
            ("end", e.to_string()),
            ("bucket_seconds", tier.to_string()),
        ]);
        s = e;
    }
    out
}

/// Bucket observed prices into candles stamped at bucket START.
fn poly_points_to_candles(
    points: &[(i64, f64)],
    step_secs: i64,
    start_ts: i64,
    end_ts: i64,
    invert: bool,
) -> Vec<eli_core::finance::Candle> {
    let step = step_secs.max(1);
    let mut sorted: Vec<(i64, f64)> = points
        .iter()
        .copied()
        .filter(|(ts, p)| *ts >= start_ts && *ts <= end_ts && p.is_finite())
        .collect();
    sorted.sort_by_key(|(ts, _)| *ts);
    let mut buckets: BTreeMap<i64, (f64, f64, f64, f64)> = BTreeMap::new();
    for (ts, raw) in sorted {
        let p = if invert { (1.0 - raw).clamp(0.0, 1.0) } else { raw.clamp(0.0, 1.0) };
        let bucket = ts.div_euclid(step) * step;
        buckets
            .entry(bucket)
            .and_modify(|(_, h, l, c)| {
                *h = h.max(p);
                *l = l.min(p);
                *c = p;
            })
            .or_insert((p, p, p, p));
    }
    buckets
        .into_iter()
        .filter_map(|(bucket, (o, h, l, c))| {
            Some(eli_core::finance::Candle {
                t: chrono::DateTime::<chrono::Utc>::from_timestamp(bucket, 0)?,
                o,
                h,
                l,
                c,
                v: None,
                kind: None,
            })
        })
        .collect()
}

fn poly_parse_v2_points(body: &serde_json::Value) -> Vec<(i64, f64)> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let ts = r.get("timestamp").and_then(parse_json_number_i64)?;
                    let p = r.get("price").and_then(parse_json_number_f64)?;
                    Some((ts, p))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn poly_history_get(
    client: &reqwest::Client,
    url: &str,
    params: &[(&str, String)],
) -> Result<serde_json::Value> {
    let mut attempt = 0;
    loop {
        let resp = client
            .get(url)
            .query(params)
            .send()
            .await
            .context("request polymarket price history")?;
        let status = resp.status();
        if status.is_success() {
            return resp.json().await.context("parse polymarket price history");
        }
        if (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) && attempt < 2 {
            let wait = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1)
                .min(5);
            attempt += 1;
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            continue;
        }
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("http {status}: {}", body.chars().take(300).collect::<String>());
    }
}

/// All native points for one token over the window at `tier`.
async fn poly_fetch_v2_points(
    client: &reqwest::Client,
    token_id: &str,
    tier: i64,
    start_ts: i64,
    end_ts: i64,
) -> Result<Vec<(i64, f64)>> {
    let mut points = Vec::new();
    for window in poly_history_windows(tier, start_ts, end_ts) {
        let mut cursor: Option<String> = None;
        for _ in 0..POLY_HISTORY_MAX_PAGES {
            let mut params = window.clone();
            params.push(("token_id", token_id.to_string()));
            if let Some(c) = cursor.as_ref() {
                params.push(("cursor", c.clone()));
            }
            let body = poly_history_get(client, POLYMARKET_PRICES_HISTORY_V2_URL, &params).await?;
            points.extend(poly_parse_v2_points(&body));
            cursor = body
                .get("pagination")
                .and_then(|p| p.get("next_cursor"))
                .and_then(|c| c.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok(points)
}

/// CLOB fidelity (minutes) for a candle size and window: about 50
/// observations per candle, but coarse enough to stay inside the point budget.
fn poly_clob_fidelity(step_secs: i64, start_ts: i64, end_ts: i64) -> i64 {
    let wanted = step_secs / 60 / POLY_HISTORY_POINTS_PER_CANDLE;
    let floor = ((end_ts - start_ts).max(1) / 60) / POLY_CLOB_POINT_BUDGET;
    let fine = POLY_CLOB_FIDELITIES
        .iter()
        .copied()
        .filter(|f| *f <= wanted.max(1))
        .last()
        .unwrap_or(1);
    let budget = POLY_CLOB_FIDELITIES
        .iter()
        .copied()
        .find(|f| *f >= floor)
        .unwrap_or(60);
    fine.max(budget)
}

/// <=15-day [start, end] slices for the CLOB's explicit-window form.
fn poly_clob_windows(start_ts: i64, end_ts: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut s = start_ts;
    while s < end_ts {
        let e = (s + POLY_CLOB_MAX_SPAN_SECS).min(end_ts);
        out.push((s, e));
        s = e;
    }
    out
}

/// Legacy CLOB history over the whole window (explicit start/end slices).
async fn poly_fetch_clob_points(
    client: &reqwest::Client,
    token_id: &str,
    fidelity_min: i64,
    start_ts: i64,
    end_ts: i64,
) -> Result<Vec<(i64, f64)>> {
    let windows = poly_clob_windows(start_ts, end_ts);
    let results: Vec<Result<Vec<(i64, f64)>>> = futures::stream::iter(windows)
        .map(|(s, e)| async move {
            let params = vec![
                ("market", token_id.to_string()),
                ("startTs", s.to_string()),
                ("endTs", e.to_string()),
                ("fidelity", fidelity_min.to_string()),
            ];
            let body = poly_history_get(client, POLYMARKET_CLOB_HISTORY_URL, &params).await?;
            Ok(body
                .get("history")
                .and_then(|h| h.as_array())
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| {
                            Some((
                                r.get("t").and_then(parse_json_number_i64)?,
                                r.get("p").and_then(parse_json_number_f64)?,
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default())
        })
        .buffer_unordered(POLY_CLOB_CONCURRENCY)
        .collect()
        .await;
    let mut points = Vec::new();
    for r in results {
        points.extend(r?);
    }
    Ok(points)
}

fn poly_history_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("init polymarket history client")
}

/// Series plus informational notices (granularity downgrade, source fallback).
async fn fetch_polymarket_series_with_notices(
    req: &PredictionMarketRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<(eli_core::finance::TickerSeries, Vec<eli_core::finance::TimeseriesError>)> {
    if req.market.starts_with("EVENT:") {
        // Expansion failed upstream (bad id/slug); say why.
        let client = poly_history_client()?;
        poly_expand_event(&client, &req.market["EVENT:".len()..]).await?;
        anyhow::bail!("polymarket event {} has no tradable legs", &req.market["EVENT:".len()..]);
    }
    let client = poly_history_client()?;
    let market_meta = fetch_polymarket_market_meta(&client, &req.market).await?;
    let market_id = market_meta
        .get("id")
        .map(json_value_to_string)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| req.market.clone());
    let outcomes = market_meta
        .get("outcomes")
        .map(parse_jsonish_string_list)
        .unwrap_or_default();
    let token_ids = market_meta
        .get("clobTokenIds")
        .map(parse_jsonish_string_list)
        .unwrap_or_default();
    if token_ids.is_empty() {
        anyhow::bail!("polymarket market {} has no clob token IDs", req.market);
    }
    let outcome_index = pick_polymarket_outcome_index(&outcomes, req.side, token_ids.len());
    let token_id = token_ids
        .get(outcome_index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("invalid polymarket outcome index for {}", req.market))?;
    // Each outcome has its own token and its own price path, so the NO side
    // reads the NO token as-is. Only a single-token market needs 1 - p.
    let invert = token_ids.len() < 2 && matches!(req.side, PredictionMarketSide::No);

    let step = granularity_seconds(granularity).max(60);
    let start_ts = start.timestamp();
    let end_ts = end.timestamp();
    let now_ts = chrono::Utc::now().timestamp();
    let label = prediction_market_request_label(req);
    let mut notices = Vec::new();
    let in_window = |pts: &[(i64, f64)]| pts.iter().any(|(t, _)| *t >= start_ts && *t <= end_ts);
    let tier = poly_history_tier(step, start_ts, end_ts, now_ts);
    let mut served_tier: Option<i64> = None;
    let mut points: Vec<(i64, f64)> = Vec::new();
    let mut first_error: Option<anyhow::Error> = None;
    // Recent window at full resolution: v2.
    if tier <= step && tier < 10_800 {
        match poly_fetch_v2_points(&client, &token_id, tier, start_ts, end_ts).await {
            Ok(p) if in_window(&p) => {
                points = p;
                served_tier = Some(tier);
            }
            Ok(_) => {}
            Err(e) => first_error = Some(e),
        }
    }
    // Older (or v2-empty) window: CLOB minute data.
    if served_tier.is_none() {
        let fidelity = poly_clob_fidelity(step, start_ts, end_ts);
        match poly_fetch_clob_points(&client, &token_id, fidelity, start_ts, end_ts).await {
            Ok(p) if in_window(&p) => {
                points = p;
                served_tier = Some(fidelity * 60);
            }
            Ok(_) => {}
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    // Last resort: v2's permanent 3h tier.
    if served_tier.is_none() {
        match poly_fetch_v2_points(&client, &token_id, 10_800, start_ts, end_ts).await {
            Ok(p) => {
                points = p;
                if step < 10_800 {
                    notices.push(eli_core::finance::TimeseriesError {
                        ticker: label.clone(),
                        stage: Some("granularity_downgrade".to_string()),
                        message: format!(
                            "Polymarket served this window only at 3h resolution; candles are 3h apart (or sparser), not {}",
                            poly_tier_label(step)
                        ),
                    });
                }
            }
            Err(e) => {
                let err = first_error.unwrap_or(e);
                return Err(err.context(format!("polymarket price history for {}", req.market)));
            }
        }
    }
    let candles = poly_points_to_candles(&points, step, start_ts, end_ts, invert);
    if candles.is_empty() {
        anyhow::bail!(
            "no polymarket price history points found for market {} in requested window — the market may not have traded then, or the id is stale; re-run finance_odds --search to get a live market id",
            req.market
        );
    }
    Ok((
        eli_core::finance::TickerSeries {
            ticker: format!("POLYMARKET:{market_id}:{}", req.side.as_str().to_ascii_uppercase()),
            candles,
            source: Some("polymarket".to_string()),
            upstream_id: Some(market_id.to_string()),
        },
        notices,
    ))
}

async fn fetch_polymarket_market_series(
    req: &PredictionMarketRequest,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    granularity: eli_core::finance::Span,
) -> Result<eli_core::finance::TickerSeries> {
    fetch_polymarket_series_with_notices(req, start, end, granularity)
        .await
        .map(|(series, _)| series)
}

async fn fetch_polymarket_market_meta(
    client: &reqwest::Client,
    market: &str,
) -> Result<serde_json::Value> {
    let direct_url = format!("{POLYMARKET_GAMMA_URL}/markets/{market}");
    let direct = client
        .get(&direct_url)
        .send()
        .await
        .context("request polymarket market metadata")?;

    if direct.status().is_success() {
        let value: serde_json::Value = direct
            .json()
            .await
            .context("parse polymarket market metadata")?;
        if value.is_object() {
            return Ok(value);
        }
    } else if direct.status() != reqwest::StatusCode::NOT_FOUND {
        let status = direct.status();
        let body = direct.text().await.unwrap_or_default();
        anyhow::bail!(
            "polymarket market metadata failed for {}: http {}: {}",
            market,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let fallback = client
        .get(format!("{POLYMARKET_GAMMA_URL}/markets"))
        .query(&[("slug", market), ("limit", "1")])
        .send()
        .await
        .context("request polymarket market metadata by slug")?;

    if !fallback.status().is_success() {
        let status = fallback.status();
        let body = fallback.text().await.unwrap_or_default();
        anyhow::bail!(
            "polymarket market metadata (slug) failed for {}: http {}: {}",
            market,
            status,
            body.chars().take(400).collect::<String>()
        );
    }

    let value: serde_json::Value = fallback
        .json()
        .await
        .context("parse polymarket market metadata by slug")?;

    let Some(first) = value.as_array().and_then(|items| items.first()).cloned() else {
        anyhow::bail!("polymarket market '{}' not found by id or slug", market);
    };

    Ok(first)
}

fn pick_polymarket_outcome_index(
    outcomes: &[String],
    side: PredictionMarketSide,
    token_count: usize,
) -> usize {
    if token_count <= 1 {
        return 0;
    }

    let target = match side {
        PredictionMarketSide::Yes => "yes",
        PredictionMarketSide::No => "no",
    };

    if let Some(idx) = outcomes
        .iter()
        .position(|o| o.trim().eq_ignore_ascii_case(target))
    {
        return idx.min(token_count - 1);
    }

    match side {
        PredictionMarketSide::Yes => 0,
        PredictionMarketSide::No => 1.min(token_count - 1),
    }
}

/// Event legs worth a series: (market id, label, lifetime volume), placeholder
/// slots (never priced) dropped, largest first, capped.
fn poly_event_legs(event: &serde_json::Value) -> (String, Vec<(String, String, f64)>) {
    let title = event
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let mut legs: Vec<(String, String, f64)> = event
        .get("markets")
        .and_then(|v| v.as_array())
        .map(|markets| {
            markets
                .iter()
                .filter(|m| {
                    !parse_jsonish_string_list(m.get("outcomePrices").unwrap_or(&serde_json::Value::Null))
                        .is_empty()
                        && !parse_jsonish_string_list(m.get("clobTokenIds").unwrap_or(&serde_json::Value::Null))
                            .is_empty()
                })
                .filter_map(|m| {
                    let id = m.get("id").map(json_value_to_string)?;
                    let label = m
                        .get("groupItemTitle")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| m.get("question").and_then(|v| v.as_str()))
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    let vol = m
                        .get("volumeNum")
                        .and_then(parse_json_number_f64)
                        .or_else(|| m.get("volume").and_then(parse_json_number_f64))
                        .unwrap_or(0.0);
                    Some((id, label, vol))
                })
                .collect()
        })
        .unwrap_or_default();
    legs.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    legs.truncate(POLY_EVENT_EXPAND_MAX_LEGS);
    (title, legs)
}

/// Resolve an event (numeric id or slug) to its legs.
async fn poly_expand_event(
    client: &reqwest::Client,
    key: &str,
) -> Result<(String, Vec<(String, String, f64)>)> {
    let key = key.trim();
    let url = if !key.is_empty() && key.chars().all(|c| c.is_ascii_digit()) {
        format!("{POLYMARKET_GAMMA_URL}/events/{key}")
    } else {
        format!("{POLYMARKET_GAMMA_URL}/events/slug/{key}")
    };
    let event = poly_history_get(client, &url, &[])
        .await
        .with_context(|| format!("polymarket event '{key}' lookup (pass the numeric event id or slug from finance_odds events[].event_ticker)"))?;
    let (title, legs) = poly_event_legs(&event);
    if legs.is_empty() {
        anyhow::bail!("polymarket event '{key}' has no priced legs");
    }
    Ok((title, legs))
}

/// Replace every POLYMARKET:EVENT:<id|slug> request with its legs (same
/// side), returning a legend notice per event so each leg id has a name.
/// A failed lookup keeps the EVENT request, whose fetch then reports why.
async fn poly_expand_event_requests(
    requests: &[PredictionMarketRequest],
) -> (Vec<PredictionMarketRequest>, Vec<eli_core::finance::TimeseriesError>) {
    let mut out = Vec::new();
    let mut notices = Vec::new();
    let Ok(client) = poly_history_client() else {
        return (requests.to_vec(), notices);
    };
    for req in requests {
        let key = match (req.provider, req.market.strip_prefix("EVENT:")) {
            (PredictionMarketProvider::Polymarket, Some(key)) => key,
            _ => {
                push_prediction_market_request(&mut out, req.clone());
                continue;
            }
        };
        match poly_expand_event(&client, key).await {
            Ok((title, legs)) => {
                let legend = legs
                    .iter()
                    .map(|(id, label, _)| format!("{id}={label}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                notices.push(eli_core::finance::TimeseriesError {
                    ticker: format!("POLYMARKET:EVENT:{key}"),
                    stage: Some("event_legs".to_string()),
                    message: format!("{title}: {legend}"),
                });
                for (id, _, _) in legs {
                    push_prediction_market_request(
                        &mut out,
                        PredictionMarketRequest {
                            provider: PredictionMarketProvider::Polymarket,
                            market: id,
                            side: req.side,
                        },
                    );
                }
            }
            Err(_) => push_prediction_market_request(&mut out, req.clone()),
        }
    }
    (out, notices)
}

#[cfg(test)]
mod timeseries_polymarket_tests {
    use super::*;

    const NOW: i64 = 1_793_726_800; // 2026-11-03T17:26:40Z
    const DAY: i64 = 86_400;

    #[test]
    fn tier_prefers_fine_retained_data_no_coarser_than_the_candle() {
        // 5m candles over the last hour: true 1m observations.
        assert_eq!(poly_history_tier(300, NOW - 3600, NOW, NOW), 60);
        // 1h candles over 14 days: 1m is past retention, 5m is not.
        assert_eq!(poly_history_tier(3600, NOW - 14 * DAY, NOW, NOW), 300);
        // 1d candles over 30 days: 30m (48 per candle), not 5m.
        assert_eq!(poly_history_tier(DAY, NOW - 30 * DAY, NOW, NOW), 1800);
        // 1d candles over a year: only the permanent tiers reach back.
        assert_eq!(poly_history_tier(DAY, NOW - 365 * DAY, NOW, NOW), 10_800);
    }

    #[test]
    fn tier_downgrades_honestly_when_the_window_is_too_old() {
        // 1h candles over a window 200 days back: 3h is the finest retained.
        let tier = poly_history_tier(3600, NOW - 200 * DAY, NOW - 186 * DAY, NOW);
        assert_eq!(tier, 10_800);
        assert!(tier > 3600);
        // 1m candles over 30 days: 5m is the finest retained.
        assert_eq!(poly_history_tier(60, NOW - 30 * DAY, NOW, NOW), 300);
    }

    #[test]
    fn windows_slice_retained_tiers_and_read_permanent_tiers_whole() {
        let w = poly_history_windows(300, NOW - 40 * DAY, NOW);
        assert_eq!(w.len(), 3);
        for win in &w {
            let s: i64 = win[0].1.parse().unwrap();
            let e: i64 = win[1].1.parse().unwrap();
            assert!(e - s <= POLY_HISTORY_MAX_SPAN_SECS);
        }
        assert_eq!(w.last().unwrap()[1].1, (NOW + 1).to_string());
        let w = poly_history_windows(10_800, NOW - 400 * DAY, NOW);
        assert_eq!(w, vec![vec![("interval", "max".to_string()), ("bucket_seconds", "10800".to_string())]]);
    }

    #[test]
    fn clob_fidelity_targets_fifty_points_per_candle_within_budget() {
        // 1h candles: 1-minute observations.
        assert_eq!(poly_clob_fidelity(3600, NOW - 9 * DAY, NOW), 1);
        // 1d candles over a year: 10-minute observations.
        assert_eq!(poly_clob_fidelity(DAY, NOW - 365 * DAY, NOW), 10);
        // 1h candles over a year: the point budget forces 5-minute.
        assert_eq!(poly_clob_fidelity(3600, NOW - 365 * DAY, NOW), 5);
        let w = poly_clob_windows(NOW - 40 * DAY, NOW);
        assert_eq!(w.len(), 3);
        assert!(w.iter().all(|(s, e)| e - s <= POLY_CLOB_MAX_SPAN_SECS));
        assert_eq!(w.first().unwrap().0, NOW - 40 * DAY);
        assert_eq!(w.last().unwrap().1, NOW);
    }

    #[test]
    fn candles_are_ohlc_of_observations_at_bucket_start() {
        let t0 = 1_793_721_600; // 16:00Z, an hour boundary
        let pts = vec![
            (t0 + 60, 0.70),
            (t0 + 120, 0.74),
            (t0 + 180, 0.69),
            (t0 + 3540, 0.72),
            // next hour, one point; a gap hour after it stays empty
            (t0 + 3600 + 300, 0.75),
            (t0 + 3 * 3600, 0.80),
        ];
        let c = poly_points_to_candles(&pts, 3600, t0, t0 + 4 * 3600, false);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].t.timestamp(), t0);
        assert_eq!((c[0].o, c[0].h, c[0].l, c[0].c), (0.70, 0.74, 0.69, 0.72));
        assert_eq!(c[1].t.timestamp(), t0 + 3600);
        assert_eq!(c[2].t.timestamp(), t0 + 3 * 3600);
        let inv = poly_points_to_candles(&pts[..1], 3600, t0, t0 + 3600, true);
        assert!((inv[0].c - 0.30).abs() < 1e-12);
        // Points outside the window are dropped.
        assert!(poly_points_to_candles(&pts, 3600, t0 + 5 * 3600, t0 + 6 * 3600, false).is_empty());
    }

    #[test]
    fn v2_points_parse_numbers_and_skip_junk() {
        let body = serde_json::json!({"data": [
            {"timestamp": 1788354000, "price": 0.255, "resolution_seconds": 3600},
            {"timestamp": "1788357600", "price": "0.26", "resolution_seconds": 0},
            {"timestamp": null, "price": 0.3}
        ], "pagination": {"has_more": false, "next_cursor": null}});
        assert_eq!(poly_parse_v2_points(&body), vec![(1788354000, 0.255), (1788357600, 0.26)]);
    }

    #[test]
    fn event_legs_drop_placeholders_and_sort_by_volume() {
        let ev = serde_json::json!({"title": "Which party will win the Senate in 2026?", "markets": [
            {"id": "562794", "groupItemTitle": "Republican Party", "outcomePrices": "[\"0.255\",\"0.745\"]", "clobTokenIds": "[\"1\",\"2\"]", "volumeNum": 9.7e6},
            {"id": "562795", "groupItemTitle": "Party A", "outcomePrices": null, "clobTokenIds": "[\"3\",\"4\"]", "volumeNum": 0},
            {"id": "562793", "groupItemTitle": "Democratic Party", "outcomePrices": "[\"0.745\",\"0.255\"]", "clobTokenIds": "[\"5\",\"6\"]", "volumeNum": 1.03e7}
        ]});
        let (title, legs) = poly_event_legs(&ev);
        assert_eq!(title, "Which party will win the Senate in 2026?");
        let ids: Vec<&str> = legs.iter().map(|l| l.0.as_str()).collect();
        assert_eq!(ids, ["562793", "562794"]);
        assert_eq!(legs[0].1, "Democratic Party");
    }

    #[test]
    fn event_ticker_parses_as_polymarket() {
        let req = parse_prediction_market_ticker("POLYMARKET:EVENT:fed-decision-in-december").unwrap();
        assert!(matches!(req.provider, PredictionMarketProvider::Polymarket));
        assert_eq!(req.market, "EVENT:fed-decision-in-december");
        let req = parse_prediction_market_ticker("POLYMARKET:EVENT:770450:NO").unwrap();
        assert_eq!(req.market, "EVENT:770450");
        assert!(matches!(req.side, PredictionMarketSide::No));
        let req = parse_prediction_market_ticker("562793:NO").unwrap();
        assert_eq!(req.market, "562793");
        assert!(matches!(req.side, PredictionMarketSide::No));
    }
}
