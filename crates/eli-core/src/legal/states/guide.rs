//! Per-state research guide: where each state's law lives, what this tool can
//! reach there, the traps, and seed personal-injury facts with citations.
//!
//! The facts were gathered by a fast research sweep across all 50 states and
//! DC, each researcher marking its own confidence. They are a starting map, not
//! an authority: every fact carries its citation so the statute can be pulled
//! and read (with `legal_statute` where that state is covered) before anything
//! is relied on. Low-confidence rows say so in the row itself.

use super::{normalize_state, supported, StateCoverage};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// When the sweep ran. Facts about deadlines and caps age; say how much.
pub const SWEPT: &str = "2026-10-21";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GuideFact {
    pub fact: String,
    pub value: String,
    pub citation: String,
    pub confidence: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GuideSource {
    pub url: String,
    pub access: String,
    /// The researcher fetched it from a script and saw real content.
    pub verified: bool,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    facts: Vec<GuideFact>,
    traps: Vec<String>,
    sources: BTreeMap<String, GuideSource>,
}

static DATA: LazyLock<BTreeMap<String, Entry>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("data/state_guide.json")).expect("state_guide.json parses")
});

#[derive(Clone, Debug)]
pub struct GuideRequest {
    pub state: String,
    /// Keyword filter over facts and traps, e.g. "notice", "cap", "medical".
    pub topic: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GuideResponse {
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub state: String,
    /// Which legal-search tools answer for this state right now.
    pub tools: Option<StateCoverage>,
    pub facts: Vec<GuideFact>,
    pub traps: Vec<String>,
    /// Official sources by layer, including ones no tool wraps yet — fetch
    /// them directly when a tool is missing.
    pub sources: BTreeMap<String, GuideSource>,
    pub warnings: Vec<String>,
}

pub fn fetch(req: GuideRequest) -> Result<GuideResponse> {
    let code = normalize_state(&req.state);
    let entry = DATA.get(&code).ok_or_else(|| {
        Error::InvalidInput(format!(
            "no guide for {code:?}; use a two-letter state code or \"dc\""
        ))
    })?;
    let topic = req
        .topic
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase);
    let hit = |s: &str| {
        topic
            .as_deref()
            .is_none_or(|t| s.to_ascii_lowercase().contains(t))
    };

    let facts: Vec<GuideFact> = entry
        .facts
        .iter()
        .filter(|f| hit(&f.fact) || hit(&f.value) || hit(&f.citation))
        .cloned()
        .collect();
    let traps: Vec<String> = entry.traps.iter().filter(|t| hit(t)).cloned().collect();

    let low = facts.iter().filter(|f| f.confidence == "low").count();
    let mut warnings = vec![format!(
        "research seed from a {SWEPT} sweep, not an authority: read the cited section before relying on any \
         fact (legal_statute with this state, or the statutes source below). Deadlines and caps are amended often."
    )];
    if low > 0 {
        warnings.push(format!("{low} of these facts are low-confidence (single secondary source) — verify those first"));
    }
    if topic.is_some() && facts.is_empty() && traps.is_empty() {
        warnings.push(
            "nothing matched the topic; call without `topic` to see everything for this state"
                .into(),
        );
    }

    Ok(GuideResponse {
        generated_at: chrono::Utc::now(),
        state: code.clone(),
        tools: supported(&code).copied(),
        facts,
        traps,
        sources: entry.sources.clone(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_all_fifty_states_and_dc() {
        assert_eq!(DATA.len(), 51);
        assert!(DATA.contains_key("dc"));
        for (code, e) in DATA.iter() {
            assert_eq!(code.len(), 2, "{code}");
            assert!(
                e.sources.contains_key("statutes"),
                "{code} has no statutes source"
            );
            for f in &e.facts {
                assert!(
                    matches!(f.confidence.as_str(), "high" | "medium" | "low"),
                    "{code}: {}",
                    f.confidence
                );
            }
        }
    }

    #[test]
    fn topic_filters_facts() {
        let all = fetch(GuideRequest {
            state: "Alabama".into(),
            topic: None,
        })
        .unwrap();
        let notice = fetch(GuideRequest {
            state: "al".into(),
            topic: Some("notice".into()),
        })
        .unwrap();
        assert!(!notice.facts.is_empty() && notice.facts.len() < all.facts.len());
    }

    #[test]
    fn reports_which_tools_answer() {
        let fl = fetch(GuideRequest {
            state: "fl".into(),
            topic: None,
        })
        .unwrap();
        assert!(fl.tools.is_some_and(|t| t.statutes));
        let al = fetch(GuideRequest {
            state: "al".into(),
            topic: None,
        })
        .unwrap();
        assert!(al.tools.is_none());
    }
}
