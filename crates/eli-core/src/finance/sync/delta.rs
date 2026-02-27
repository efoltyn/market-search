use super::super::{
    OddsListedMarket, OddsSyncBaselineQuality, OddsSyncDeltaIndex, OddsSyncDeltaSummary,
    OddsSyncMarketDelta, OddsSyncSourceBaseline, OddsSyncSourceDelta,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;

const TOP_MOVERS_LIMIT: usize = 5;
const PROBABILITY_CHANGE_EPSILON: f64 = 0.0001;
const TOP_PROBABILITY_MOVE_MIN_PCT_POINTS: f64 = 1.0;
const TOP_YES_PRICE_MOVE_MIN_CENTS: i64 = 2;
const TOP_VOLUME_MOVE_MIN_UNITS: i64 = 1_000;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub(crate) struct OddsSyncState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_sync_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) sources: HashMap<String, OddsSyncSourceState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OddsSyncSourceState {
    pub(crate) synced_at: DateTime<Utc>,
    #[serde(default)]
    pub(crate) baseline_quality: SourceBaselineQuality,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) baseline_quality_reason: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) markets: HashMap<String, OddsSyncMarketState>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceBaselineQuality {
    Trusted,
    Untrusted,
    #[default]
    Unknown,
}

impl SourceBaselineQuality {
    pub(crate) fn is_trusted(&self) -> bool {
        matches!(self, Self::Trusted)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OddsSyncMarketState {
    pub(crate) ticker: String,
    pub(crate) title: String,
    pub(crate) event_ticker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) probability_yes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) yes_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) clob_token_ids: Option<Vec<String>>,
}

pub(crate) struct SourceDeltaBuild {
    pub(crate) source_delta: OddsSyncSourceDelta,
    pub(crate) market_deltas: Vec<OddsSyncMarketDelta>,
    pub(crate) next_state: OddsSyncSourceState,
}

pub(crate) fn source_market_key(source: &str, ticker: &str) -> String {
    format!("{source}::{ticker}")
}

pub(crate) fn load_sync_state(path: &Path) -> OddsSyncState {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return OddsSyncState::default(),
    };
    serde_json::from_str::<OddsSyncState>(&raw).unwrap_or_default()
}

pub(crate) fn write_sync_state(path: &Path, state: &OddsSyncState) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            crate::Error::Other(format!(
                "create sync state directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    let raw = serde_json::to_string_pretty(state)
        .map_err(|e| crate::Error::Other(format!("serialize sync state: {e}")))?;
    write_json_atomic(path, raw.as_bytes(), "sync state")
}

pub(crate) fn write_delta_index(path: &Path, index: &OddsSyncDeltaIndex) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            crate::Error::Other(format!(
                "create delta index directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    let raw = serde_json::to_string_pretty(index)
        .map_err(|e| crate::Error::Other(format!("serialize delta index: {e}")))?;
    write_json_atomic(path, raw.as_bytes(), "delta index")
}

pub(crate) fn build_source_delta(
    source: &str,
    current_markets: &[OddsListedMarket],
    previous_state: Option<&OddsSyncSourceState>,
    current_sync_at: DateTime<Utc>,
) -> SourceDeltaBuild {
    let previous_markets_map = previous_state
        .map(|s| &s.markets)
        .cloned()
        .unwrap_or_default();
    let previous_markets = previous_markets_map.len();

    let mut next_state_markets: HashMap<String, OddsSyncMarketState> = HashMap::new();
    let mut seen = HashSet::new();
    let mut market_deltas: Vec<OddsSyncMarketDelta> = Vec::new();

    let mut new_markets = 0usize;
    let mut removed_markets = 0usize;
    let mut updated_markets = 0usize;
    let mut changed_markets = 0usize;
    let mut unchanged_markets = 0usize;
    let mut probability_changed_markets = 0usize;
    let mut yes_price_changed_markets = 0usize;
    let mut volume_changed_markets = 0usize;
    let mut status_changed_markets = 0usize;

    for market in current_markets {
        let key = market.ticker.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let current_state = market_to_state(market);
        let previous = previous_markets_map.get(&key);
        let delta = build_market_delta(source, previous, Some(&current_state));
        let metric_changes = count_metric_changes(&delta);
        if let Some(prev) = previous {
            let changed = metric_changes.any_changed;
            if changed {
                updated_markets += 1;
                changed_markets += 1;
                probability_changed_markets += usize::from(metric_changes.probability_changed);
                yes_price_changed_markets += usize::from(metric_changes.yes_price_changed);
                volume_changed_markets += usize::from(metric_changes.volume_changed);
                status_changed_markets += usize::from(metric_changes.status_changed);
                market_deltas.push(delta);
            } else {
                unchanged_markets += 1;
            }
            // Keep most recent title/category if provider improves labels.
            let merged = OddsSyncMarketState {
                ticker: current_state.ticker.clone(),
                title: if current_state.title.trim().is_empty() {
                    prev.title.clone()
                } else {
                    current_state.title.clone()
                },
                event_ticker: if current_state.event_ticker.trim().is_empty() {
                    prev.event_ticker.clone()
                } else {
                    current_state.event_ticker.clone()
                },
                category: current_state
                    .category
                    .clone()
                    .or_else(|| prev.category.clone()),
                probability_yes: current_state.probability_yes,
                yes_price: current_state.yes_price,
                volume: current_state.volume,
                status: current_state.status.clone().or_else(|| prev.status.clone()),
                clob_token_ids: current_state
                    .clob_token_ids
                    .clone()
                    .or_else(|| prev.clob_token_ids.clone()),
            };
            next_state_markets.insert(key.clone(), merged);
        } else {
            new_markets += 1;
            changed_markets += 1;
            probability_changed_markets += usize::from(metric_changes.probability_changed);
            yes_price_changed_markets += usize::from(metric_changes.yes_price_changed);
            volume_changed_markets += usize::from(metric_changes.volume_changed);
            status_changed_markets += usize::from(metric_changes.status_changed);
            market_deltas.push(delta);
            next_state_markets.insert(key.clone(), current_state);
        }
        seen.insert(key);
    }

    for (ticker, previous) in &previous_markets_map {
        if seen.contains(ticker) {
            continue;
        }
        removed_markets += 1;
        changed_markets += 1;
        let delta = build_market_delta(source, Some(previous), None);
        let metric_changes = count_metric_changes(&delta);
        probability_changed_markets += usize::from(metric_changes.probability_changed);
        yes_price_changed_markets += usize::from(metric_changes.yes_price_changed);
        volume_changed_markets += usize::from(metric_changes.volume_changed);
        status_changed_markets += usize::from(metric_changes.status_changed);
        market_deltas.push(delta);
    }

    let current_markets_count = next_state_markets.len();
    let compared_markets = current_markets_count + removed_markets;
    let churn_markets = new_markets + removed_markets;

    let source_delta = OddsSyncSourceDelta {
        source: source.to_string(),
        previous_markets,
        current_markets: current_markets_count,
        compared_markets,
        updated_markets,
        churn_markets,
        new_markets,
        removed_markets,
        changed_markets,
        unchanged_markets,
        probability_changed_markets,
        yes_price_changed_markets,
        volume_changed_markets,
        status_changed_markets,
        baseline_quality: OddsSyncBaselineQuality::Trusted,
        baseline_reset_applied: false,
        baseline_reset_reason: None,
        top_probability_moves: top_probability_moves(&market_deltas, TOP_MOVERS_LIMIT),
        top_yes_price_moves: top_yes_price_moves(&market_deltas, TOP_MOVERS_LIMIT),
        top_volume_moves: top_volume_moves(&market_deltas, TOP_MOVERS_LIMIT),
    };

    let next_state = OddsSyncSourceState {
        synced_at: current_sync_at,
        baseline_quality: SourceBaselineQuality::Trusted,
        baseline_quality_reason: None,
        markets: next_state_markets,
    };

    SourceDeltaBuild {
        source_delta,
        market_deltas,
        next_state,
    }
}

pub(crate) fn apply_source_delta_baseline_reset(
    source_delta: &mut OddsSyncSourceDelta,
    market_deltas: &mut Vec<OddsSyncMarketDelta>,
    previous_markets: usize,
    reason: Option<String>,
) {
    source_delta.previous_markets = previous_markets;
    source_delta.compared_markets = 0;
    source_delta.updated_markets = 0;
    source_delta.churn_markets = 0;
    source_delta.new_markets = 0;
    source_delta.removed_markets = 0;
    source_delta.changed_markets = 0;
    source_delta.unchanged_markets = source_delta.current_markets;
    source_delta.probability_changed_markets = 0;
    source_delta.yes_price_changed_markets = 0;
    source_delta.volume_changed_markets = 0;
    source_delta.status_changed_markets = 0;
    source_delta.top_probability_moves.clear();
    source_delta.top_yes_price_moves.clear();
    source_delta.top_volume_moves.clear();
    source_delta.baseline_quality = OddsSyncBaselineQuality::Reset;
    source_delta.baseline_reset_applied = true;
    source_delta.baseline_reset_reason = reason;
    market_deltas.clear();
}

pub(crate) fn build_overall_delta(
    previous_sync_at: Option<DateTime<Utc>>,
    current_sync_at: DateTime<Utc>,
    source_deltas: &[OddsSyncSourceDelta],
    market_deltas: &[OddsSyncMarketDelta],
) -> OddsSyncDeltaSummary {
    let previous_markets = source_deltas.iter().map(|d| d.previous_markets).sum();
    let current_markets = source_deltas.iter().map(|d| d.current_markets).sum();
    let compared_markets = source_deltas.iter().map(|d| d.compared_markets).sum();
    let updated_markets = source_deltas.iter().map(|d| d.updated_markets).sum();
    let churn_markets = source_deltas.iter().map(|d| d.churn_markets).sum();
    let new_markets = source_deltas.iter().map(|d| d.new_markets).sum();
    let removed_markets = source_deltas.iter().map(|d| d.removed_markets).sum();
    let changed_markets = source_deltas.iter().map(|d| d.changed_markets).sum();
    let unchanged_markets = source_deltas.iter().map(|d| d.unchanged_markets).sum();
    let probability_changed_markets = source_deltas
        .iter()
        .map(|d| d.probability_changed_markets)
        .sum();
    let yes_price_changed_markets = source_deltas
        .iter()
        .map(|d| d.yes_price_changed_markets)
        .sum();
    let volume_changed_markets = source_deltas.iter().map(|d| d.volume_changed_markets).sum();
    let status_changed_markets = source_deltas.iter().map(|d| d.status_changed_markets).sum();

    OddsSyncDeltaSummary {
        previous_sync_at,
        current_sync_at,
        previous_markets,
        current_markets,
        compared_markets,
        updated_markets,
        churn_markets,
        new_markets,
        removed_markets,
        changed_markets,
        unchanged_markets,
        probability_changed_markets,
        yes_price_changed_markets,
        volume_changed_markets,
        status_changed_markets,
        top_probability_moves: top_probability_moves(market_deltas, TOP_MOVERS_LIMIT),
        top_yes_price_moves: top_yes_price_moves(market_deltas, TOP_MOVERS_LIMIT),
        top_volume_moves: top_volume_moves(market_deltas, TOP_MOVERS_LIMIT),
    }
}

pub(crate) fn build_delta_index(
    summary: &OddsSyncDeltaSummary,
    source_deltas: &[OddsSyncSourceDelta],
    market_deltas: &[OddsSyncMarketDelta],
) -> OddsSyncDeltaIndex {
    let mut market_deltas_map: BTreeMap<String, OddsSyncMarketDelta> = BTreeMap::new();
    for delta in market_deltas {
        market_deltas_map.insert(
            source_market_key(&delta.source, &delta.ticker),
            delta.clone(),
        );
    }
    let mut source_baselines: BTreeMap<String, OddsSyncSourceBaseline> = BTreeMap::new();
    for source_delta in source_deltas {
        source_baselines.insert(
            source_delta.source.clone(),
            OddsSyncSourceBaseline {
                baseline_quality: source_delta.baseline_quality.clone(),
                baseline_reset_applied: source_delta.baseline_reset_applied,
                baseline_reset_reason: source_delta.baseline_reset_reason.clone(),
            },
        );
    }
    OddsSyncDeltaIndex {
        previous_sync_at: summary.previous_sync_at,
        current_sync_at: summary.current_sync_at,
        changed_markets: summary.changed_markets,
        market_deltas: market_deltas_map,
        source_baselines,
        top_probability_moves: summary.top_probability_moves.clone(),
        top_yes_price_moves: summary.top_yes_price_moves.clone(),
        top_volume_moves: summary.top_volume_moves.clone(),
    }
}

fn write_json_atomic(path: &Path, bytes: &[u8], label: &str) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            crate::Error::Other(format!(
                "create {label} directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sync_state.json");
    let tmp_name = format!(
        ".{}.{}.{}.tmp",
        file_name,
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let tmp_path = path.with_file_name(tmp_name);
    let mut tmp_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)
        .map_err(|e| {
            crate::Error::Other(format!("open temp {label} {}: {e}", tmp_path.display()))
        })?;
    tmp_file.write_all(bytes).map_err(|e| {
        crate::Error::Other(format!("write temp {label} {}: {e}", tmp_path.display()))
    })?;
    tmp_file.sync_all().map_err(|e| {
        crate::Error::Other(format!("fsync temp {label} {}: {e}", tmp_path.display()))
    })?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        crate::Error::Other(format!(
            "atomic rename {label} {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        ))
    })?;
    if let Some(parent) = path.parent() {
        if let Ok(dir_file) = std::fs::File::open(parent) {
            let _ = dir_file.sync_all();
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct MetricChanges {
    any_changed: bool,
    probability_changed: bool,
    yes_price_changed: bool,
    volume_changed: bool,
    status_changed: bool,
}

fn count_metric_changes(delta: &OddsSyncMarketDelta) -> MetricChanges {
    let probability_changed = delta.probability_delta.is_some();
    let yes_price_changed = delta.yes_price_delta.is_some();
    let volume_changed = delta.volume_delta.is_some();
    let status_changed = status_changed(
        delta.previous_status.as_deref(),
        delta.current_status.as_deref(),
    );
    let any_changed = probability_changed || yes_price_changed || volume_changed || status_changed;
    MetricChanges {
        any_changed,
        probability_changed,
        yes_price_changed,
        volume_changed,
        status_changed,
    }
}

fn status_changed(previous_status: Option<&str>, current_status: Option<&str>) -> bool {
    normalize_status(previous_status) != normalize_status(current_status)
}

fn normalize_status(status: Option<&str>) -> Option<String> {
    status
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
}

fn market_to_state(market: &OddsListedMarket) -> OddsSyncMarketState {
    OddsSyncMarketState {
        ticker: market.ticker.clone(),
        title: market.title.clone(),
        event_ticker: market.event_ticker.clone(),
        category: market.category.clone(),
        probability_yes: market.probability_yes,
        yes_price: market.yes_price,
        volume: market.volume,
        status: market.status.clone(),
        clob_token_ids: market.clob_token_ids.clone(),
    }
}

fn build_market_delta(
    source: &str,
    previous: Option<&OddsSyncMarketState>,
    current: Option<&OddsSyncMarketState>,
) -> OddsSyncMarketDelta {
    let change_kind = match (previous.is_some(), current.is_some()) {
        (false, true) => "new",
        (true, false) => "removed",
        _ => "updated",
    }
    .to_string();

    let previous_probability = previous.and_then(|m| m.probability_yes);
    let current_probability = current.and_then(|m| m.probability_yes);
    let probability_delta = option_f64_delta(previous_probability, current_probability);
    let probability_delta = probability_delta.filter(|d| d.abs() > PROBABILITY_CHANGE_EPSILON);
    let probability_delta_pct_points = probability_delta.map(|d| d * 100.0);

    let previous_yes_price = previous.and_then(|m| m.yes_price);
    let current_yes_price = current.and_then(|m| m.yes_price);
    let yes_price_delta = option_i64_delta(previous_yes_price, current_yes_price);

    let previous_volume = previous.and_then(|m| m.volume);
    let current_volume = current.and_then(|m| m.volume);
    let volume_delta = option_i64_delta(previous_volume, current_volume);

    let previous_status = previous.and_then(|m| normalize_status(m.status.as_deref()));
    let current_status = current.and_then(|m| normalize_status(m.status.as_deref()));

    OddsSyncMarketDelta {
        source: source.to_string(),
        ticker: current
            .map(|m| m.ticker.clone())
            .or_else(|| previous.map(|m| m.ticker.clone()))
            .unwrap_or_default(),
        title: current
            .map(|m| m.title.clone())
            .or_else(|| previous.map(|m| m.title.clone()))
            .unwrap_or_default(),
        event_ticker: current
            .map(|m| m.event_ticker.clone())
            .or_else(|| previous.map(|m| m.event_ticker.clone()))
            .unwrap_or_default(),
        category: current
            .and_then(|m| m.category.clone())
            .or_else(|| previous.and_then(|m| m.category.clone())),
        change_kind,
        previous_probability_yes: previous_probability,
        current_probability_yes: current_probability,
        probability_delta,
        probability_delta_pct_points,
        previous_yes_price,
        current_yes_price,
        yes_price_delta,
        previous_volume,
        current_volume,
        volume_delta,
        previous_status,
        current_status,
    }
}

fn option_f64_delta(previous: Option<f64>, current: Option<f64>) -> Option<f64> {
    match (previous, current) {
        (Some(prev), Some(cur)) => Some(cur - prev),
        (None, Some(cur)) => Some(cur),
        (Some(prev), None) => Some(-prev),
        (None, None) => None,
    }
}

fn option_i64_delta(previous: Option<i64>, current: Option<i64>) -> Option<i64> {
    match (previous, current) {
        (Some(prev), Some(cur)) if prev != cur => Some(cur - prev),
        (None, Some(cur)) if cur != 0 => Some(cur),
        (Some(prev), None) if prev != 0 => Some(-prev),
        _ => None,
    }
}

fn sort_by_abs_desc(values: &mut [OddsSyncMarketDelta], key: impl Fn(&OddsSyncMarketDelta) -> f64) {
    values.sort_by(|a, b| {
        let kb = key(b).abs();
        let ka = key(a).abs();
        kb.partial_cmp(&ka)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.current_volume
                    .unwrap_or(0)
                    .cmp(&a.current_volume.unwrap_or(0))
                    .then_with(|| a.ticker.cmp(&b.ticker))
            })
    });
}

fn top_probability_moves(deltas: &[OddsSyncMarketDelta], limit: usize) -> Vec<OddsSyncMarketDelta> {
    let mut out: Vec<OddsSyncMarketDelta> = deltas
        .iter()
        .filter(|d| {
            d.change_kind == "updated"
                && d.probability_delta_pct_points
                    .map(|v| v.abs() >= TOP_PROBABILITY_MOVE_MIN_PCT_POINTS)
                    .unwrap_or(false)
        })
        .cloned()
        .collect();
    sort_by_abs_desc(&mut out, |d| d.probability_delta_pct_points.unwrap_or(0.0));
    out.truncate(limit);
    out
}

fn top_yes_price_moves(deltas: &[OddsSyncMarketDelta], limit: usize) -> Vec<OddsSyncMarketDelta> {
    let mut out: Vec<OddsSyncMarketDelta> = deltas
        .iter()
        .filter(|d| {
            d.change_kind == "updated"
                && d.yes_price_delta
                    .map(|v| v.abs() >= TOP_YES_PRICE_MOVE_MIN_CENTS)
                    .unwrap_or(false)
        })
        .cloned()
        .collect();
    sort_by_abs_desc(&mut out, |d| d.yes_price_delta.unwrap_or(0) as f64);
    out.truncate(limit);
    out
}

fn top_volume_moves(deltas: &[OddsSyncMarketDelta], limit: usize) -> Vec<OddsSyncMarketDelta> {
    let mut out: Vec<OddsSyncMarketDelta> = deltas
        .iter()
        .filter(|d| {
            d.change_kind == "updated"
                && d.volume_delta
                    .map(|v| v.abs() >= TOP_VOLUME_MOVE_MIN_UNITS)
                    .unwrap_or(false)
        })
        .cloned()
        .collect();
    sort_by_abs_desc(&mut out, |d| d.volume_delta.unwrap_or(0) as f64);
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eli_finance_types::{Freshness, FreshnessOrigin, FreshnessQuality, FreshnessState};

    fn listed_market(
        ticker: &str,
        probability_yes: Option<f64>,
        yes_price: Option<i64>,
        volume: Option<i64>,
        status: Option<&str>,
    ) -> OddsListedMarket {
        OddsListedMarket {
            ticker: ticker.to_string(),
            title: format!("title {ticker}"),
            event_ticker: format!("event {ticker}"),
            freshness: Freshness::new(
                Utc::now(),
                Utc::now(),
                FreshnessState::Unknown,
                FreshnessOrigin::Derived,
                FreshnessQuality::Estimated,
            ),
            yes_price,
            volume,
            status: status.map(ToString::to_string),
            source: Some("kalshi".to_string()),
            market_id: None,
            event_id: None,
            slug: None,
            outcomes: None,
            outcome_prices: None,
            clob_token_ids: None,
            probability_yes,
            category: Some("Economics".to_string()),
        }
    }

    #[test]
    fn source_delta_tracks_new_removed_and_updated_markets() {
        let now = Utc::now();
        let previous_state = OddsSyncSourceState {
            synced_at: now,
            baseline_quality: SourceBaselineQuality::Trusted,
            baseline_quality_reason: None,
            markets: HashMap::from([
                (
                    "A".to_string(),
                    OddsSyncMarketState {
                        ticker: "A".to_string(),
                        title: "A".to_string(),
                        event_ticker: "EA".to_string(),
                        category: Some("Economics".to_string()),
                        probability_yes: Some(0.50),
                        yes_price: Some(50),
                        volume: Some(1_000),
                        status: Some("open".to_string()),
                        clob_token_ids: None,
                    },
                ),
                (
                    "B".to_string(),
                    OddsSyncMarketState {
                        ticker: "B".to_string(),
                        title: "B".to_string(),
                        event_ticker: "EB".to_string(),
                        category: Some("Economics".to_string()),
                        probability_yes: Some(0.30),
                        yes_price: Some(30),
                        volume: Some(2_000),
                        status: Some("open".to_string()),
                        clob_token_ids: None,
                    },
                ),
            ]),
        };

        let current_markets = vec![
            listed_market("A", Some(0.62), Some(62), Some(1_800), Some("open")),
            listed_market("C", Some(0.10), Some(10), Some(400), Some("open")),
        ];

        let built = build_source_delta("kalshi", &current_markets, Some(&previous_state), now);
        assert_eq!(built.source_delta.previous_markets, 2);
        assert_eq!(built.source_delta.current_markets, 2);
        assert_eq!(built.source_delta.new_markets, 1);
        assert_eq!(built.source_delta.removed_markets, 1);
        assert_eq!(built.source_delta.changed_markets, 3);
        assert_eq!(built.source_delta.unchanged_markets, 0);
        assert!(built
            .market_deltas
            .iter()
            .any(|d| d.ticker == "A" && d.change_kind == "updated"));
        assert!(built
            .market_deltas
            .iter()
            .any(|d| d.ticker == "B" && d.change_kind == "removed"));
        assert!(built
            .market_deltas
            .iter()
            .any(|d| d.ticker == "C" && d.change_kind == "new"));
    }

    #[test]
    fn epsilon_filters_micro_probability_noise() {
        let previous = OddsSyncMarketState {
            ticker: "X".to_string(),
            title: "X".to_string(),
            event_ticker: "EX".to_string(),
            category: None,
            probability_yes: Some(0.50000),
            yes_price: Some(50),
            volume: Some(100),
            status: Some("open".to_string()),
            clob_token_ids: None,
        };
        let current = OddsSyncMarketState {
            ticker: "X".to_string(),
            title: "X".to_string(),
            event_ticker: "EX".to_string(),
            category: None,
            probability_yes: Some(0.50005),
            yes_price: Some(50),
            volume: Some(100),
            status: Some("open".to_string()),
            clob_token_ids: None,
        };

        let delta = build_market_delta("kalshi", Some(&previous), Some(&current));
        assert!(delta.probability_delta.is_none());
        assert!(delta.probability_delta_pct_points.is_none());
    }
}
