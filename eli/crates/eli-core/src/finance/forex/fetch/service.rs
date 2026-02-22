use chrono::{Datelike, Timelike, Weekday};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UsdSide {
    Base,
    Quote,
    None,
}

#[derive(Clone, Debug)]
struct PairSpec {
    ticker: String,
    pair: String,
    base_currency: String,
    quote_currency: String,
    usd_side: UsdSide,
    non_usd_currency: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct EventWindowConfig {
    event_at: DateTime<Utc>,
    window: Span,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

impl EventWindowConfig {
    fn new(event_at: DateTime<Utc>, window: Span) -> Self {
        let dur = window.approx_duration();
        Self {
            event_at,
            window,
            start: event_at - dur,
            end: event_at + dur,
        }
    }
}

#[derive(Clone, Debug)]
struct PairEventWindowStats {
    pair: String,
    pre_usd_change_pct: f64,
    post_usd_change_pct: f64,
    shift_pct: f64,
}

#[derive(Clone, Debug)]
struct SessionHit {
    session: String,
    usd_impact_pct: f64,
}

#[derive(Clone, Debug)]
struct PairComparisonSnapshot {
    as_of: DateTime<Utc>,
    usd_change_by_pair: std::collections::HashMap<String, f64>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
struct ForexDeltaState {
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    snapshots: std::collections::HashMap<String, ForexDeltaSnapshot>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ForexDeltaSnapshot {
    as_of: DateTime<Utc>,
    #[serde(default)]
    captured_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    usd_change_by_pair: std::collections::HashMap<String, f64>,
}

const FOREX_COMPARE_MAX_POINTS: usize = 8;
const FOREX_DELTA_TOP_LIMIT: usize = 10;
const FOREX_DELTA_CHANGE_EPSILON: f64 = 0.0001;
const FOREX_DELTA_MAX_SNAPSHOTS: usize = 64;
const FOREX_DELTA_STATE_FILE: &str = "forex_delta_state.json";
const USD_BENCHMARK_SYMBOL: &str = "DTWEXBGS";

pub async fn fetch_forex(req: ForexRequest, cache_dir: &Path) -> Result<ForexResponse> {
    let now = Utc::now();
    let as_of = req.as_of.unwrap_or(now).min(now);
    let top = req.top.unwrap_or(12).max(1).min(200);
    let recent_points = req.recent_points.unwrap_or(0).min(128);
    let horizons = resolve_horizons(&req.horizons);

    let mut warnings = Vec::new();
    let event_window_cfg = resolve_event_window_config(&req, as_of, &mut warnings);
    if event_window_cfg.is_some() && !supports_session_attribution(req.granularity) {
        warnings.push(
            "session attribution is most reliable with minute/hour granularity (current granularity is coarser)"
                .to_string(),
        );
    }
    let selection = build_selection(&req, &mut warnings);
    let mut specs = build_pair_specs(&req, &selection, &mut warnings);
    if specs.is_empty() {
        return Err(Error::InvalidInput(
            "no valid forex pairs were resolved; try --pairs or valid --currencies/--countries/--groups"
                .to_string(),
        ));
    }

    if let Some(max_pairs) = req.max_pairs {
        let cap = max_pairs.max(1);
        specs.sort_by(|a, b| a.pair.cmp(&b.pair));
        specs.truncate(cap);
    }

    use futures::stream::{self, StreamExt};
    let rows: Vec<(
        Option<ForexPairPerformance>,
        Vec<ForexHitEvent>,
        Option<PairEventWindowStats>,
        Vec<SessionHit>,
        Option<String>,
    )> =
        stream::iter(specs.iter().cloned().map(|spec| {
            let ts_req = TimeseriesRequest {
                tickers: vec![spec.ticker.clone()],
                range: req.range,
                granularity: req.granularity,
                as_of: Some(as_of),
                provider: ProviderKind::Yahoo,
                max_points_per_ticker: None,
            };
            let horizons = horizons.clone();
            let event_window_cfg = event_window_cfg;
            async move {
                match fetch_timeseries(ts_req, cache_dir).await {
                    Ok(resp) => {
                        pair_performance_from_timeseries(
                            spec,
                            &resp,
                            &horizons,
                            recent_points,
                            event_window_cfg,
                        )
                    }
                    Err(e) => (
                        None,
                        Vec::new(),
                        None,
                        Vec::new(),
                        Some(format!("{}: {}", spec.ticker, e)),
                    ),
                }
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

    let mut pairs = Vec::new();
    let mut hits = Vec::new();
    let mut event_pair_stats = Vec::new();
    let mut session_hits = Vec::new();
    for (pair, mut pair_hits, pair_event, mut pair_sessions, warning) in rows {
        if let Some(pair) = pair {
            pairs.push(pair);
        }
        hits.append(&mut pair_hits);
        if let Some(stats) = pair_event {
            event_pair_stats.push(stats);
        }
        session_hits.append(&mut pair_sessions);
        if let Some(w) = warning {
            warnings.push(w);
        }
    }

    if pairs.is_empty() {
        return Err(Error::Provider(
            "forex fetch failed for all resolved pairs".to_string(),
        ));
    }

    pairs.sort_by(|a, b| {
        b.usd_change_pct
            .unwrap_or(f64::NEG_INFINITY)
            .partial_cmp(&a.usd_change_pct.unwrap_or(f64::NEG_INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut usd_changes: Vec<(String, f64)> = pairs
        .iter()
        .filter_map(|p| p.usd_change_pct.map(|v| (p.pair.clone(), v)))
        .collect();
    usd_changes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let usd_pairs_up = usd_changes.iter().filter(|(_, v)| *v > 0.0).count();
    let usd_pairs_down = usd_changes.iter().filter(|(_, v)| *v < 0.0).count();
    let usd_strength_score_pct = if usd_changes.is_empty() {
        None
    } else {
        Some(usd_changes.iter().map(|(_, v)| *v).sum::<f64>() / usd_changes.len() as f64)
    };

    let top_usd_gainers = usd_changes
        .iter()
        .take(3)
        .map(|(pair, v)| ForexPairMove {
            pair: pair.clone(),
            usd_change_pct: *v,
        })
        .collect::<Vec<_>>();
    let top_usd_losers = usd_changes
        .iter()
        .rev()
        .take(3)
        .map(|(pair, v)| ForexPairMove {
            pair: pair.clone(),
            usd_change_pct: *v,
        })
        .collect::<Vec<_>>();

    let mut daily_hits = collapse_hits_by_pair_date(&hits);
    let hot_dates = build_date_clusters(&daily_hits);

    daily_hits.sort_by(|a, b| {
        let b_mag = b.usd_impact_pct.unwrap_or(b.daily_change_pct).abs();
        let a_mag = a.usd_impact_pct.unwrap_or(a.daily_change_pct).abs();
        b_mag
            .partial_cmp(&a_mag)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    daily_hits.truncate(top);

    let strongest_usd_pair = usd_changes.first().map(|(pair, _)| pair.clone());
    let weakest_usd_pair = usd_changes.last().map(|(pair, _)| pair.clone());
    let summary = ForexSummary {
        usd_strength_score_pct,
        usd_pairs_up,
        usd_pairs_down,
        strongest_usd_pair,
        weakest_usd_pair,
        top_usd_gainers,
        top_usd_losers,
        hot_dates,
    };

    let event_window = build_event_window_summary(event_window_cfg, &event_pair_stats, &session_hits);
    if event_window_cfg.is_some() && event_window.is_none() {
        warnings.push(
            "event window analysis requested, but insufficient candles were available around event timestamp"
                .to_string(),
        );
    }

    let basket = if req.pairs.is_empty() {
        if selection.requested_groups.is_empty()
            && selection.requested_countries.is_empty()
            && selection.requested_currencies.is_empty()
        {
            if req.include_em {
                "usd_broad".to_string()
            } else {
                "usd_majors".to_string()
            }
        } else {
            "usd_filtered".to_string()
        }
    } else {
        "custom".to_string()
    };
    let as_of = pairs
        .iter()
        .map(|p| p.last_observation_at)
        .max()
        .unwrap_or(as_of);

    let mut resolved_currencies: Vec<String> = specs
        .iter()
        .filter_map(|s| s.non_usd_currency.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    resolved_currencies.sort();
    let mut resolved_pairs = pairs.iter().map(|p| p.pair.clone()).collect::<Vec<_>>();
    resolved_pairs.sort();

    let (comparisons, comparison_deltas, mut compare_warnings) = build_comparisons(
        &req,
        &specs,
        cache_dir,
        as_of,
        summary.usd_strength_score_pct,
        summary.usd_pairs_up,
        summary.usd_pairs_down,
    )
    .await;
    warnings.append(&mut compare_warnings);

    let delta_context =
        update_delta_context(cache_dir, &req, &resolved_pairs, &pairs, as_of, &mut warnings);

    let usd_benchmark = match fetch_usd_benchmark(req.range, as_of, &horizons, cache_dir).await {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("usd benchmark unavailable: {e}"));
            None
        }
    };

    let selection = ForexSelection {
        requested_groups: selection.requested_groups,
        requested_countries: selection.requested_countries,
        requested_currencies: selection.requested_currencies,
        resolved_currencies,
        resolved_pairs,
    };

    Ok(ForexResponse {
        generated_at: now,
        as_of,
        range: req.range,
        granularity: req.granularity,
        basket,
        pair_count: pairs.len(),
        selection,
        pairs,
        summary,
        comparisons,
        comparison_deltas,
        event_window,
        delta_context,
        usd_benchmark,
        biggest_daily_usd_moves: daily_hits,
        warnings,
    })
}

#[derive(Clone, Debug)]
struct SelectionDraft {
    requested_groups: Vec<String>,
    requested_countries: Vec<String>,
    requested_currencies: Vec<String>,
    resolved_currencies: Vec<String>,
}

fn build_selection(req: &ForexRequest, warnings: &mut Vec<String>) -> SelectionDraft {
    let requested_groups = normalize_symbols(&req.groups);
    let requested_countries = normalize_symbols(&req.countries);
    let requested_currencies = normalize_symbols(&req.currencies);

    let explicit_filters = !requested_groups.is_empty()
        || !requested_countries.is_empty()
        || !requested_currencies.is_empty();

    let mut currencies: std::collections::HashSet<String> = std::collections::HashSet::new();

    if !explicit_filters {
        for c in group_currencies("majors") {
            currencies.insert(c.to_string());
        }
        if req.include_em {
            for c in group_currencies("em") {
                currencies.insert(c.to_string());
            }
        }
    }

    for g in &requested_groups {
        let key = g.to_ascii_lowercase();
        let group = group_currencies(&key);
        if group.is_empty() {
            warnings.push(format!(
                "{g}: unknown group (use majors,g10,em,europe,americas,asia,commodity)"
            ));
            continue;
        }
        for c in group {
            currencies.insert(c.to_string());
        }
    }

    for cc in &requested_countries {
        if let Some(currency) = country_to_currency(cc) {
            currencies.insert(currency.to_string());
        } else {
            warnings.push(format!(
                "{cc}: unknown country code for forex mapping (try ISO2 like CA,JP,GB)"
            ));
        }
    }

    for cur in &requested_currencies {
        if cur.len() == 3 && cur.chars().all(|c| c.is_ascii_alphabetic()) {
            currencies.insert(cur.clone());
        } else {
            warnings.push(format!("{cur}: invalid currency code (expected 3 letters)"));
        }
    }

    currencies.remove("USD");
    let mut resolved_currencies = currencies.into_iter().collect::<Vec<_>>();
    resolved_currencies.sort();

    SelectionDraft {
        requested_groups,
        requested_countries,
        requested_currencies,
        resolved_currencies,
    }
}

fn build_pair_specs(
    req: &ForexRequest,
    selection: &SelectionDraft,
    warnings: &mut Vec<String>,
) -> Vec<PairSpec> {
    if !req.pairs.is_empty() {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for raw in &req.pairs {
            let normalized = raw.trim().to_ascii_uppercase();
            if normalized.is_empty() || !seen.insert(normalized.clone()) {
                continue;
            }
            match parse_pair_spec(&normalized) {
                Some(spec) => out.push(spec),
                None => warnings.push(format!(
                    "{normalized}: unsupported format (expected Yahoo FX ticker like EURUSD=X)"
                )),
            }
        }
        return out;
    }

    let mut out = Vec::new();
    for currency in &selection.resolved_currencies {
        let ticker = usd_ticker_for_currency(currency);
        match parse_pair_spec(&ticker) {
            Some(spec) => out.push(spec),
            None => warnings.push(format!("{ticker}: failed to parse generated USD FX pair")),
        }
    }
    out
}

fn pair_performance_from_timeseries(
    spec: PairSpec,
    resp: &TimeseriesResponse,
    horizons: &[Span],
    recent_points: usize,
    event_window_cfg: Option<EventWindowConfig>,
) -> (
    Option<ForexPairPerformance>,
    Vec<ForexHitEvent>,
    Option<PairEventWindowStats>,
    Vec<SessionHit>,
    Option<String>,
) {
    let mut hits = Vec::new();
    let mut session_hits = Vec::new();
    let Some(series) = resp.series.first() else {
        return (
            None,
            hits,
            None,
            session_hits,
            Some(format!("{}: no timeseries returned", spec.ticker)),
        );
    };
    if series.candles.len() < 2 {
        return (
            None,
            hits,
            None,
            session_hits,
            Some(format!("{}: insufficient candles", spec.ticker)),
        );
    }

    let first = match series.candles.first() {
        Some(v) => v,
        None => {
            return (
                None,
                hits,
                None,
                session_hits,
                Some(format!("{}: missing first candle", spec.ticker)),
            )
        }
    };
    let last = match series.candles.last() {
        Some(v) => v,
        None => {
            return (
                None,
                hits,
                None,
                session_hits,
                Some(format!("{}: missing last candle", spec.ticker)),
            )
        }
    };
    if first.c == 0.0 {
        return (
            None,
            hits,
            None,
            session_hits,
            Some(format!("{}: invalid start rate 0.0", spec.ticker)),
        );
    }

    let change_pct = ((last.c - first.c) / first.c) * 100.0;
    let usd_change_pct = match spec.usd_side {
        UsdSide::Base => Some(change_pct),
        UsdSide::Quote => Some(-change_pct),
        UsdSide::None => None,
    };

    let mut biggest_drop: Option<(f64, String)> = None;
    let mut biggest_rise: Option<(f64, String)> = None;
    for win in series.candles.windows(2) {
        let prev = &win[0];
        let cur = &win[1];
        if prev.c == 0.0 {
            continue;
        }
        let daily_change_pct = ((cur.c - prev.c) / prev.c) * 100.0;
        let date = cur.t.format("%Y-%m-%d").to_string();

        if biggest_drop
            .as_ref()
            .map(|(v, _)| daily_change_pct < *v)
            .unwrap_or(true)
        {
            biggest_drop = Some((daily_change_pct, date.clone()));
        }
        if biggest_rise
            .as_ref()
            .map(|(v, _)| daily_change_pct > *v)
            .unwrap_or(true)
        {
            biggest_rise = Some((daily_change_pct, date.clone()));
        }

        let usd_impact_pct = match spec.usd_side {
            UsdSide::Base => Some(daily_change_pct),
            UsdSide::Quote => Some(-daily_change_pct),
            UsdSide::None => None,
        };
        hits.push(ForexHitEvent {
            ticker: spec.ticker.clone(),
            pair: spec.pair.clone(),
            date,
            daily_change_pct,
            usd_impact_pct,
        });
    }

    let mut horizon_change_pct = std::collections::BTreeMap::new();
    for span in horizons {
        let key = span.to_string_compact();
        let target = last.t - span.approx_duration();
        let anchor = series.candles.iter().rev().find(|c| c.t <= target);
        if let Some(anchor) = anchor {
            if anchor.c != 0.0 {
                let raw_change = ((last.c - anchor.c) / anchor.c) * 100.0;
                let usd_change = match spec.usd_side {
                    UsdSide::Base => raw_change,
                    UsdSide::Quote => -raw_change,
                    UsdSide::None => raw_change,
                };
                horizon_change_pct.insert(key, usd_change);
            }
        }
    }

    let annualized_vol_pct = annualized_volatility_pct(&series.candles, resp.granularity);

    let recent_prices = if recent_points == 0 {
        Vec::new()
    } else {
        series
            .candles
            .iter()
            .rev()
            .take(recent_points)
            .rev()
            .map(|c| ForexPricePoint { t: c.t, c: c.c })
            .collect::<Vec<_>>()
    };

    let pair_event = analyze_event_window(&spec, &series.candles, event_window_cfg);
    if let Some((_, ref event_sessions)) = pair_event {
        session_hits.extend(event_sessions.iter().cloned());
    }

    let pair = ForexPairPerformance {
        ticker: spec.ticker,
        pair: spec.pair,
        base_currency: spec.base_currency,
        quote_currency: spec.quote_currency,
        first_observation_at: first.t,
        last_observation_at: last.t,
        observations: series.candles.len(),
        start_rate: first.c,
        end_rate: last.c,
        change_pct,
        usd_change_pct,
        horizon_change_pct,
        annualized_vol_pct,
        recent_prices,
        biggest_daily_drop_pct: biggest_drop.as_ref().map(|(v, _)| *v),
        biggest_daily_drop_date: biggest_drop.as_ref().map(|(_, d)| d.clone()),
        biggest_daily_rise_pct: biggest_rise.as_ref().map(|(v, _)| *v),
        biggest_daily_rise_date: biggest_rise.as_ref().map(|(_, d)| d.clone()),
    };
    (Some(pair), hits, pair_event.map(|(p, _)| p), session_hits, None)
}

fn annualized_volatility_pct(candles: &[Candle], granularity: Span) -> Option<f64> {
    if candles.len() < 3 {
        return None;
    }
    let mut returns = Vec::new();
    for win in candles.windows(2) {
        let prev = win[0].c;
        let cur = win[1].c;
        if prev == 0.0 {
            continue;
        }
        returns.push((cur - prev) / prev);
    }
    if returns.len() < 2 {
        return None;
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let var = returns
        .iter()
        .map(|r| {
            let d = r - mean;
            d * d
        })
        .sum::<f64>()
        / (returns.len() as f64 - 1.0);
    let period_days = (granularity.approx_duration().num_seconds().max(1) as f64) / 86_400.0;
    if period_days <= 0.0 {
        return None;
    }
    let periods_per_year = 365.0 / period_days;
    Some(var.sqrt() * periods_per_year.sqrt() * 100.0)
}

fn resolve_event_window_config(
    req: &ForexRequest,
    as_of: DateTime<Utc>,
    warnings: &mut Vec<String>,
) -> Option<EventWindowConfig> {
    match (req.event_at, req.event_window) {
        (Some(event_at_raw), Some(window)) => {
            let event_at = event_at_raw.min(as_of);
            if event_at != event_at_raw {
                warnings.push(
                    "event-at is after as-of; clamping event-at to available range end".to_string(),
                );
            }
            Some(EventWindowConfig::new(event_at, clamp_event_window(window)))
        }
        (Some(event_at_raw), None) => {
            let default_window = Span {
                n: 24,
                unit: SpanUnit::Hour,
            };
            warnings.push("event-window not set; defaulting to 24h".to_string());
            let event_at = event_at_raw.min(as_of);
            if event_at != event_at_raw {
                warnings.push(
                    "event-at is after as-of; clamping event-at to available range end".to_string(),
                );
            }
            Some(EventWindowConfig::new(event_at, default_window))
        }
        (None, Some(_)) => {
            warnings.push("event-window ignored because event-at is not set".to_string());
            None
        }
        (None, None) => None,
    }
}

fn clamp_event_window(window: Span) -> Span {
    // Keep windows useful for event studies and bounded for performance.
    let min = Span {
        n: 1,
        unit: SpanUnit::Hour,
    };
    let max = Span {
        n: 14,
        unit: SpanUnit::Day,
    };
    if window.approx_duration() < min.approx_duration() {
        min
    } else if window.approx_duration() > max.approx_duration() {
        max
    } else {
        window
    }
}

fn supports_session_attribution(granularity: Span) -> bool {
    matches!(granularity.unit, SpanUnit::Minute | SpanUnit::Hour)
}

fn usd_adjust(side: UsdSide, raw_change_pct: f64) -> f64 {
    match side {
        UsdSide::Base => raw_change_pct,
        UsdSide::Quote => -raw_change_pct,
        UsdSide::None => raw_change_pct,
    }
}

fn analyze_event_window(
    spec: &PairSpec,
    candles: &[Candle],
    cfg: Option<EventWindowConfig>,
) -> Option<(PairEventWindowStats, Vec<SessionHit>)> {
    let cfg = cfg?;
    if candles.len() < 3 {
        return None;
    }

    let anchor_idx = candles.iter().rposition(|c| c.t <= cfg.event_at)?;
    if anchor_idx == 0 {
        return None;
    }

    let pre_start_idx = candles
        .iter()
        .enumerate()
        .find(|(_, c)| c.t >= cfg.start && c.t <= candles[anchor_idx].t)
        .map(|(idx, _)| idx)?;
    if pre_start_idx >= anchor_idx {
        return None;
    }

    let post_end_idx = candles
        .iter()
        .rposition(|c| c.t <= cfg.end && c.t >= candles[anchor_idx].t)?;
    if post_end_idx <= anchor_idx {
        return None;
    }

    let pre_start = &candles[pre_start_idx];
    let anchor = &candles[anchor_idx];
    let post_end = &candles[post_end_idx];
    if pre_start.c == 0.0 || anchor.c == 0.0 {
        return None;
    }

    let pre_raw = ((anchor.c - pre_start.c) / pre_start.c) * 100.0;
    let post_raw = ((post_end.c - anchor.c) / anchor.c) * 100.0;
    let pre_usd = usd_adjust(spec.usd_side, pre_raw);
    let post_usd = usd_adjust(spec.usd_side, post_raw);
    let shift_pct = post_usd - pre_usd;

    let mut session_hits = Vec::new();
    for win in candles.windows(2) {
        let prev = &win[0];
        let cur = &win[1];
        if cur.t < cfg.start || cur.t > cfg.end || prev.c == 0.0 {
            continue;
        }
        let raw = ((cur.c - prev.c) / prev.c) * 100.0;
        session_hits.push(SessionHit {
            session: classify_session(cur.t).to_string(),
            usd_impact_pct: usd_adjust(spec.usd_side, raw),
        });
    }

    Some((
        PairEventWindowStats {
            pair: spec.pair.clone(),
            pre_usd_change_pct: pre_usd,
            post_usd_change_pct: post_usd,
            shift_pct,
        },
        session_hits,
    ))
}

fn classify_session(ts: DateTime<Utc>) -> &'static str {
    match ts.weekday() {
        Weekday::Sat | Weekday::Sun => "weekend_gap",
        _ => {
            let hour = ts.hour();
            if hour < 7 || hour >= 21 {
                "asia"
            } else if hour < 13 {
                "europe"
            } else {
                "us"
            }
        }
    }
}

fn build_event_window_summary(
    cfg: Option<EventWindowConfig>,
    pair_stats: &[PairEventWindowStats],
    session_hits: &[SessionHit],
) -> Option<ForexEventWindowSummary> {
    let cfg = cfg?;
    if pair_stats.is_empty() {
        return None;
    }

    let pre_vals = pair_stats
        .iter()
        .map(|s| s.pre_usd_change_pct)
        .collect::<Vec<_>>();
    let post_vals = pair_stats
        .iter()
        .map(|s| s.post_usd_change_pct)
        .collect::<Vec<_>>();

    let pre_usd_strength_score_pct = mean(&pre_vals);
    let post_usd_strength_score_pct = mean(&post_vals);
    let shift_usd_strength_pct = match (pre_usd_strength_score_pct, post_usd_strength_score_pct) {
        (Some(pre), Some(post)) => Some(post - pre),
        _ => None,
    };

    let pre_pairs_up = pre_vals.iter().filter(|v| **v > 0.0).count();
    let pre_pairs_down = pre_vals.iter().filter(|v| **v < 0.0).count();
    let post_pairs_up = post_vals.iter().filter(|v| **v > 0.0).count();
    let post_pairs_down = post_vals.iter().filter(|v| **v < 0.0).count();

    let mut top_pair_shifts = pair_stats
        .iter()
        .cloned()
        .collect::<Vec<PairEventWindowStats>>();
    top_pair_shifts.sort_by(|a, b| {
        b.shift_pct
            .abs()
            .partial_cmp(&a.shift_pct.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top_pair_shifts = top_pair_shifts
        .into_iter()
        .take(5)
        .map(|s| ForexEventPairShift {
            pair: s.pair,
            pre_usd_change_pct: s.pre_usd_change_pct,
            post_usd_change_pct: s.post_usd_change_pct,
            shift_pct: s.shift_pct,
        })
        .collect::<Vec<_>>();

    let mut by_session: std::collections::HashMap<String, (usize, f64, f64)> =
        std::collections::HashMap::new();
    for hit in session_hits {
        let entry = by_session
            .entry(hit.session.clone())
            .or_insert((0, 0.0, 0.0));
        entry.0 += 1;
        entry.1 += hit.usd_impact_pct;
        let mag = hit.usd_impact_pct.abs();
        if mag > entry.2 {
            entry.2 = mag;
        }
    }
    let mut session_attribution = by_session
        .into_iter()
        .map(|(session, (move_count, sum, max_abs))| ForexSessionAttribution {
            session,
            move_count,
            avg_usd_impact_pct: if move_count == 0 {
                0.0
            } else {
                sum / move_count as f64
            },
            max_abs_usd_impact_pct: max_abs,
        })
        .collect::<Vec<_>>();
    session_attribution.sort_by(|a, b| {
        b.move_count
            .cmp(&a.move_count)
            .then_with(|| {
                b.max_abs_usd_impact_pct
                    .partial_cmp(&a.max_abs_usd_impact_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.session.cmp(&b.session))
    });

    Some(ForexEventWindowSummary {
        event_at: cfg.event_at,
        window: cfg.window,
        pre_usd_strength_score_pct,
        post_usd_strength_score_pct,
        shift_usd_strength_pct,
        pre_pairs_up,
        pre_pairs_down,
        post_pairs_up,
        post_pairs_down,
        top_pair_shifts,
        session_attribution,
    })
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

async fn build_comparisons(
    req: &ForexRequest,
    specs: &[PairSpec],
    cache_dir: &Path,
    current_as_of: DateTime<Utc>,
    current_usd_strength_score_pct: Option<f64>,
    current_usd_pairs_up: usize,
    current_usd_pairs_down: usize,
) -> (Vec<ForexComparisonPoint>, Vec<ForexComparisonDelta>, Vec<String>) {
    if req.compare_as_of.is_empty() || specs.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut anchors = Vec::new();
    for raw in &req.compare_as_of {
        let clamped = (*raw).min(current_as_of);
        if clamped != *raw {
            warnings.push(format!(
                "compare-as-of {} is after as-of {}; clamping to as-of",
                raw.to_rfc3339(),
                current_as_of.to_rfc3339()
            ));
        }
        let key = clamped.to_rfc3339();
        if seen.insert(key) {
            anchors.push(clamped);
        }
    }

    if anchors.len() > FOREX_COMPARE_MAX_POINTS {
        warnings.push(format!(
            "compare-as-of anchors limited to {FOREX_COMPARE_MAX_POINTS}; truncating from {}",
            anchors.len()
        ));
        anchors.truncate(FOREX_COMPARE_MAX_POINTS);
    }

    let mut comparisons = Vec::new();
    let mut comparison_deltas = Vec::new();
    for anchor in anchors {
        let snapshot = build_comparison_snapshot(req, specs, cache_dir, anchor).await;
        if snapshot.usd_change_by_pair.is_empty() {
            warnings.push(format!(
                "compare-as-of {} returned no usable USD pair deltas",
                anchor.to_rfc3339()
            ));
            continue;
        }
        if snapshot.usd_change_by_pair.len() < specs.len() {
            warnings.push(format!(
                "compare-as-of {} partial coverage: {}/{} pairs",
                anchor.to_rfc3339(),
                snapshot.usd_change_by_pair.len(),
                specs.len()
            ));
        }
        let point = comparison_point_from_snapshot(&snapshot);
        let delta = ForexComparisonDelta {
            as_of: point.as_of,
            delta_usd_strength_pct: match (
                current_usd_strength_score_pct,
                point.usd_strength_score_pct,
            ) {
                (Some(cur), Some(hist)) => Some(cur - hist),
                _ => None,
            },
            delta_usd_pairs_up: current_usd_pairs_up as i64 - point.usd_pairs_up as i64,
            delta_usd_pairs_down: current_usd_pairs_down as i64 - point.usd_pairs_down as i64,
        };
        comparisons.push(point);
        comparison_deltas.push(delta);
    }

    (comparisons, comparison_deltas, warnings)
}

async fn build_comparison_snapshot(
    req: &ForexRequest,
    specs: &[PairSpec],
    cache_dir: &Path,
    as_of: DateTime<Utc>,
) -> PairComparisonSnapshot {
    use futures::stream::{self, StreamExt};
    let rows: Vec<Option<(String, f64)>> = stream::iter(specs.iter().cloned().map(|spec| {
        let ts_req = TimeseriesRequest {
            tickers: vec![spec.ticker.clone()],
            range: req.range,
            granularity: req.granularity,
            as_of: Some(as_of),
            provider: ProviderKind::Yahoo,
            max_points_per_ticker: None,
        };
        async move {
            let resp = fetch_timeseries(ts_req, cache_dir).await.ok()?;
            let series = resp.series.first()?;
            let usd_change_pct = usd_change_from_candles(spec.usd_side, &series.candles)?;
            Some((spec.pair, usd_change_pct))
        }
    }))
    .buffer_unordered(8)
    .collect()
    .await;

    let mut usd_change_by_pair = std::collections::HashMap::new();
    for row in rows.into_iter().flatten() {
        usd_change_by_pair.insert(row.0, row.1);
    }

    PairComparisonSnapshot {
        as_of,
        usd_change_by_pair,
    }
}

fn usd_change_from_candles(side: UsdSide, candles: &[Candle]) -> Option<f64> {
    let first = candles.first()?;
    let last = candles.last()?;
    if candles.len() < 2 || first.c == 0.0 {
        return None;
    }
    let raw_change_pct = ((last.c - first.c) / first.c) * 100.0;
    Some(usd_adjust(side, raw_change_pct))
}

fn comparison_point_from_snapshot(snapshot: &PairComparisonSnapshot) -> ForexComparisonPoint {
    let mut ranked = snapshot
        .usd_change_by_pair
        .iter()
        .map(|(pair, change)| (pair.clone(), *change))
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let usd_pairs_up = ranked.iter().filter(|(_, v)| *v > 0.0).count();
    let usd_pairs_down = ranked.iter().filter(|(_, v)| *v < 0.0).count();
    let usd_strength_score_pct = if ranked.is_empty() {
        None
    } else {
        Some(ranked.iter().map(|(_, v)| *v).sum::<f64>() / ranked.len() as f64)
    };

    ForexComparisonPoint {
        as_of: snapshot.as_of,
        usd_strength_score_pct,
        usd_pairs_up,
        usd_pairs_down,
        strongest_usd_pair: ranked.first().map(|(pair, _)| pair.clone()),
        weakest_usd_pair: ranked.last().map(|(pair, _)| pair.clone()),
    }
}

fn update_delta_context(
    cache_dir: &Path,
    req: &ForexRequest,
    resolved_pairs: &[String],
    pairs: &[ForexPairPerformance],
    as_of: DateTime<Utc>,
    warnings: &mut Vec<String>,
) -> Option<ForexDeltaContext> {
    if req.as_of.is_some() {
        // Historical point-in-time reads should not overwrite live delta baseline.
        return None;
    }

    let current_map = pairs
        .iter()
        .filter_map(|p| p.usd_change_pct.map(|v| (p.pair.clone(), v)))
        .collect::<std::collections::HashMap<_, _>>();
    if current_map.is_empty() {
        return None;
    }

    let state_path = cache_dir.join("finance").join(FOREX_DELTA_STATE_FILE);
    let legacy_state_path = cache_dir.join(FOREX_DELTA_STATE_FILE);
    let mut state = if state_path.exists() {
        load_forex_delta_state(&state_path, warnings)
    } else {
        load_forex_delta_state(&legacy_state_path, warnings)
    };
    let key = forex_delta_state_key(req, resolved_pairs);
    let previous = state.snapshots.get(&key).cloned();
    let current_synced_at = Utc::now();

    state.snapshots.insert(
        key,
        ForexDeltaSnapshot {
            as_of,
            captured_at: Some(current_synced_at),
            usd_change_by_pair: current_map.clone(),
        },
    );
    prune_forex_delta_state(&mut state);
    persist_forex_delta_state(&state_path, &state, warnings);

    let previous = previous?;
    let mut compared_pairs = 0usize;
    let mut changed_pairs = 0usize;
    let mut top_pair_deltas = Vec::new();
    for (pair, current_usd_change_pct) in &current_map {
        let Some(previous_usd_change_pct) = previous.usd_change_by_pair.get(pair).copied() else {
            continue;
        };
        compared_pairs += 1;
        let delta_usd_change_pct = *current_usd_change_pct - previous_usd_change_pct;
        if delta_usd_change_pct.abs() >= FOREX_DELTA_CHANGE_EPSILON {
            changed_pairs += 1;
            top_pair_deltas.push(ForexDeltaPairMove {
                pair: pair.clone(),
                previous_usd_change_pct,
                current_usd_change_pct: *current_usd_change_pct,
                delta_usd_change_pct,
            });
        }
    }

    if compared_pairs == 0 {
        return None;
    }

    top_pair_deltas.sort_by(|a, b| {
        b.delta_usd_change_pct
            .abs()
            .partial_cmp(&a.delta_usd_change_pct.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.pair.cmp(&b.pair))
    });
    top_pair_deltas.truncate(FOREX_DELTA_TOP_LIMIT);

    Some(ForexDeltaContext {
        previous_as_of: previous.as_of,
        current_as_of: as_of,
        previous_synced_at: previous.captured_at,
        current_synced_at: Some(current_synced_at),
        compared_pairs,
        changed_pairs,
        top_pair_deltas,
    })
}

fn forex_delta_state_key(req: &ForexRequest, resolved_pairs: &[String]) -> String {
    let mut key = format!(
        "v1|range={}|granularity={}|include_em={}",
        req.range.to_string_compact(),
        req.granularity.to_string_compact(),
        req.include_em
    );
    if !resolved_pairs.is_empty() {
        key.push_str("|pairs=");
        key.push_str(&resolved_pairs.join(","));
    }
    key
}

fn load_forex_delta_state(path: &Path, warnings: &mut Vec<String>) -> ForexDeltaState {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                warnings.push(format!("read forex delta state {}: {e}", path.display()));
            }
            return ForexDeltaState::default();
        }
    };
    match serde_json::from_str::<ForexDeltaState>(&raw) {
        Ok(state) => state,
        Err(e) => {
            warnings.push(format!(
                "parse forex delta state {}: {e} (state reset)",
                path.display()
            ));
            ForexDeltaState::default()
        }
    }
}

fn prune_forex_delta_state(state: &mut ForexDeltaState) {
    if state.snapshots.len() <= FOREX_DELTA_MAX_SNAPSHOTS {
        return;
    }
    let mut ranked = state
        .snapshots
        .iter()
        .map(|(key, snapshot)| (key.clone(), snapshot.as_of))
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let keep = ranked
        .into_iter()
        .take(FOREX_DELTA_MAX_SNAPSHOTS)
        .map(|(key, _)| key)
        .collect::<std::collections::HashSet<_>>();
    state.snapshots.retain(|key, _| keep.contains(key));
}

fn persist_forex_delta_state(path: &Path, state: &ForexDeltaState, warnings: &mut Vec<String>) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warnings.push(format!(
                "create forex delta state directory {}: {e}",
                parent.display()
            ));
            return;
        }
    }

    let raw = match serde_json::to_string_pretty(state) {
        Ok(raw) => raw,
        Err(e) => {
            warnings.push(format!("serialize forex delta state {}: {e}", path.display()));
            return;
        }
    };
    if let Err(e) = std::fs::write(path, raw) {
        warnings.push(format!("write forex delta state {}: {e}", path.display()));
    }
}

async fn fetch_usd_benchmark(
    range: Span,
    as_of: DateTime<Utc>,
    horizons: &[Span],
    cache_dir: &Path,
) -> Result<Option<ForexUsdBenchmark>> {
    let ts_req = TimeseriesRequest {
        tickers: vec![USD_BENCHMARK_SYMBOL.to_string()],
        range,
        granularity: Span {
            n: 1,
            unit: SpanUnit::Day,
        },
        as_of: Some(as_of),
        provider: ProviderKind::Fred,
        max_points_per_ticker: None,
    };
    let resp = fetch_timeseries(ts_req, cache_dir).await?;
    let Some(series) = resp.series.first() else {
        return Ok(None);
    };
    if series.candles.len() < 2 {
        return Ok(None);
    }
    let Some(first) = series.candles.first() else {
        return Ok(None);
    };
    let Some(last) = series.candles.last() else {
        return Ok(None);
    };

    let change_pct = if first.c == 0.0 {
        None
    } else {
        Some(((last.c - first.c) / first.c) * 100.0)
    };
    let mut horizon_change_pct = std::collections::BTreeMap::new();
    for span in horizons {
        let key = span.to_string_compact();
        let target = last.t - span.approx_duration();
        let Some(anchor) = series.candles.iter().rev().find(|c| c.t <= target) else {
            continue;
        };
        if anchor.c == 0.0 {
            continue;
        }
        let raw_change_pct = ((last.c - anchor.c) / anchor.c) * 100.0;
        horizon_change_pct.insert(key, raw_change_pct);
    }

    Ok(Some(ForexUsdBenchmark {
        source: "fred".to_string(),
        symbol: USD_BENCHMARK_SYMBOL.to_string(),
        as_of: last.t,
        change_pct,
        horizon_change_pct,
    }))
}

fn resolve_horizons(raw: &[Span]) -> Vec<Span> {
    if raw.is_empty() {
        return default_horizons();
    }
    let mut dedup = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for span in raw {
        let key = span.to_string_compact();
        if dedup.insert(key) {
            out.push(*span);
        }
    }
    out
}

fn default_horizons() -> Vec<Span> {
    vec![
        Span {
            n: 1,
            unit: SpanUnit::Day,
        },
        Span {
            n: 1,
            unit: SpanUnit::Week,
        },
        Span {
            n: 1,
            unit: SpanUnit::Month,
        },
        Span {
            n: 3,
            unit: SpanUnit::Month,
        },
        Span {
            n: 6,
            unit: SpanUnit::Month,
        },
        Span {
            n: 1,
            unit: SpanUnit::Year,
        },
    ]
}

fn normalize_symbols(values: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for v in values {
        let n = v.trim().to_ascii_uppercase();
        if n.is_empty() || !seen.insert(n.clone()) {
            continue;
        }
        out.push(n);
    }
    out
}

fn build_date_clusters(hits: &[ForexHitEvent]) -> Vec<ForexDateCluster> {
    let mut by_date: std::collections::HashMap<String, (usize, f64)> = std::collections::HashMap::new();
    for hit in hits {
        let entry = by_date
            .entry(hit.date.clone())
            .or_insert((0, 0.0));
        entry.0 += 1;
        let mag = hit.usd_impact_pct.unwrap_or(hit.daily_change_pct).abs();
        if mag > entry.1 {
            entry.1 = mag;
        }
    }
    let mut out = by_date
        .into_iter()
        .map(|(date, (move_count, max_abs_usd_impact_pct))| ForexDateCluster {
            date,
            move_count,
            max_abs_usd_impact_pct,
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| {
        b.move_count
            .cmp(&a.move_count)
            .then_with(|| {
                b.max_abs_usd_impact_pct
                    .partial_cmp(&a.max_abs_usd_impact_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.date.cmp(&b.date))
    });
    out.truncate(10);
    out
}

fn collapse_hits_by_pair_date(hits: &[ForexHitEvent]) -> Vec<ForexHitEvent> {
    let mut by_key: std::collections::HashMap<(String, String), ForexHitEvent> =
        std::collections::HashMap::new();
    for hit in hits {
        let key = (hit.pair.clone(), hit.date.clone());
        let mag = hit.usd_impact_pct.unwrap_or(hit.daily_change_pct).abs();
        let replace = by_key
            .get(&key)
            .map(|existing| mag > existing.usd_impact_pct.unwrap_or(existing.daily_change_pct).abs())
            .unwrap_or(true);
        if replace {
            by_key.insert(key, hit.clone());
        }
    }
    by_key.into_values().collect()
}

fn parse_pair_spec(ticker: &str) -> Option<PairSpec> {
    let core = ticker.strip_suffix("=X")?;
    if core.len() != 6 || !core.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let base = core[..3].to_string();
    let quote = core[3..].to_string();
    let usd_side = if base == "USD" {
        UsdSide::Base
    } else if quote == "USD" {
        UsdSide::Quote
    } else {
        UsdSide::None
    };
    let non_usd_currency = if base == "USD" {
        Some(quote.clone())
    } else if quote == "USD" {
        Some(base.clone())
    } else {
        None
    };
    Some(PairSpec {
        ticker: ticker.to_string(),
        pair: format!("{base}/{quote}"),
        base_currency: base,
        quote_currency: quote,
        usd_side,
        non_usd_currency,
    })
}

fn usd_ticker_for_currency(currency: &str) -> String {
    match currency {
        "EUR" | "GBP" | "AUD" | "NZD" => format!("{currency}USD=X"),
        _ => format!("USD{currency}=X"),
    }
}

fn group_currencies(group: &str) -> &'static [&'static str] {
    match group {
        "majors" | "g10" => &[
            "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "NZD", "SEK", "NOK",
        ],
        "em" => &["MXN", "ZAR", "TRY", "BRL", "INR", "KRW", "CNY", "PLN", "HUF", "CZK"],
        "europe" => &["EUR", "GBP", "CHF", "SEK", "NOK", "TRY", "PLN", "HUF", "CZK"],
        "americas" => &["CAD", "MXN", "BRL"],
        "asia" => &["JPY", "CNY", "KRW", "INR", "SGD", "HKD"],
        "commodity" => &["AUD", "CAD", "NOK", "NZD"],
        _ => &[],
    }
}

fn country_to_currency(country: &str) -> Option<&'static str> {
    match country {
        "US" | "USA" => Some("USD"),
        "EU" => Some("EUR"),
        "GB" | "UK" => Some("GBP"),
        "CA" => Some("CAD"),
        "MX" => Some("MXN"),
        "BR" => Some("BRL"),
        "JP" => Some("JPY"),
        "CN" => Some("CNY"),
        "KR" => Some("KRW"),
        "IN" => Some("INR"),
        "CH" => Some("CHF"),
        "SE" => Some("SEK"),
        "NO" => Some("NOK"),
        "AU" => Some("AUD"),
        "NZ" => Some("NZD"),
        "ZA" => Some("ZAR"),
        "TR" => Some("TRY"),
        "SG" => Some("SGD"),
        "HK" => Some("HKD"),
        "PL" => Some("PLN"),
        "HU" => Some("HUF"),
        "CZ" => Some("CZK"),
        "DE" | "FR" | "IT" | "ES" | "NL" | "BE" | "AT" | "IE" | "PT" | "FI" | "GR" => {
            Some("EUR")
        }
        _ => None,
    }
}
