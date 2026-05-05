use crate::{Error, Result};
use chrono::{DateTime, Utc};
use eli_finance_types::{Freshness, FreshnessOrigin, FreshnessQuality, FreshnessState, PolicyMode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const DEFAULT_POLICY_TOML: &str = include_str!("policy/defaults.toml");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RankingPolicy {
    pub title_exact_bonus: f64,
    pub term_match_weight: f64,
    pub ticker_event_match_weight: f64,
    pub category_topic_match_weight: f64,
    pub volume_log_weight: f64,
    pub macro_keyword_weight: i64,
    pub macro_economics_boost: i64,
    pub macro_financials_boost: i64,
    pub macro_mentions_penalty: i64,
    pub macro_offtopic_penalty: i64,
    pub delta_prob_weight: f64,
    pub delta_yes_price_weight: f64,
    pub delta_volume_weight: f64,
    pub novelty_weight: f64,
    pub confidence_weight: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilteringPolicy {
    pub macro_profile_min_relevance: i64,
    pub enforce_requires_match_terms: bool,
    pub enforce_min_volume_usd: Option<f64>,
    pub macro_keywords: Vec<String>,
    pub macro_offtopic_keywords: Vec<String>,
    pub us_hint_keywords: Vec<String>,
    pub policy_action_keywords: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreshnessPolicy {
    pub live_after_seconds: i64,
    pub delayed_after_seconds: i64,
    pub stale_after_seconds: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StdoutCompactionPolicy {
    pub top_categories: usize,
    pub top_markets_by_volume: usize,
    pub top_markets_by_informative_volume: usize,
    pub top_anomalous_zero_yes_markets: usize,
    pub top_near_even_high_volume_markets: usize,
    pub top_high_confidence_high_volume_markets: usize,
    pub top_probability_moves: usize,
    pub top_yes_price_moves: usize,
    pub top_volume_moves: usize,
    pub max_title_chars: usize,
    pub max_category_chars: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroIndicatorPolicy {
    pub id: String,
    pub name: String,
    pub category: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroCatalogPolicy {
    pub indicators: Vec<MacroIndicatorPolicy>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentPolicy {
    pub ranking: RankingPolicy,
    pub filtering: FilteringPolicy,
    pub freshness: FreshnessPolicy,
    pub stdout_compaction: StdoutCompactionPolicy,
    pub macro_catalog: MacroCatalogPolicy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolvedPolicy {
    pub mode: PolicyMode,
    pub sources: Vec<String>,
    pub policy: AgentPolicy,
}

fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_map), toml::Value::Table(overlay_map)) => {
            for (k, v) in overlay_map {
                match base_map.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (slot, other) => *slot = other,
    }
}

fn load_toml_file(path: &Path) -> Result<toml::Value> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Provider(format!("policy read failed '{}': {e}", path.display())))?;
    toml::from_str::<toml::Value>(&raw)
        .map_err(|e| Error::InvalidInput(format!("policy parse failed '{}': {e}", path.display())))
}

fn discover_policy_overlays() -> Vec<PathBuf> {
    let dir = directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.config_dir().join("policies"))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config/eli/policies")
        });
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let is_toml = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("toml"))
                .unwrap_or(false);
            if is_toml {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

pub fn load_policy(policy_file: Option<&Path>, mode: PolicyMode) -> Result<ResolvedPolicy> {
    let mut merged: toml::Value = toml::from_str(DEFAULT_POLICY_TOML)
        .map_err(|e| Error::System(format!("default policy parse failed: {e}")))?;
    let mut sources = vec!["embedded:finance/policy/defaults.toml".to_string()];

    if let Some(path) = policy_file {
        let overlay = load_toml_file(path)?;
        deep_merge(&mut merged, overlay);
        sources.push(path.display().to_string());
    } else {
        for overlay_path in discover_policy_overlays() {
            if let Ok(overlay) = load_toml_file(&overlay_path) {
                deep_merge(&mut merged, overlay);
                sources.push(overlay_path.display().to_string());
            }
        }
    }

    let policy: AgentPolicy = merged
        .try_into()
        .map_err(|e| Error::InvalidInput(format!("policy decode failed: {e}")))?;

    Ok(ResolvedPolicy {
        mode,
        sources,
        policy,
    })
}

pub fn freshness_state_for_age(age_seconds: i64, policy: &FreshnessPolicy) -> FreshnessState {
    if age_seconds <= policy.live_after_seconds {
        FreshnessState::Live
    } else if age_seconds <= policy.delayed_after_seconds {
        FreshnessState::Delayed
    } else if age_seconds <= policy.stale_after_seconds {
        FreshnessState::Historical
    } else {
        FreshnessState::Stale
    }
}

pub fn freshness_from_observed(
    observed_at: DateTime<Utc>,
    collected_at: DateTime<Utc>,
    policy: &FreshnessPolicy,
    origin: FreshnessOrigin,
    quality: FreshnessQuality,
) -> Freshness {
    let age_seconds = (collected_at - observed_at).num_seconds().max(0);
    Freshness::new(
        observed_at,
        collected_at,
        freshness_state_for_age(age_seconds, policy),
        origin,
        quality,
    )
}

pub fn parse_policy_mode(raw: Option<&str>) -> Result<PolicyMode> {
    match raw
        .unwrap_or("observe")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "observe" => Ok(PolicyMode::Observe),
        "assist" => Ok(PolicyMode::Assist),
        "enforce" => Ok(PolicyMode::Enforce),
        other => Err(Error::InvalidInput(format!(
            "invalid policy mode '{other}' (expected observe|assist|enforce)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_freshness_state() {
        let p = FreshnessPolicy {
            live_after_seconds: 15,
            delayed_after_seconds: 600,
            stale_after_seconds: 3600,
        };
        assert_eq!(freshness_state_for_age(1, &p), FreshnessState::Live);
        assert_eq!(freshness_state_for_age(100, &p), FreshnessState::Delayed);
        assert_eq!(
            freshness_state_for_age(1800, &p),
            FreshnessState::Historical
        );
        assert_eq!(freshness_state_for_age(7200, &p), FreshnessState::Stale);
    }

    #[test]
    fn policy_file_overrides_defaults() {
        let path = std::env::temp_dir().join(format!(
            "eli_policy_test_{}_{}.toml",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::write(
            &path,
            r#"
[filtering]
macro_profile_min_relevance = 95
"#,
        )
        .expect("write policy file");

        let resolved = load_policy(Some(&path), PolicyMode::Assist).expect("load policy");
        std::fs::remove_file(&path).ok();

        assert_eq!(resolved.mode, PolicyMode::Assist);
        assert_eq!(resolved.policy.filtering.macro_profile_min_relevance, 95);
        assert!(resolved.policy.ranking.term_match_weight > 0.0);
        assert!(resolved
            .sources
            .iter()
            .any(|s| s == "embedded:finance/policy/defaults.toml"));
        assert!(resolved
            .sources
            .iter()
            .any(|s| s == &path.display().to_string()));
    }
}
