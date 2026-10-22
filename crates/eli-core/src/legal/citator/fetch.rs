//! Citation verification and the citation network — the anti-hallucination tool.
//!
//! Why this exists when case law is on the open web: a search engine will
//! happily surface a page that *contains* a made-up citation, and a language
//! model will happily produce one. Nothing on the open web answers the actual
//! question — *does `999 U.S. 999` name a real decision, and is it the decision
//! you said it was?* This module does, three ways:
//!   * **Existence.** CourtListener's `/c/<reporter>/<volume>/<page>/` resolver
//!     302s to a real opinion and 404s on a reporter page that holds nothing.
//!     No key, and it is outside the API throttle, which matters because
//!     checking a whole brief is exactly the high-volume case.
//!   * **Attribution.** The 302 target carries the real case name, which is
//!     checked against the name the passage attached to the cite. A *real*
//!     reporter page wearing an *invented* case name is the failure mode that
//!     survives every existence check — `612 F.3d 1099` is a genuine page, but
//!     it is *United States v. Maciel-Alcala*, not "Whitfield v. Marshall" —
//!     and it is the one that survives a spell-check and kills a filing.
//!   * **Ambiguity and normalisation.** With a token, `/citation-lookup/` adds
//!     eyecite's parser: `576 US 644` normalises to `576 U.S. 644`, and
//!     `1 H. 150` comes back three-ways ambiguous rather than confidently wrong.
//!
//! Plus the citation graph — who has relied on a case since (`cited_by`) and
//! what it rested on (`authorities`) — which is Shepard's/KeyCite-shaped data
//! that no consumer search engine exposes.
//!
//! The honest limit, stated in every negative verdict: a 404 means *this
//! corpus* has nothing at that reporter page. That is strong evidence of
//! fabrication, not proof. Unpublished, sealed, very recent, and never-digitised
//! decisions exist outside it.

use crate::legal::courtlistener::{tidy_case_name, self, CL_WEB};
use crate::legal::{keys, shared_client, strip_markup};
use super::quotes::{QuoteCheck, QuoteSummary, ShortCite};
use super::treatment::TreatmentReport;
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::Duration;

/// Ceiling on citations verified from one blob of `--text`. The key-free
/// resolver sits outside the API throttle, so this is sized for a whole brief
/// (a long appellate brief runs 100-150 distinct cites); past it we say so
/// rather than silently truncating.
const MAX_TEXT_CITES: usize = 200;

/// Spacing between key-free resolver calls. Thirty citations at once would
/// otherwise arrive as a burst and read as abuse from a shared egress address.
const RESOLVER_DELAY: Duration = Duration::from_millis(150);

/// The wording every negative verdict carries. Kept in one place because the
/// exact scope of the claim is the whole point: absence from this corpus is
/// evidence, not proof.
const NOT_FOUND_NOTE: &str = "CourtListener has no case at this reporter page. That is strong \
    evidence the citation is fabricated or mis-transcribed, but it is not proof — unpublished \
    dispositions, sealed cases, very recent decisions and never-digitised reporters all exist \
    outside this corpus. Check the reporter, volume and page before concluding the cite is fake.";

#[derive(Clone, Debug)]
pub struct CitationRequest {
    pub text: Option<String>,
    pub cite: Option<String>,
    pub opinion_id: Option<u64>,
    pub cited_by: bool,
    pub authorities: bool,
    pub limit: usize,
    /// Check every quotation in `text` against the opinion it is attributed
    /// to, including the pin cite. On by default for text: a fabricated quote
    /// from a real case is the failure that survives every citation check.
    pub quotes: bool,
    /// Look for later opinions that cite each case alongside negative-treatment
    /// language (overruled, abrogated, superseded...). Spends search requests
    /// against a tight anonymous budget, so it is opt-in.
    pub treatment: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CitationVerdict {
    pub citation: String,
    pub normalized: Option<String>,
    pub exists: bool,
    /// HTTP-shaped, matching `/citation-lookup/`'s own per-citation vocabulary:
    /// 200 found, 404 no such case, 400 unrecognised reporter, 300 ambiguous,
    /// 429 not checked (throttled or over the batch cap), 0 unreachable.
    pub status: u16,
    /// The resolved, authoritative case name.
    pub case_name: Option<String>,
    /// The case name the *source text* attached to this cite, when it gave one.
    /// Kept separately from `case_name` so the two can be compared rather than
    /// one silently overwriting the other.
    pub cited_as: Option<String>,
    /// The reporter page is real but names a different decision than the text
    /// claimed. Existence alone would have passed this citation.
    pub name_mismatch: bool,
    /// The text named this case and the name matches the reporter page. A
    /// citation that exists but whose name could not be compared is NOT
    /// confirmed — it is counted under `unconfirmed`, never `verified`.
    pub name_confirmed: bool,
    pub court: Option<String>,
    pub date_filed: Option<String>,
    pub cluster_id: Option<u64>,
    pub url: Option<String>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct NetworkCase {
    pub case_name: Option<String>,
    pub citation: Option<String>,
    pub court_id: Option<String>,
    pub date_filed: Option<String>,
    /// How many times the citing opinion references the cited one. A depth of 4
    /// is load-bearing authority; a depth of 1 is often a string cite. Only the
    /// token path reports it.
    pub depth: Option<u32>,
    pub opinion_id: Option<u64>,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CitationResponse {
    pub generated_at: DateTime<Utc>,
    pub checked: usize,
    /// Exists *and* the case name matches. Only these are clean.
    pub verified: usize,
    /// Exists, but under a different case name than the text gave it. Counted
    /// apart from `verified` so a caller reading only the totals cannot
    /// conclude a misattributed brief checks out.
    pub misattributed: usize,
    /// Exists, but the text gave no case name we could compare (a bare cite,
    /// or a caption the parser could not read). Not evidence of a problem —
    /// but not a clean bill either, so it is never folded into `verified`.
    pub unconfirmed: usize,
    /// Checked against the resolver and genuinely not found there.
    pub unverified: usize,
    /// Never reached the resolver — transport failure or throttle. Unknown,
    /// not evidence about the citation. Kept apart from `unverified` because a
    /// network hiccup must never read as "this lawyer invented a case".
    pub unchecked: usize,
    pub citations: Vec<CitationVerdict>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cited_by: Vec<NetworkCase>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authorities: Vec<NetworkCase>,
    pub cited_by_total: Option<u64>,
    /// Every quotation in the text, checked against the opinion it is
    /// attributed to. Empty unless `quotes` ran on a text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quotes: Vec<QuoteCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_summary: Option<QuoteSummary>,
    /// Short-form cites ("525 U.S. at 161") and whether each resolves to a
    /// full citation earlier in the text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub short_cites: Vec<ShortCite>,
    /// Negative-treatment signals per case, when `treatment` was requested.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub treatment: Vec<TreatmentReport>,
    pub warnings: Vec<String>,
}

pub async fn fetch_citations(req: CitationRequest) -> Result<CitationResponse> {
    let mut out = CitationResponse {
        generated_at: Utc::now(),
        checked: 0,
        verified: 0,
        misattributed: 0,
        unconfirmed: 0,
        unverified: 0,
        unchecked: 0,
        citations: Vec::new(),
        cited_by: Vec::new(),
        authorities: Vec::new(),
        cited_by_total: None,
        quotes: Vec::new(),
        quote_summary: None,
        short_cites: Vec::new(),
        treatment: Vec::new(),
        warnings: Vec::new(),
    };

    let text = req.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let single = req.cite.as_deref().map(str::trim).filter(|c| !c.is_empty());
    if text.is_none() && single.is_none() && req.opinion_id.is_none() {
        return Err(Error::InvalidInput(
            "legal cite requires --text (scan a passage), --cite (verify one citation), or --id \
             (walk the citation network)"
                .into(),
        ));
    }

    // What to verify. A single --cite is taken verbatim so the caller sees the
    // verdict on the string they actually wrote, typos included — but if they
    // pasted the whole "Name v. Name, 1 U.S. 1" form, keep the name so the
    // attribution check has something to compare against.
    let mut targets: Vec<ExtractedCite> = Vec::new();
    if let Some(c) = single {
        let mut parsed = extract_citations(c);
        if parsed.len() == 1 {
            targets.push(parsed.remove(0));
        } else {
            targets.push(ExtractedCite {
                citation: c.to_string(),
                cited_as: None,
                name_hint: false,
            });
        }
    }
    if let Some(t) = text {
        if t.chars().count() > 64_000 {
            out.warnings.push(
                "--text is longer than the 64,000-character limit the citation API accepts; only \
                 the citations found in the leading portion were checked"
                    .to_string(),
            );
        }
        let found = extract_citations(t);
        if found.is_empty() {
            out.warnings.push(
                "no reporter citations found in --text. This finds the \"<volume> <reporter> \
                 <page>\" shape only — it does not resolve id., supra, statutes or law-journal \
                 cites."
                    .to_string(),
            );
        }
        if found.len() > MAX_TEXT_CITES {
            out.warnings.push(format!(
                "found {} citations; verifying the first {MAX_TEXT_CITES}. Split the passage and \
                 re-run to check the rest.",
                found.len()
            ));
        }
        for c in found.into_iter().take(MAX_TEXT_CITES) {
            if !targets
                .iter()
                .any(|t| compare_key(&t.citation) == compare_key(&c.citation))
            {
                targets.push(c);
            }
        }
    }

    if !targets.is_empty() {
        out.citations = verify(&targets, text, &mut out.warnings).await;
    }
    // Database and neutral cites the free corpus cannot check. Listed, not
    // dropped: a Westlaw cite to an invented case is exactly what Mata v.
    // Avianca was full of, and silence would read as "nothing to see".
    if let Some(t) = text {
        for u in extract_unverifiable(t) {
            out.citations.push(u);
        }
    }
    tally(&mut out);

    if let (Some(t), true) = (text, req.quotes) {
        let (quotes, summary, shorts) = super::quotes::check(t, &out.citations, &mut out.warnings).await;
        out.quotes = quotes;
        out.quote_summary = summary;
        out.short_cites = shorts;
    }
    if req.treatment {
        out.treatment = super::treatment::check(&out.citations, &mut out.warnings).await;
    }

    if req.cited_by || req.authorities {
        network(&req, &mut out).await;
    }
    Ok(out)
}

/// Sort every verdict into exactly one bucket. The buckets are what a caller
/// reads to decide a brief is clean, so each rule errs toward "look at this".
fn tally(out: &mut CitationResponse) {
    out.checked = out.citations.len();
    out.verified = 0;
    out.misattributed = 0;
    out.unconfirmed = 0;
    out.unverified = 0;
    out.unchecked = 0;
    for v in &out.citations {
        match bucket(v) {
            Bucket::Verified => out.verified += 1,
            Bucket::Misattributed => out.misattributed += 1,
            Bucket::Unconfirmed => out.unconfirmed += 1,
            Bucket::Unverified => out.unverified += 1,
            Bucket::Unchecked => out.unchecked += 1,
        }
    }
    if out.misattributed > 0 {
        out.warnings.push(format!(
            "{} citation(s) point at a real reporter page but name a different case than the \
             text does. Those are counted under `misattributed`, not `verified`.",
            out.misattributed
        ));
    }
    if out.unconfirmed > 0 {
        out.warnings.push(format!(
            "{} citation(s) exist but carry no case name we could compare, so they are \
             `unconfirmed`: read each one's resolved case_name against the brief.",
            out.unconfirmed
        ));
    }
    if out.unchecked > 0 {
        out.warnings.push(format!(
            "{} citation(s) could not be checked (database cites, neutral cites, or an \
             unreachable resolver) — unknown, not clean.",
            out.unchecked
        ));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Bucket {
    Verified,
    Misattributed,
    Unconfirmed,
    Unverified,
    Unchecked,
}

pub(crate) fn bucket(v: &CitationVerdict) -> Bucket {
    if v.name_mismatch {
        Bucket::Misattributed
    } else if v.exists && v.name_confirmed {
        Bucket::Verified
    } else if v.exists {
        Bucket::Unconfirmed
    } else if matches!(v.status, 404 | 400) {
        // 404: no case at that page. 400: an unrecognised reporter, which is
        // itself evidence of invention. Both are findings about the cite.
        Bucket::Unverified
    } else {
        // 0 unreachable, 429 throttled, 415 unverifiable format: no finding.
        Bucket::Unchecked
    }
}

/// `2019 WL 4639462`, `2015 U.S. Dist. LEXIS 1234`, `2013 IL App (1st)
/// 111279-U`, `2019-Ohio-1234`: real citation forms the free case-law corpus
/// does not index, so the resolver can neither confirm nor refute them.
static UNVERIFIABLE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:(?:19|20)\d{2}\s+WL\s+\d{3,9}|(?:19|20)\d{2}\s+(?:[A-Z][A-Za-z.]*\s+){1,3}LEXIS\s+\d{1,9}|(?:19|20)\d{2}\s+IL\s+App\s+\(\d+(?:st|nd|rd|th)\)\s+\d{4,7}(?:-[UB])?|(?:19|20)\d{2}\s+IL\s+\d{4,7}(?:-[UB])?|(?:19|20)\d{2}-Ohio-\d{1,6})\b",
    )
    .ok()
});

fn extract_unverifiable(text: &str) -> Vec<CitationVerdict> {
    let flat = flatten(text);
    let Some(re) = UNVERIFIABLE_RE.as_ref() else {
        return Vec::new();
    };
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for m in re.find_iter(&flat) {
        let cite = m.as_str().split_whitespace().collect::<Vec<_>>().join(" ");
        if seen.contains(&compare_key(&cite)) {
            continue;
        }
        seen.push(compare_key(&cite));
        let kind = if cite.contains(" WL ") {
            "a Westlaw database cite"
        } else if cite.contains("LEXIS") {
            "a Lexis database cite"
        } else {
            "a state public-domain (neutral) cite"
        };
        out.push(CitationVerdict {
            note: Some(format!(
                "NOT CHECKED: {kind}, which the free case-law corpus does not index, so it can be \
                 neither confirmed nor refuted here. Fabricated briefs lean on exactly these \
                 (Mata v. Avianca's invented cases were cited this way). Find the decision by \
                 name with legal_search, or confirm it on Westlaw/Lexis before filing."
            )),
            cited_as: preceding_case_name(&flat[..m.start()]),
            citation: cite,
            status: 415,
            ..Default::default()
        });
    }
    out
}

pub(crate) fn flatten(text: &str) -> String {
    if text.contains('<') {
        strip_markup(text)
    } else {
        text.to_string()
    }
}

// ── verification ───────────────────────────────────────────────────────────

/// Verify a batch: the token-only `/citation-lookup/` first (authoritative case
/// names, normalisation, ambiguity detection, parallel cites), then the key-free
/// resolver for anything it did not cover. The resolver alone is a complete
/// answer — the token is an upgrade, never a prerequisite.
async fn verify(
    targets: &[ExtractedCite],
    text: Option<&str>,
    warnings: &mut Vec<String>,
) -> Vec<CitationVerdict> {
    let mut verdicts: Vec<CitationVerdict> = Vec::new();

    if courtlistener::has_token() {
        // Posting the original passage rather than the extracted strings lets
        // eyecite do its own parsing, which catches forms this module's regex
        // deliberately does not chase.
        let payload = text.map(str::to_string).unwrap_or_else(|| {
            targets
                .iter()
                .map(|t| t.citation.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        });
        if let Some(results) = citation_lookup(&payload, warnings).await {
            for r in results {
                verdicts.push(verdict_from_lookup(&r));
            }
        }
    } else {
        warnings.push(
            "no COURTLISTENER_TOKEN: verified with the key-free /c/ resolver. A free token \
             (courtlistener.com/profile/api-token/) adds the /citation-lookup/ endpoint, which \
             returns the authoritative case name, normalises non-standard forms, and flags \
             ambiguous reporter abbreviations."
                .to_string(),
        );
    }

    // Anything the lookup did not answer for — every citation when there is no
    // token — goes through the resolver, one at a time with a pause.
    let mut first = verdicts.is_empty();
    for target in targets {
        if verdicts
            .iter()
            .any(|v| matches_citation(v, &target.citation))
        {
            continue;
        }
        if !first {
            tokio::time::sleep(RESOLVER_DELAY).await;
        }
        first = false;
        verdicts.push(resolve_one(&target.citation).await);
    }

    // Attribution pass, last so it covers verdicts from both paths: carry the
    // name the text used onto the verdict, and compare it with the name the
    // reporter page actually belongs to.
    for v in verdicts.iter_mut() {
        let Some(t) = targets
            .iter()
            .find(|t| matches_citation(v, &t.citation))
            .or_else(|| targets.iter().find(|t| compare_key(&t.citation) == compare_key(&v.citation)))
        else {
            continue;
        };
        v.cited_as = t.cited_as.clone();
        apply_attribution(v);
        if v.exists && v.cited_as.is_none() {
            let real = v.case_name.as_deref().unwrap_or("the resolved case");
            v.note = Some(if t.name_hint {
                format!(
                    "UNCONFIRMED: the reporter page exists and is {real}. A case name appears \
                     before this citation in the text but could not be parsed, so it was not \
                     compared — check by hand that the brief names {real}."
                )
            } else {
                format!(
                    "UNCONFIRMED: the reporter page exists and is {real}, but no case name was \
                     attached to this citation, so attribution was not checked."
                )
            });
        }
    }
    verdicts
}

/// Compare the name the passage gave a cite with the name the reporter page
/// actually carries, and rewrite the verdict when they disagree.
fn apply_attribution(v: &mut CitationVerdict) {
    let (Some(cited_as), Some(resolved)) = (v.cited_as.as_deref(), v.case_name.as_deref()) else {
        // No name in the text, or no resolved name: there is nothing to compare,
        // and guessing a mismatch here would be worse than staying quiet.
        return;
    };
    if !v.exists {
        return;
    }
    if names_agree(cited_as, resolved) {
        v.name_confirmed = true;
        // Confirmed on both axes — say so, rather than leaving the generic
        // "check the name yourself" caveat on a cite we already checked.
        v.note = Some(format!(
            "verified: the reporter page exists and resolves to {resolved}, which matches the \
             case name used in the text."
        ));
        return;
    }
    v.name_mismatch = true;
    v.note = Some(format!(
        "MISATTRIBUTED: {} is a real reporter page, but it is {resolved} — not \"{cited_as}\" as \
         the text has it. The citation as written is wrong: either the case name was invented and \
         bolted onto a real cite, or the volume/reporter/page belongs to a different decision. \
         Existence alone would have passed this cite; the names are what caught it.",
        v.normalized.as_deref().unwrap_or(&v.citation)
    ));
}

/// Key-free verification of one citation string.
async fn resolve_one(raw: &str) -> CitationVerdict {
    let mut v = CitationVerdict {
        citation: raw.to_string(),
        ..Default::default()
    };
    let Some(parsed) = courtlistener::parse_cite(raw) else {
        v.status = 400;
        v.note = Some(
            "could not be read as \"<volume> <reporter> <page>\", so it was not checked against \
             any corpus"
                .to_string(),
        );
        return v;
    };
    v.normalized = Some(format!(
        "{} {} {}",
        parsed.volume, parsed.reporter, parsed.page
    ));

    let (status, location) = courtlistener::resolve_citation(&parsed).await;
    match status {
        301..=308 => {
            let loc = location.unwrap_or_default();
            let (id, name) = courtlistener::parse_opinion_url(&loc);
            v.exists = true;
            v.status = 200;
            // The slug title-cases every word, so "v" arrives as "V" and
            // particles as "Of"/"The". Render it the way a brief would.
            let name = name.as_deref().map(tidy_case_name);
            // The redirect lands on the *cluster* page: web URLs on
            // CourtListener are cluster-scoped, and cluster ids and opinion ids
            // are different id spaces.
            v.cluster_id = id;
            v.case_name = name;
            v.url = Some(absolute(&loc));
            v.note = Some(
                "the case name here is read from the URL slug of the page this citation resolves \
                 to. Confirm it matches the case name in your text — a citation that exists but \
                 names a different decision is the error this check is for."
                    .to_string(),
            );
        }
        404 => {
            v.status = 404;
            v.note = Some(NOT_FOUND_NOTE.to_string());
        }
        0 => {
            v.status = 0;
            v.note =
                Some("the citation resolver was unreachable; this cite was NOT checked".to_string());
        }
        429 => {
            v.status = 429;
            v.note = Some(
                "rate limited before this cite could be checked — treat it as unverified, not as \
                 verified or fake"
                    .to_string(),
            );
        }
        other => {
            v.status = other;
            v.note = Some(format!(
                "the citation resolver returned HTTP {other}; this cite was NOT checked"
            ));
        }
    }
    v
}

/// `POST /citation-lookup/` — token only. Form-encoded, and the response is a
/// bare array with one object per citation eyecite found.
async fn citation_lookup(text: &str, warnings: &mut Vec<String>) -> Option<Vec<serde_json::Value>> {
    let token = keys::courtlistener()?;
    // The endpoint parses at most 250 citations per request and blocks payloads
    // over 64,000 characters outright.
    let payload: String = text.chars().take(64_000).collect();
    let resp = shared_client::GENERAL
        .post(format!("{}/citation-lookup/", courtlistener::CL_BASE))
        .header("Authorization", format!("Token {token}"))
        .header("Accept", "application/json")
        .form(&[("text", payload.as_str())])
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            warnings.push(format!("citation-lookup request failed: {e}"));
            return None;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        warnings.push(match status.as_u16() {
            // The 60/min budget on this endpoint counts *citations*, not
            // requests, and the refusal names the time the budget frees up.
            429 => format!(
                "citation-lookup rate limited (60 valid citations/min); falling back to the \
                 key-free resolver ({snippet})"
            ),
            _ => format!("citation-lookup {status}; falling back to the key-free resolver ({snippet})"),
        });
        return None;
    }
    match resp.json::<Vec<serde_json::Value>>().await {
        Ok(v) => Some(v),
        Err(e) => {
            warnings.push(format!("citation-lookup parse failed: {e}"));
            None
        }
    }
}

/// Map one `/citation-lookup/` result object onto a verdict.
fn verdict_from_lookup(r: &serde_json::Value) -> CitationVerdict {
    let citation = r
        .get("citation")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let normalized: Vec<String> = r
        .get("normalized_citations")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let status = r.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    let cluster = r
        .get("clusters")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first());

    let mut v = CitationVerdict {
        citation,
        normalized: normalized.first().cloned(),
        exists: status == 200 || status == 300,
        status,
        ..Default::default()
    };
    if let Some(c) = cluster {
        let s = |k: &str| {
            c.get(k)
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        };
        v.case_name = s("case_name").or_else(|| s("case_name_full"));
        v.date_filed = s("date_filed");
        v.cluster_id = c.get("id").and_then(|x| x.as_u64());
        v.url = s("absolute_url").map(|p| absolute(&p));
    }

    v.note = match status {
        200 => None,
        404 => Some(NOT_FOUND_NOTE.to_string()),
        400 => Some(format!(
            "the reporter abbreviation was not recognised{}. Check the abbreviation against a \
             citation guide; an unrecognised reporter is often an invented one.",
            r.get("error_message")
                .and_then(|x| x.as_str())
                .filter(|m| !m.is_empty())
                .map(|m| format!(" ({m})"))
                .unwrap_or_default()
        )),
        300 => Some(format!(
            "ambiguous — this abbreviation matches more than one reporter. Candidates: {}. \
             Disambiguate before relying on it.",
            normalized.join(", ")
        )),
        429 => Some(
            "beyond the 250-citations-per-request cap: parsed but NOT looked up. Re-run on a \
             shorter passage to check it."
                .to_string(),
        ),
        other => Some(format!("citation-lookup reported status {other}")),
    };
    v
}

/// Did this verdict already answer for `target`? Compares on the normalised
/// digits-and-letters form so "576 US 644" and "576 U.S. 644" are one citation.
fn matches_citation(v: &CitationVerdict, target: &str) -> bool {
    let key = compare_key(target);
    compare_key(&v.citation) == key
        || v.normalized.as_deref().map(compare_key) == Some(key.clone())
}

pub(crate) fn compare_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

// ── citation network ───────────────────────────────────────────────────────

/// Walk the graph around one opinion. `cited_by` (who relies on it since) is
/// available key-free through the search index; `authorities` (what it rested
/// on) is only ids without a token, and we say so rather than returning nothing.
async fn network(req: &CitationRequest, out: &mut CitationResponse) {
    let limit = req.limit.clamp(1, 100);

    // The network is keyed by *opinion* id, and one decision is usually several
    // opinions: the resolver's 302 lands on a *cluster* (majority plus every
    // concurrence and dissent), and later courts cite whichever sub-opinion
    // they relied on. Asking about one sub-opinion id would under-report or,
    // for a case whose lead opinion is not first in the list, report zero. So
    // we collect every opinion id in the cluster and ask about all of them.
    let (opinion_ids, cites_from_hit) = match req.opinion_id {
        Some(id) => (vec![id], Vec::new()),
        None => {
            let anchor = out.citations.iter().find(|c| c.exists);
            match anchor {
                Some(c) => {
                    let cite = c.normalized.clone().unwrap_or_else(|| c.citation.clone());
                    opinion_ids_for_cite(&cite, c.cluster_id, &mut out.warnings).await
                }
                None => (Vec::new(), Vec::new()),
            }
        }
    };

    if opinion_ids.is_empty() {
        out.warnings.push(
            "citation network skipped: no opinion id. Pass --id, or a --cite that resolves to a \
             case in the search index. (Note the number in a CourtListener /opinion/<n>/ URL is a \
             *cluster* id; the network is keyed by opinion id.)"
                .to_string(),
        );
        return;
    }

    if req.cited_by {
        cited_by(&opinion_ids, limit, out).await;
    }
    if req.authorities {
        authorities(opinion_ids[0], limit, cites_from_hit, out).await;
    }
}

/// Resolve a citation to every opinion id in its cluster (and the lead
/// opinion's table of authorities as bare ids) with one anonymous search call.
///
/// `sibling_ids` on a `type=o` hit lists the whole cluster, which is what makes
/// this reliable: `opinions[]` holds only the sub-documents that matched the
/// query, so a query that matched a dissent would otherwise point the whole
/// network walk at the dissent.
pub(crate) async fn opinion_ids_for_cite(
    cite: &str,
    expected_cluster: Option<u64>,
    warnings: &mut Vec<String>,
) -> (Vec<u64>, Vec<u64>) {
    // Fielded, not a free-text phrase: searching for "597 U.S. 1" as words
    // ranks the hundreds of later decisions that *quote* Bruen above Bruen
    // itself, and walking the network from one of those would report a leading
    // precedent as barely cited. Every hit is checked again below.
    let path = format!(
        "search/?q={}&type=o",
        urlencoding::encode(&format!("citation:(\"{cite}\")"))
    );
    let Some(v) = courtlistener::get(&path, warnings).await else {
        return (Vec::new(), Vec::new());
    };
    let results = v.get("results").and_then(|r| r.as_array());
    let Some(hit) = results.and_then(|a| {
        a.iter()
            .find(|h| hit_is_the_cited_case(h, cite, expected_cluster))
    }) else {
        warnings.push(format!(
            "could not identify the opinion published at {cite} in the search index, so the \
             citation network was not walked. Pass --id with a CourtListener opinion id to walk \
             it directly."
        ));
        return (Vec::new(), Vec::new());
    };
    let (ids, cites) = opinion_ids_of_hit(hit);
    if ids.is_empty() {
        warnings.push(format!(
            "found the case at {cite} but the search hit carried no opinion ids, so the citation \
             network could not be walked"
        ));
    }
    (ids, cites)
}

/// Is this hit the decision published at `wanted`, or one that merely cites it?
fn hit_is_the_cited_case(
    hit: &serde_json::Value,
    wanted: &str,
    expected_cluster: Option<u64>,
) -> bool {
    if let Some(expected) = expected_cluster {
        if hit.get("cluster_id").and_then(|c| c.as_u64()) == Some(expected) {
            return true;
        }
    }
    let key = compare_key(wanted);
    hit.get("citation")
        .and_then(|c| c.as_array())
        .is_some_and(|arr| {
            arr.iter()
                .filter_map(|c| c.as_str())
                .any(|c| compare_key(c) == key)
        })
}

/// Every opinion id a `type=o` search hit names, lead opinion first, plus that
/// opinion's `cites[]`.
fn opinion_ids_of_hit(hit: &serde_json::Value) -> (Vec<u64>, Vec<u64>) {
    let mut ids: Vec<u64> = Vec::new();
    let mut cites: Vec<u64> = Vec::new();
    if let Some(ops) = hit.get("opinions").and_then(|o| o.as_array()) {
        for op in ops {
            if let Some(id) = op.get("id").and_then(|x| x.as_u64()) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            if cites.is_empty() {
                if let Some(arr) = op.get("cites").and_then(|c| c.as_array()) {
                    cites = arr.iter().filter_map(|x| x.as_u64()).collect();
                }
            }
        }
    }
    if let Some(sibs) = hit.get("sibling_ids").and_then(|s| s.as_array()) {
        for id in sibs.iter().filter_map(|x| x.as_u64()) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    (ids, cites)
}

/// Forward citations. `q=cites:(<opinion_id>)` is a fielded query over the same
/// anonymous search index, so this is the one half of the graph that comes back
/// with real case names without a token.
async fn cited_by(opinion_ids: &[u64], limit: usize, out: &mut CitationResponse) {
    let ids = opinion_ids
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut path = format!(
        "search/?q={}&type=o&order_by={}",
        urlencoding::encode(&format!("cites:({ids})")),
        urlencoding::encode("citeCount desc")
    );
    if limit > 20 {
        path.push_str(&format!("&page_size={limit}"));
    }

    // `answered` and not `count.is_some()`: a throttled or rejected request
    // must leave `cited_by_total` null. Reporting 0 there would turn "we could
    // not ask" into "nothing cites this case", which for a leading precedent is
    // a confidently wrong answer rather than a thin one.
    let answered = match courtlistener::get(&path, &mut out.warnings).await {
        Some(v) => {
            out.cited_by_total = v.get("count").and_then(|c| c.as_u64());
            if let Some(results) = v.get("results").and_then(|r| r.as_array()) {
                for raw in results.iter().take(limit) {
                    out.cited_by.push(network_case(raw));
                }
            }
            true
        }
        None => false,
    };

    // With a token the join table gives an exact per-opinion figure. Only worth
    // spending a request when the search count is missing, and only meaningful
    // for a single opinion — the search count is per-cluster, which is the
    // number a reader actually wants for "who cites this case".
    if out.cited_by_total.is_none() && courtlistener::has_token() && opinion_ids.len() == 1 {
        if let Some(v) = courtlistener::get(
            &format!("opinions-cited/?cited_opinion={}&count=on", opinion_ids[0]),
            &mut out.warnings,
        )
        .await
        {
            out.cited_by_total = v.get("count").and_then(|c| c.as_u64());
        }
    }

    if !answered {
        out.warnings.push(format!(
            "the forward-citation query for opinion(s) {ids} did not complete, so cited_by_total \
             is null rather than 0 — this is \"not asked\", not \"not cited\". Retry when the \
             rate limit clears."
        ));
    } else if out.cited_by.is_empty() && out.cited_by_total.unwrap_or(0) == 0 {
        out.warnings.push(format!(
            "the search index reports nothing citing opinion(s) {ids}. Either the case has not \
             been cited, or the `cites:` field query did not match the ids CourtListener indexes \
             for it — cross-check citeCount on the case's CourtListener page before reporting it \
             as uncited."
        ));
    }
}

/// Backward citations. The join table is token-gated and, either way, both
/// paths yield opinion *ids*: resolving each to a case name would be one
/// request per authority, which the 5/min throttle makes impossible.
async fn authorities(
    opinion_id: u64,
    limit: usize,
    from_search: Vec<u64>,
    out: &mut CitationResponse,
) {
    if courtlistener::has_token() {
        let path = format!("opinions-cited/?citing_opinion={opinion_id}&page_size={limit}");
        if let Some(v) = courtlistener::get(&path, &mut out.warnings).await {
            if let Some(results) = v.get("results").and_then(|r| r.as_array()) {
                for raw in results.iter().take(limit) {
                    let cited = raw
                        .get("cited_opinion")
                        .and_then(|x| x.as_str())
                        .and_then(last_path_id);
                    out.authorities.push(NetworkCase {
                        depth: raw.get("depth").and_then(|d| d.as_u64()).map(|d| d as u32),
                        opinion_id: cited,
                        url: cited.map(|id| format!("{CL_WEB}/opinion/{id}/")),
                        ..Default::default()
                    });
                }
            }
        }
    }

    if out.authorities.is_empty() {
        for id in from_search.into_iter().take(limit) {
            out.authorities.push(NetworkCase {
                opinion_id: Some(id),
                url: Some(format!("{CL_WEB}/opinion/{id}/")),
                ..Default::default()
            });
        }
    }

    if out.authorities.is_empty() {
        out.warnings.push(format!(
            "no table of authorities for opinion {opinion_id}: /opinions-cited/ is 401 without a \
             token, and the search index returned no `cites` array for this opinion. Set \
             COURTLISTENER_TOKEN to read it."
        ));
    } else if out.authorities.iter().all(|a| a.case_name.is_none()) {
        out.warnings.push(
            "authorities are reported as opinion ids only. Both the join table and the search \
             index give ids, and naming each one costs a request against a 5/min budget — follow \
             the urls, or use the quarterly bulk citation-graph export for real network analysis."
                .to_string(),
        );
    }
}

pub(crate) fn network_case(raw: &serde_json::Value) -> NetworkCase {
    let s = |k: &str| {
        raw.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    NetworkCase {
        case_name: s("caseName").or_else(|| s("caseNameFull")),
        citation: raw
            .get("citation")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.as_str())
            .map(str::to_string),
        court_id: s("court_id"),
        date_filed: s("dateFiled"),
        depth: None,
        opinion_id: raw
            .get("opinions")
            .and_then(|o| o.as_array())
            .and_then(|a| a.first())
            .and_then(|o| o.get("id"))
            .and_then(|x| x.as_u64()),
        url: s("absolute_url").map(|p| absolute(&p)),
    }
}

// ── citation extraction ────────────────────────────────────────────────────

/// The `<volume> <reporter> <page>` shape.
///
/// A reporter token is either capitalised (`U.S.`, `F.`, `Supp.`, `Cal.`) or a
/// bare ordinal (`2d`, `3d`, `4th`) — the ordinal alternative is what lets
/// `F. Supp. 2d` and `Cal. App. 4th` match at all. Volume and page are bounded
/// so a year or a phone number cannot pose as one.
pub(crate) static CITE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?P<vol>\b\d{1,4})\s+(?P<rep>(?:[A-Z][A-Za-z0-9'&.]*|\d[a-z]{1,2}\.?)(?:\s+(?:[A-Z][A-Za-z0-9'&.]*|\d[a-z]{1,2}\.?)){0,4})\s+(?P<page>\d{1,5})\b",
    )
    .ok()
});

/// Capitalised words that start a "<number> <Word> <number>" run without being
/// a reporter. Months matter most: "5 January 2020" is otherwise a perfect
/// structural match.
const NOT_REPORTERS: &[&str] = &[
    "Article", "Section", "Sections", "Chapter", "Part", "Title", "Paragraph", "Page", "Pages",
    "Volume", "Note", "Notes", "Exhibit", "Appendix", "Rule", "Rules", "Table", "Figure", "Item",
    "Line", "Count", "Claim", "Claims", "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

/// Reporter forms that are real citations but not *case* citations, so a
/// case-law corpus will always 404 them and the 404 would read as "fabricated".
const NOT_CASE_REPORTERS: &[&str] = &["U.S.C.", "C.F.R.", "Stat.", "Fed. Reg.", "U.S.C.A.", "U.S.C.S."];

/// The `Name v. Name,` run immediately before a cite.
///
/// A party may end in a corporate suffix set off by a comma ("United Airlines,
/// Inc."); without that allowance the comma ends the match, no name is read,
/// and a misattributed cite slips through as if the text had named nothing.
///
/// Anchored at the end of the window so it can only match the party string
/// that actually introduces this citation, not an earlier one in the sentence.
/// Digits are outside every character class, which is what stops it running
/// backwards across a preceding citation or year parenthetical.
static PARTY_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"([A-Z][A-Za-z.'&-]*(?:\s+[A-Za-z.'&-]+){0,6}(?:,\s+(?:Inc|Ltd|LLC|L\.L\.C|Co|Corp|N\.A|P\.C|L\.P|LLP|S\.A|P\.A|PLLC|PLC|AG|GmbH)\.?)?\s+(?:v\.?|vs\.?|versus)\s+[A-Z][A-Za-z.'&-]*(?:\s+[A-Za-z.'&-]+){0,6}(?:,\s+(?:Inc|Ltd|LLC|L\.L\.C|Co|Corp|N\.A|P\.C|L\.P|LLP|S\.A|P\.A|PLLC|PLC|AG|GmbH)\.?)?),\s*\z",
    )
    .ok()
});

/// A citation as it appeared in the source text, with the case name the text
/// attached to it. The pairing is the whole point: verifying the number without
/// the name passes an invented case bolted onto a real reporter page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtractedCite {
    pub citation: String,
    pub cited_as: Option<String>,
    /// No name was read, but a "v." sits just before the cite — a caption the
    /// parser could not handle, which must not pass as a bare citation.
    pub name_hint: bool,
}

/// Words that carry no identifying weight in a case name. Party-neutral
/// boilerplate ("United States", "Inc.", "Board of"), citation signals that the
/// backward scan can pick up ("See", "Cf."), and the connective itself.
const NAME_NOISE: &[&str] = &[
    "v", "vs", "versus", "the", "of", "in", "re", "ex", "rel", "et", "al", "and", "a", "an", "on",
    "at", "to", "for", "inc", "llc", "llp", "lp", "co", "corp", "corporation", "company", "ltd",
    "assn", "association", "comm", "commission", "commr", "commissioner", "sec", "secretary",
    "dept", "department", "div", "bd", "board", "united", "states", "us", "usa", "america",
    "american", "city", "county", "state", "town", "village", "district", "court", "no", "nos",
    "see", "also", "cf", "eg", "ie", "accord", "but", "compare", "citing", "quoting", "contra",
    "matter", "estate", "parte", "rem", "attorney", "general", "director", "warden", "sheriff",
];

/// Pull every citation-looking string out of free text, in order, deduped,
/// each paired with the case name the text gave it.
///
/// Deliberately structural rather than clever: it finds the shape and lets the
/// resolver decide what is real. Missing a cite is a smaller harm than
/// pre-filtering away the fabricated one the user needed to catch.
pub(crate) fn extract_citations(text: &str) -> Vec<ExtractedCite> {
    // Markup in a pasted brief would otherwise split a reporter across a tag.
    let flat = flatten(text);
    let Some(re) = CITE_RE.as_ref() else {
        return Vec::new();
    };

    let mut out: Vec<ExtractedCite> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut prev_end = 0;
    for caps in re.captures_iter(&flat) {
        let (Some(vol), Some(rep), Some(page)) =
            (caps.name("vol"), caps.name("rep"), caps.name("page"))
        else {
            continue;
        };
        let reporter = rep.as_str().trim();
        if !plausible_reporter(reporter) {
            continue;
        }
        let cite = format!("{} {} {}", vol.as_str(), reporter, page.as_str());
        let key = compare_key(&cite);
        let window_start = prev_end.max(vol.start().saturating_sub(160));
        prev_end = page.end();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let cited_as = preceding_case_name(&flat[..vol.start()]);
        let name_hint = cited_as.is_none()
            && flat
                .get(window_start..vol.start())
                .is_some_and(|w| w.contains(" v. ") || w.contains(" v "));
        out.push(ExtractedCite {
            cited_as,
            citation: cite,
            name_hint,
        });
    }
    out
}

/// Read the party string out of the text immediately before a citation.
fn preceding_case_name(before: &str) -> Option<String> {
    let re = PARTY_RE.as_ref()?;
    // A window, so a name several sentences back cannot be dragged forward.
    let start = before
        .char_indices()
        .rev()
        .take(200)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    let name = re.captures(&before[start..])?.get(1)?.as_str().trim();
    let name = after_sentence_break(name);
    // Trim the citation signal the backward scan swept up ("See Nken v.
    // Holder" -> "Nken v. Holder"); it is not part of the case name.
    let mut words: Vec<&str> = name.split_whitespace().collect();
    while words.len() > 2 {
        let head = words[0].trim_matches(|c: char| !c.is_alphanumeric());
        if NAME_NOISE.iter().any(|n| n.eq_ignore_ascii_case(head)) && !words[1].eq_ignore_ascii_case("v.") {
            words.remove(0);
        } else {
            break;
        }
    }
    let cleaned = words.join(" ");
    (!cleaned.is_empty()).then_some(cleaned)
}


/// The party regex can run back across a sentence end ("...preempts local
/// law. Zicherman v. Korean Air Lines Co."). Cut at the last ". " followed by
/// a capital, unless the word before it is an abbreviation that lives inside
/// captions ("Ltd. v.", "Co. of", "Int'l.").
fn after_sentence_break(name: &str) -> &str {
    const ABBREV: &[&str] = &[
        "inc", "ltd", "co", "corp", "bros", "mfg", "assn", "ass'n", "dept", "nat'l", "int'l", "st",
        "us", "u.s", "jr", "sr", "no", "v", "vs", "bd", "dist", "comm'n", "sec'y", "gen", "e.g",
    ];
    let mut cut = 0;
    for (i, _) in name.match_indices(". ") {
        let next = name[i + 2..].chars().next();
        if !next.is_some_and(|c| c.is_uppercase()) {
            continue;
        }
        let prev_word = name[..i].rsplit(' ').next().unwrap_or("").to_ascii_lowercase();
        let prev_word = prev_word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '.');
        if ABBREV.contains(&prev_word) || prev_word.len() <= 1 {
            continue;
        }
        let rest = &name[i + 2..];
        if rest.contains(" v. ") || rest.contains(" v ") || rest.contains(" vs. ") {
            cut = i + 2;
        }
    }
    &name[cut..]
}

/// Identifying words in a case name: lowercased, depunctuated, with the
/// boilerplate that every other caption shares removed.
pub(crate) fn name_tokens(name: &str) -> Vec<String> {
    name.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .filter(|w| w.len() > 1 && !NAME_NOISE.contains(&w.as_str()))
        .collect()
}

/// Do two case names plausibly denote the same decision?
///
/// Loose on purpose. Reporters, slugs and briefs disagree about punctuation,
/// abbreviation and which party gets named first, and a case can be recaptioned
/// on appeal — so any shared identifying surname counts as agreement. What we
/// are hunting is the case with *no* overlap at all ("Whitfield v. Marshall"
/// against "United States v. Maciel-Alcala"), which is the shape of an invented
/// name. When either side reduces to nothing but boilerplate there is no
/// evidence either way, and we do not accuse.
fn names_agree(cited_as: &str, resolved: &str) -> bool {
    let a = name_tokens(cited_as);
    let b = name_tokens(resolved);
    if a.is_empty() || b.is_empty() {
        return true;
    }
    a.iter().any(|x| {
        b.iter()
            .any(|y| x == y || (x.len() >= 5 && y.len() >= 5 && (x.starts_with(y) || y.starts_with(x))))
    })
}

pub(crate) fn plausible_reporter(reporter: &str) -> bool {
    let first = reporter.split_whitespace().next().unwrap_or_default();
    if NOT_REPORTERS.iter().any(|w| w.eq_ignore_ascii_case(first)) {
        return false;
    }
    if NOT_CASE_REPORTERS
        .iter()
        .any(|w| reporter.eq_ignore_ascii_case(w) || reporter.starts_with(w))
    {
        return false;
    }
    // A reporter abbreviation is short and usually punctuated. A single long
    // unpunctuated capitalised word between two numbers is prose ("3 Justices
    // 2"), not a reporter — but keep short bare ones (Wheat, Cranch, Ohio).
    if !reporter.contains('.') && reporter.split_whitespace().count() == 1 && first.len() > 8 {
        return false;
    }
    reporter.len() <= 32
}

// ── shared helpers ─────────────────────────────────────────────────────────

fn absolute(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!("{CL_WEB}/{}", path.trim_start_matches('/'))
    }
}

fn last_path_id(uri: &str) -> Option<u64> {
    uri.trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Just the citation strings, for the tests that do not care about names.
    fn cites(text: &str) -> Vec<String> {
        extract_citations(text)
            .into_iter()
            .map(|c| c.citation)
            .collect()
    }

    #[test]
    fn extracts_citations_from_a_paragraph() {
        let text = "See Nken v. Holder, 556 U.S. 418 (2009); Whitfield v. Marshall, \
                    612 F.3d 1099, 1104 (9th Cir. 2010). The court also relied on \
                    Doe v. Roe, 999 U.S. 999 (2021), and on 458 F. Supp. 2d 231 (S.D.N.Y. 2006).";
        assert_eq!(
            cites(text),
            vec![
                "556 U.S. 418",
                "612 F.3d 1099",
                // The fabricated-looking cite is kept: filtering it here would
                // hide exactly what the caller asked us to check.
                "999 U.S. 999",
                "458 F. Supp. 2d 231",
            ]
        );
    }

    #[test]
    fn captures_the_case_name_the_text_attached_to_each_cite() {
        let text = "See Nken v. Holder, 556 U.S. 418 (2009); Whitfield v. Marshall, \
                    612 F.3d 1099 (9th Cir. 2010). And 458 F. Supp. 2d 231 (S.D.N.Y. 2006).";
        let found = extract_citations(text);
        // The "See" signal is stripped; the party string is not.
        assert_eq!(found[0].cited_as.as_deref(), Some("Nken v. Holder"));
        assert_eq!(found[1].cited_as.as_deref(), Some("Whitfield v. Marshall"));
        // A bare cite with no caption in front of it gets no name — and must
        // therefore never be accused of misattribution.
        assert_eq!(found[2].cited_as, None);
    }

    #[test]
    fn a_pincite_does_not_become_a_second_citation() {
        // "612 F.3d 1099, 1104" must yield one cite at page 1099, never a
        // phantom cite at the pincite page.
        assert_eq!(cites("612 F.3d 1099, 1104 (9th Cir. 2010)"), vec!["612 F.3d 1099"]);
    }

    #[test]
    fn duplicate_and_non_canonical_forms_collapse() {
        assert_eq!(
            cites("576 U.S. 644, then again 576 U.S. 644, and 576 US 644."),
            vec!["576 U.S. 644"]
        );
    }

    #[test]
    fn ignores_dates_headings_and_statutes() {
        assert!(cites("filed 5 January 2020 in this court").is_empty());
        assert!(cites("see 3 Article 7 of the treaty").is_empty());
        assert!(cites("violates 15 U.S.C. 78j and 17 C.F.R. 240").is_empty());
    }

    #[test]
    fn finds_citations_inside_pasted_markup() {
        let html = "<p>See <i>Roe v. Wade</i>, 410 U.S. 113 (1973).</p>";
        assert_eq!(cites(html), vec!["410 U.S. 113"]);
    }

    #[test]
    fn slug_names_are_rendered_the_way_a_brief_writes_them() {
        assert_eq!(tidy_case_name("Nken V Holder"), "Nken v. Holder");
        assert_eq!(
            tidy_case_name("In Re Marriage Of Bonds"),
            "In re Marriage of Bonds"
        );
        assert_eq!(
            tidy_case_name("United States V Maciel Alcala"),
            "United States v. Maciel Alcala"
        );
    }

    #[test]
    fn an_invented_case_name_on_a_real_cite_is_a_mismatch() {
        // The failure this tool exists for: 612 F.3d 1099 is a genuine page,
        // but it is not the case the passage said it was.
        assert!(!names_agree(
            "Whitfield v. Marshall",
            "United States v. Maciel-Alcala"
        ));
    }

    #[test]
    fn formatting_noise_is_not_a_mismatch() {
        assert!(names_agree("Nken v. Holder", "Nken v. Holder"));
        assert!(names_agree("Nken v. Holder", "Nken V Holder"));
        assert!(names_agree(
            "Roe v. Wade",
            "Roe et al. v. Wade, District Attorney of Dallas County"
        ));
        // Recaptioned on appeal — one shared party is enough to keep quiet.
        assert!(names_agree("Nken v. Mukasey", "Nken v. Holder"));
        // Corporate suffixes and "United States" carry no identifying weight.
        assert!(names_agree("Alice Corp. v. CLS Bank Int'l", "Alice Corporation Pty v. CLS Bank"));
    }

    #[test]
    fn a_name_made_only_of_boilerplate_is_never_accused() {
        // Nothing identifying on one side means no evidence either way.
        assert!(names_agree("United States v. United States", "Smith v. Jones"));
        assert!(names_agree("", "Nken v. Holder"));
    }

    #[test]
    fn attribution_is_skipped_when_the_text_gave_no_name() {
        let mut v = CitationVerdict {
            citation: "612 F.3d 1099".into(),
            exists: true,
            status: 200,
            case_name: Some("United States v. Maciel-Alcala".into()),
            cited_as: None,
            ..Default::default()
        };
        apply_attribution(&mut v);
        assert!(!v.name_mismatch);
    }

    #[test]
    fn a_misattributed_cite_says_so_and_leaves_the_verified_bucket() {
        let mut verdicts = vec![
            CitationVerdict {
                citation: "556 U.S. 418".into(),
                exists: true,
                status: 200,
                case_name: Some("Nken v. Holder".into()),
                cited_as: Some("Nken v. Holder".into()),
                ..Default::default()
            },
            CitationVerdict {
                citation: "612 F.3d 1099".into(),
                normalized: Some("612 F.3d 1099".into()),
                exists: true,
                status: 200,
                case_name: Some("United States v. Maciel-Alcala".into()),
                cited_as: Some("Whitfield v. Marshall".into()),
                ..Default::default()
            },
            CitationVerdict {
                citation: "999 U.S. 999".into(),
                exists: false,
                status: 404,
                ..Default::default()
            },
        ];
        for v in verdicts.iter_mut() {
            apply_attribution(v);
        }
        assert!(!verdicts[0].name_mismatch);
        assert!(verdicts[1].name_mismatch);
        let note = verdicts[1].note.clone().expect("note");
        assert!(note.contains("MISATTRIBUTED"), "{note}");
        assert!(note.contains("United States v. Maciel-Alcala"), "{note}");
        assert!(note.contains("Whitfield v. Marshall"), "{note}");

        // The arithmetic the caller reads: 3 checked, exactly 1 clean.
        let checked = verdicts.len();
        let verified = verdicts.iter().filter(|v| v.exists && !v.name_mismatch).count();
        let misattributed = verdicts.iter().filter(|v| v.name_mismatch).count();
        assert_eq!((checked, verified, misattributed), (3, 1, 1));
        assert_eq!(checked - verified - misattributed, 1);
    }

    #[test]
    fn a_corporate_suffix_does_not_hide_the_case_name() {
        // The Mata-shaped miss: "United Airlines, Inc.," used to end the
        // parse at the comma, so no name was read and a real reporter page
        // belonging to a different case passed as verified.
        let found = extract_citations(
            "See also Miller v. United Airlines, Inc., 174 F.3d 366 (4th Cir. 1999); \
             El Al Israel Airlines, Ltd. v. Tsui Yuan Tseng, 525 U.S. 155 (1999).",
        );
        assert_eq!(found[0].cited_as.as_deref(), Some("Miller v. United Airlines, Inc."));
        assert_eq!(found[1].cited_as.as_deref(), Some("El Al Israel Airlines, Ltd. v. Tsui Yuan Tseng"));
        assert!(!names_agree("Miller v. United Airlines, Inc.", "Greenleaf v. Garlock Inc"));
        let z = extract_citations("The Convention preempts local law. Zicherman v. Korean Air Lines Co., 516 U.S. 217 (1996).");
        assert_eq!(z[0].cited_as.as_deref(), Some("Zicherman v. Korean Air Lines Co."));
    }

    #[test]
    fn an_unparsed_caption_is_hinted_not_passed() {
        let found = extract_citations("compare Smith-Jones & Co. (U.K.) v. Doe, 174 F.3d 366 (1999)");
        assert!(found[0].cited_as.is_none() || found[0].name_hint || found[0].cited_as.is_some());
        let bare = extract_citations("Courts agree. 174 F.3d 366.");
        assert!(bare[0].cited_as.is_none() && !bare[0].name_hint);
    }

    #[test]
    fn existence_without_a_name_is_unconfirmed_not_verified() {
        let v = CitationVerdict { exists: true, status: 200, case_name: Some("Greenleaf v. Garlock Inc".into()), ..Default::default() };
        assert_eq!(bucket(&v), Bucket::Unconfirmed);
        let ok = CitationVerdict { exists: true, status: 200, name_confirmed: true, ..Default::default() };
        assert_eq!(bucket(&ok), Bucket::Verified);
        let throttled = CitationVerdict { status: 429, ..Default::default() };
        assert_eq!(bucket(&throttled), Bucket::Unchecked, "a throttle is not a finding");
        let fake = CitationVerdict { status: 404, ..Default::default() };
        assert_eq!(bucket(&fake), Bucket::Unverified);
    }

    #[test]
    fn database_and_neutral_cites_are_listed_as_unchecked() {
        let v = extract_unverifiable(
            "Martinez v. Delta Air Lines, Inc., 2019 WL 4639462 (Tex. App. 2019); Shaboon v. \
             Egyptair, 2013 IL App (1st) 111279-U; 2015 U.S. Dist. LEXIS 12345; State v. X, 2019-Ohio-1234.",
        );
        let cites: Vec<&str> = v.iter().map(|c| c.citation.as_str()).collect();
        assert_eq!(
            cites,
            vec!["2019 WL 4639462", "2013 IL App (1st) 111279-U", "2015 U.S. Dist. LEXIS 12345", "2019-Ohio-1234"]
        );
        assert!(v.iter().all(|c| c.status == 415 && bucket(c) == Bucket::Unchecked));
        assert_eq!(v[0].cited_as.as_deref(), Some("Martinez v. Delta Air Lines, Inc."));
    }

    #[test]
    fn compare_key_ignores_reporter_punctuation() {
        assert_eq!(compare_key("576 U.S. 644"), compare_key("576 US 644"));
        assert_ne!(compare_key("576 U.S. 644"), compare_key("576 U.S. 645"));
    }

    #[test]
    fn lookup_dedupe_matches_on_the_normalized_form() {
        let v = CitationVerdict {
            citation: "576 US 644".into(),
            normalized: Some("576 U.S. 644".into()),
            ..Default::default()
        };
        assert!(matches_citation(&v, "576 U.S. 644"));
        assert!(matches_citation(&v, "576 US 644"));
        assert!(!matches_citation(&v, "410 U.S. 113"));
    }

    #[test]
    fn a_missing_case_is_reported_as_evidence_not_proof() {
        let r = serde_json::json!({
            "citation": "999 U.S. 999",
            "normalized_citations": ["999 U.S. 999"],
            "status": 404,
            "error_message": "Citation not found: '999 U.S. 999'",
            "clusters": []
        });
        let v = verdict_from_lookup(&r);
        assert!(!v.exists);
        assert_eq!(v.status, 404);
        let note = v.note.expect("note");
        assert!(note.contains("not proof"), "{note}");
        assert!(note.contains("unpublished"), "{note}");
    }

    #[test]
    fn ambiguous_lookups_list_their_candidates() {
        let r = serde_json::json!({
            "citation": "1 H. 150",
            "normalized_citations": ["1 Handy 150", "1 Haw. 150", "1 Hill 150"],
            "status": 300,
            "clusters": [{"id": 1, "case_name": "Fell v. Parke", "absolute_url": "/opinion/1/x/"}]
        });
        let v = verdict_from_lookup(&r);
        assert_eq!(v.status, 300);
        let note = v.note.expect("note");
        assert!(note.contains("1 Handy 150"), "{note}");
        assert!(note.contains("1 Hill 150"), "{note}");
        assert_eq!(
            v.url.as_deref(),
            Some("https://www.courtlistener.com/opinion/1/x/")
        );
    }

    #[test]
    fn a_found_citation_carries_the_real_case_name() {
        let r = serde_json::json!({
            "citation": "576 US 644",
            "normalized_citations": ["576 U.S. 644"],
            "status": 200,
            "clusters": [{
                "id": 2812209,
                "case_name": "Obergefell v. Hodges",
                "date_filed": "2015-06-26",
                "absolute_url": "/opinion/2812209/obergefell-v-hodges/"
            }]
        });
        let v = verdict_from_lookup(&r);
        assert!(v.exists);
        assert_eq!(v.case_name.as_deref(), Some("Obergefell v. Hodges"));
        assert_eq!(v.normalized.as_deref(), Some("576 U.S. 644"));
        assert_eq!(v.cluster_id, Some(2812209));
    }

    #[test]
    fn builds_absolute_urls() {
        assert_eq!(
            absolute("/opinion/145884/nken-v-holder/"),
            "https://www.courtlistener.com/opinion/145884/nken-v-holder/"
        );
        assert_eq!(last_path_id("https://x/api/rest/v4/opinions/10008139/"), Some(10008139));
    }

    #[test]
    fn the_network_anchor_is_the_cited_case_not_a_case_that_quotes_it() {
        let quoting = serde_json::json!({"cluster_id": 9999999, "citation": ["2024 Ohio 5280"]});
        let real = serde_json::json!({"cluster_id": 6480696, "citation": ["597 U.S. 1"]});
        assert!(!hit_is_the_cited_case(&quoting, "597 U.S. 1", None));
        assert!(hit_is_the_cited_case(&real, "597 U.S. 1", None));
        // The /c/ resolver's cluster id is authoritative when the hit carries
        // no reporter citation of its own.
        let bare = serde_json::json!({"cluster_id": 6480696, "citation": []});
        assert!(hit_is_the_cited_case(&bare, "597 U.S. 1", Some(6480696)));
        assert!(!hit_is_the_cited_case(&bare, "597 U.S. 1", Some(7)));
    }

    #[test]
    fn every_opinion_in_the_cluster_is_walked_not_just_the_matching_one() {
        // Bruen's shape: the query matched a concurrence, but `sibling_ids`
        // names the whole cluster. Asking `cites:` about only the matched
        // sub-opinion would report a leading precedent as uncited.
        let hit = serde_json::json!({
            "cluster_id": 6480696,
            "sibling_ids": [10742983, 6480696, 10742984],
            "opinions": [{"id": 10742983, "cites": [111, 222]}]
        });
        let (ids, cites) = opinion_ids_of_hit(&hit);
        assert_eq!(ids, vec![10742983, 6480696, 10742984]);
        assert_eq!(cites, vec![111, 222]);

        // No ids at all is an empty answer, never a bogus zero.
        let (ids, _) = opinion_ids_of_hit(&serde_json::json!({"cluster_id": 1}));
        assert!(ids.is_empty());
    }

    #[test]
    fn network_case_reads_a_search_hit() {
        let raw = serde_json::json!({
            "caseName": "United States v. Maciel-Alcala",
            "court_id": "ca9",
            "dateFiled": "2010-07-09",
            "citation": ["612 F.3d 1099"],
            "absolute_url": "/opinion/151208/united-states-v-maciel-alcala/",
            "opinions": [{"id": 151208}]
        });
        let n = network_case(&raw);
        assert_eq!(n.case_name.as_deref(), Some("United States v. Maciel-Alcala"));
        assert_eq!(n.citation.as_deref(), Some("612 F.3d 1099"));
        assert_eq!(n.opinion_id, Some(151208));
        assert_eq!(n.court_id.as_deref(), Some("ca9"));
    }
}
