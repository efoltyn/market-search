//! Negative-treatment signals — a free, rough first pass at "is this still
//! good law?".
//!
//! Westlaw's KeyCite flags and Lexis's Shepard's signals are editorial work
//! and copyrighted. This builds the same first question from primary sources:
//! for each cited case, find later opinions that both cite it and use the
//! language courts use when they take a precedent down (overruled, abrogated,
//! superseded, no longer good law...), and return each one with the passage
//! the search index highlights.
//!
//! It is a sorter, not a verdict. A hit means "a later opinion cites this case
//! and says one of these words somewhere" — the passage decides whether the
//! word is about *this* case. So every signal carries its passage and a label
//! saying how close the passage is to the case, and the report says plainly
//! what was not covered: direct appellate history (a reversal of this very
//! decision) lives in dockets, not in citing opinions.

use super::fetch::{name_tokens, network_case, opinion_ids_for_cite, CitationVerdict};
use crate::legal::courtlistener;
use serde::{Deserialize, Serialize};

/// Cases checked per call. Each costs two searches against an anonymous
/// budget of about a hundred a day on a shared address.
const MAX_CASES: usize = 12;
const MAX_SIGNALS: usize = 8;

const NEGATIVE_QUERY: &str = "(overruled OR overrule OR overruling OR abrogated OR abrogating OR \
     superseded OR \"no longer good law\" OR disapproved OR \"called into doubt\" OR repudiated)";

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TreatmentSignal {
    pub case_name: Option<String>,
    pub citation: Option<String>,
    pub court_id: Option<String>,
    pub date_filed: Option<String>,
    pub url: Option<String>,
    /// `overruled` | `abrogated` | `superseded` | `questioned` | `unclear`.
    pub label: String,
    /// Whether the treatment word in the passage is aimed at the cited case —
    /// "overruling Roe v. Wade", or "Roe v. Wade, 410 U.S. 113, overruled by" —
    /// rather than at some other authority the citing opinion mentions in the
    /// same breath ("(quoting Nken v. Holder), overruled on other grounds by").
    pub names_this_case: bool,
    pub passage: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TreatmentReport {
    pub citation: String,
    pub case_name: Option<String>,
    /// `negative_signals` | `no_negative_signals` | `unchecked`.
    pub status: String,
    /// Later opinions that cite this case and use negative-treatment language.
    pub flagged_total: Option<u64>,
    pub signals: Vec<TreatmentSignal>,
    pub note: String,
}

pub(crate) async fn check(
    verdicts: &[CitationVerdict],
    warnings: &mut Vec<String>,
) -> Vec<TreatmentReport> {
    let cases: Vec<&CitationVerdict> = verdicts
        .iter()
        .filter(|v| v.exists && !v.name_mismatch)
        .collect();
    if cases.len() > MAX_CASES {
        warnings.push(format!(
            "treatment checked the first {MAX_CASES} of {} cases (two searches each against a tight \
             anonymous budget); re-run on the rest",
            cases.len()
        ));
    }
    let mut out = Vec::new();
    for v in cases.into_iter().take(MAX_CASES) {
        out.push(one(v, warnings).await);
    }
    let flagged: Vec<&str> = out
        .iter()
        .filter(|r| {
            r.signals
                .iter()
                .any(|s| s.names_this_case && s.label != "unclear")
        })
        .map(|r| r.citation.as_str())
        .collect();
    if !flagged.is_empty() {
        warnings.push(format!(
            "negative treatment that names the case itself was found for: {} — read those passages \
             before relying on the case",
            flagged.join(", ")
        ));
    }
    warnings.push(
        "treatment is a first pass from citing opinions only: it does not see direct appellate \
         history (this decision itself reversed or vacated), statutory supersession nobody wrote \
         about, or anything outside CourtListener"
            .to_string(),
    );
    out
}

async fn one(v: &CitationVerdict, warnings: &mut Vec<String>) -> TreatmentReport {
    let cite = v.normalized.clone().unwrap_or_else(|| v.citation.clone());
    let mut r = TreatmentReport {
        citation: cite.clone(),
        case_name: v.case_name.clone(),
        ..Default::default()
    };
    let mut local = Vec::new();
    let (ids, _) = opinion_ids_for_cite(&cite, v.cluster_id, &mut local).await;
    if ids.is_empty() {
        r.status = "unchecked".into();
        r.note = format!(
            "could not find {cite} in the search index to ask who cites it{}",
            local.first().map(|w| format!(" ({w})")).unwrap_or_default()
        );
        return r;
    }
    let ids_q = ids
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(" OR ");
    let path = format!(
        "search/?q={}&type=o&order_by={}&highlight=on",
        urlencoding::encode(&format!("cites:({ids_q}) AND {NEGATIVE_QUERY}")),
        urlencoding::encode("dateFiled desc")
    );
    let Some(body) = courtlistener::get(&path, &mut local).await else {
        r.status = "unchecked".into();
        r.note = format!(
            "the treatment search did not complete — not asked, not clean{}",
            local.first().map(|w| format!(" ({w})")).unwrap_or_default()
        );
        warnings.extend(local.into_iter().take(1));
        return r;
    };
    r.flagged_total = body.get("count").and_then(|c| c.as_u64());
    let tokens: Vec<String> = v
        .case_name
        .iter()
        .chain(v.cited_as.iter())
        .flat_map(|n| name_tokens(n))
        .filter(|t| t.len() >= 4)
        .collect();
    if let Some(results) = body.get("results").and_then(|x| x.as_array()) {
        for hit in results.iter().take(MAX_SIGNALS) {
            let n = network_case(hit);
            let passage = snippet(hit);
            let lower = passage.as_deref().unwrap_or_default().to_lowercase();
            let names_this_case = aims_at(&lower, &tokens, &cite.to_lowercase());
            r.signals.push(TreatmentSignal {
                label: label(&lower).to_string(),
                names_this_case,
                passage,
                case_name: n.case_name,
                citation: n.citation,
                court_id: n.court_id,
                date_filed: n.date_filed,
                url: n.url,
            });
        }
    }
    let strong = r
        .signals
        .iter()
        .filter(|s| s.names_this_case && s.label != "unclear")
        .count();
    r.status = if r.signals.is_empty() {
        "no_negative_signals"
    } else {
        "negative_signals"
    }
    .into();
    r.note = match (r.signals.is_empty(), strong) {
        (true, _) => {
            "no later opinion citing this case uses negative-treatment language in the index".into()
        }
        (false, 0) => format!(
            "{} later opinion(s) cite this case and use negative-treatment language, but no \
             highlighted passage names this case — likely about other authority; skim to be sure",
            r.flagged_total.unwrap_or(r.signals.len() as u64)
        ),
        (false, k) => format!(
            "{k} passage(s) use negative-treatment language next to this case's name — read them \
             before relying on it"
        ),
    };
    r
}

static TREAT_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"overrul(?:ed|ing|e)|abrogat(?:ed|ing|ion)|supersed(?:ed|ing)|disapprov(?:ed|ing)|repudiat(?:ed|ing)|no longer good law|called into doubt",
    )
    .ok()
});

/// Is a treatment word in `p` aimed at this case (named by any of `names`, or
/// by its citation)? Two grammatical shapes count:
///   * active — the word, then the case within a short reach, with no new
///     clause between ("overruling Roe v. Wade");
///   * passive — the case, then the word, where everything between is the
///     case's own citation: parentheses balanced, so the word is not hanging
///     off an enclosing parenthetical about a different authority.
fn aims_at(p: &str, names: &[String], cite: &str) -> bool {
    let Some(re) = TREAT_RE.as_ref() else {
        return false;
    };
    let mut targets: Vec<usize> = Vec::new();
    for n in names
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(cite))
        .filter(|n| !n.is_empty())
    {
        targets.extend(p.match_indices(n).map(|(i, _)| i));
    }
    if targets.is_empty() {
        return false;
    }
    for m in re.find_iter(p) {
        // Active.
        let reach = floor(p, m.end() + 90);
        let after = &p[m.end()..reach];
        let stop = after.find([';', '(']).unwrap_or(after.len());
        if targets.iter().any(|&t| t >= m.end() && t < m.end() + stop) {
            return true;
        }
        // Passive.
        for &t in targets
            .iter()
            .filter(|&&t| t < m.start() && m.start() - t < 160)
        {
            let between = &p[t..m.start()];
            let open = between.matches('(').count();
            let close = between.matches(')').count();
            if open == close
                && !between.contains(';')
                && !between.contains("quoting")
                && !between.contains("citing")
            {
                return true;
            }
        }
    }
    false
}

fn floor(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The highlighted fragment(s) of a search hit, tags stripped.
fn snippet(hit: &serde_json::Value) -> Option<String> {
    let raw = hit
        .get("opinions")
        .and_then(|o| o.as_array())
        .and_then(|a| {
            a.iter().find_map(|op| {
                op.get("snippet")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.trim().is_empty())
            })
        })
        .or_else(|| hit.get("snippet").and_then(|s| s.as_str()))?;
    let text = crate::legal::strip_markup(raw);
    let text = html_escape::decode_html_entities(&text);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| text.chars().take(600).collect())
}

/// Read the treatment word out of the passage. Order matters: "overruled"
/// outranks "questioned" when a passage says both.
fn label(passage: &str) -> &'static str {
    if passage.contains("overrul")
        || passage.contains("no longer good law")
        || passage.contains("repudiat")
    {
        "overruled"
    } else if passage.contains("abrogat") {
        "abrogated"
    } else if passage.contains("supersed") {
        "superseded"
    } else if passage.contains("disapprov")
        || passage.contains("called into doubt")
        || passage.contains("question")
    {
        "questioned"
    } else {
        "unclear"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_the_strongest_treatment_word() {
        assert_eq!(
            label("we now hold that roe and casey are overruled"),
            "overruled"
        );
        assert_eq!(label("abrogated on other grounds by"), "abrogated");
        assert_eq!(label("superseded by statute as stated in"), "superseded");
        assert_eq!(
            label("its reasoning has been called into doubt"),
            "questioned"
        );
        assert_eq!(label("the court cited it approvingly"), "unclear");
    }

    #[test]
    fn treatment_must_be_aimed_at_the_case_itself() {
        let roe = vec!["roe".to_string(), "wade".to_string()];
        assert!(aims_at(
            "jackson women's health org., 597 u.s. 215 (2022), overruling roe v. wade, 410 u.s. 113 (1973)",
            &roe,
            "410 u.s. 113"
        ));
        assert!(aims_at(
            "roe v. wade, 410 u.s. 113 (1973), overruled by dobbs",
            &roe,
            "410 u.s. 113"
        ));
        // The live false positive: the word hangs off the outer citation.
        let nken = vec!["nken".to_string(), "holder".to_string()];
        assert!(!aims_at(
            "(quoting nken v. holder, 556 u.s. 418, 435 (2009)), overruled on other grounds by ariz. all.",
            &nken,
            "556 u.s. 418"
        ));
        assert!(!aims_at(
            "casey, overruled by dobbs; see also roe v. wade",
            &roe,
            "410 u.s. 113"
        ));
    }

    #[test]
    fn reads_the_highlighted_snippet() {
        let hit = serde_json::json!({
            "opinions": [{"id": 1, "snippet": "We hold that <mark>Roe</mark> and Casey must be <mark>overruled</mark>"}]
        });
        assert_eq!(
            snippet(&hit).as_deref(),
            Some("We hold that Roe and Casey must be overruled")
        );
        assert_eq!(
            snippet(&serde_json::json!({"opinions": [{"snippet": ""}]})),
            None
        );
    }
}
