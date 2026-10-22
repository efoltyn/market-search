//! Quotation and pin-cite verification — the check that catches a real case
//! made to say something it never said.
//!
//! Existence and attribution checks pass a fabricated quotation from a real
//! decision, and that is the failure courts have started sanctioning on its
//! own (fabricated quotes from real opinions, not just invented cases). So for
//! every quotation in a passage this module:
//!
//!   1. works out which case it is attributed to — the citation right after
//!      it, a short form ("525 U.S. at 161"), an "Id.", or the case named in
//!      the sentence that introduces it;
//!   2. loads that opinion's text, preferring the Caselaw Access Project's
//!      static HTML because it carries the reporter's page breaks (`*161`),
//!      which is what makes a pin cite checkable at all;
//!   3. aligns the quotation against the opinion word by word, tolerant of the
//!      OCR noise in digitised reporters ("een-trally" for "centrally"), of
//!      ellipses, and of bracketed alterations;
//!   4. reports verbatim / close (with the court's actual words) / not found,
//!      and whether the pages the passage sits on include the pin cite.
//!
//! A quote it cannot load text for is `unchecked`, never `not_found`: no text
//! is no finding.

use super::fetch::{
    compare_key, flatten, name_tokens, plausible_reporter, CitationVerdict, CITE_RE,
};
use crate::legal::{courtlistener, shared_client, strip_markup};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

/// Quotations shorter than this many words are usually defined terms or a
/// single phrase ("arising under"), where a match proves little and a miss
/// means less.
const MIN_QUOTE_WORDS: usize = 4;
/// Longest quotation aligned in full; the rest is reported as truncated.
const MAX_QUOTE_WORDS: usize = 160;
/// Opinions fetched per call. CAP is not throttled, but a brief quoting forty
/// cases should not turn into forty downloads without saying so.
const MAX_OPINIONS: usize = 40;
const MAX_QUOTES: usize = 120;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct QuoteCheck {
    /// The quotation as it appears in the text (clamped).
    pub quote: String,
    /// The full citation the quotation is attributed to.
    pub attributed_to: Option<String>,
    pub case_name: Option<String>,
    /// How the attribution was made: `following citation`, `short cite`,
    /// `id.`, `case named in sentence`, `preceding citation`, or `none`.
    pub attribution: String,
    /// The pin cite given for the quotation, when there was one.
    pub pin: Option<String>,
    /// `verbatim` | `close` | `not_found` | `unchecked`.
    pub status: String,
    /// Share of the quotation's words found in order in the opinion.
    pub match_score: Option<f32>,
    /// The court's actual words at the best match — for `close`, this is what
    /// the brief should say instead.
    pub found_text: Option<String>,
    /// Reporter pages the matched passage sits on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub found_pages: Vec<u32>,
    /// `majority`, `dissent`, `concurrence`... when the source marks it. A
    /// quotation from a dissent attributed to "the Court" is its own error.
    pub opinion_part: Option<String>,
    /// `ok` | `mismatch` | `unknown` (no page data) | null (no pin given).
    pub pin_check: Option<String>,
    pub source_url: Option<String>,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct QuoteSummary {
    pub quotes: usize,
    pub verbatim: usize,
    pub close: usize,
    pub not_found: usize,
    pub unchecked: usize,
    pub pin_mismatches: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ShortCite {
    pub text: String,
    /// The full citation earlier in the text this short form points to.
    pub resolves_to: Option<String>,
    pub note: Option<String>,
}

// ── citation occurrences ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Kind {
    /// "525 U.S. 155" (key is the full cite).
    Full,
    /// "525 U.S. at 161" (key is volume + reporter).
    Short,
    /// "Id." / "id. at 161".
    Id,
}

#[derive(Clone, Debug)]
struct Occurrence {
    start: usize,
    end: usize,
    kind: Kind,
    /// Full cite text for `Full`; "vol reporter" for `Short`; empty for `Id`.
    cite: String,
    pin: Option<String>,
}

static SHORT_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?P<vol>\b\d{1,4})\s+(?P<rep>(?:[A-Z][A-Za-z0-9'&.]*|\d[a-z]{1,2}\.?)(?:\s+(?:[A-Z][A-Za-z0-9'&.]*|\d[a-z]{1,2}\.?)){0,4}),?\s+at\s+(?P<pin>\d{1,5}(?:\s*[-–]\s*\d{1,5})?)",
    )
    .ok()
});

static ID_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"\b[Ii]d\.(?:,?\s+at\s+(?P<pin>\d{1,5}(?:\s*[-–]\s*\d{1,5})?))?").ok()
});

static PIN_AFTER_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^,\s*(\d{1,5}(?:\s*[-–]\s*\d{1,5})?)").ok());

fn occurrences(flat: &str) -> Vec<Occurrence> {
    let mut out = Vec::new();
    if let Some(re) = SHORT_RE.as_ref() {
        for c in re.captures_iter(flat) {
            let (Some(vol), Some(rep), Some(pin), Some(m)) =
                (c.name("vol"), c.name("rep"), c.name("pin"), c.get(0))
            else {
                continue;
            };
            let rep = rep.as_str().trim();
            if !plausible_reporter(rep) {
                continue;
            }
            out.push(Occurrence {
                start: m.start(),
                end: m.end(),
                kind: Kind::Short,
                cite: format!("{} {rep}", vol.as_str()),
                pin: Some(pin.as_str().split_whitespace().collect()),
            });
        }
    }
    if let Some(re) = CITE_RE.as_ref() {
        for c in re.captures_iter(flat) {
            let (Some(vol), Some(rep), Some(page), Some(m)) =
                (c.name("vol"), c.name("rep"), c.name("page"), c.get(0))
            else {
                continue;
            };
            let rep = rep.as_str().trim();
            if !plausible_reporter(rep)
                || out
                    .iter()
                    .any(|o| o.start <= m.start() && m.start() < o.end)
            {
                continue;
            }
            // A pin follows the page after a comma — unless what follows is
            // the next citation in a string cite ("556 U.S. 418, 612 F.3d").
            let rest = &flat[m.end()..];
            let pin = PIN_AFTER_RE
                .as_ref()
                .and_then(|r| r.captures(rest))
                .and_then(|pc| {
                    let p = pc.get(1)?;
                    let after = rest[p.end()..].trim_start();
                    let next_is_reporter = after
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_ascii_uppercase())
                        && !after.starts_with("n.")
                        && after
                            .split_whitespace()
                            .nth(1)
                            .is_some_and(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()));
                    (!next_is_reporter).then(|| p.as_str().split_whitespace().collect::<String>())
                });
            out.push(Occurrence {
                start: m.start(),
                end: m.end(),
                kind: Kind::Full,
                cite: format!("{} {rep} {}", vol.as_str(), page.as_str()),
                pin,
            });
        }
    }
    if let Some(re) = ID_RE.as_ref() {
        for c in re.captures_iter(flat) {
            let m = c.get(0).expect("match");
            out.push(Occurrence {
                start: m.start(),
                end: m.end(),
                kind: Kind::Id,
                cite: String::new(),
                pin: c
                    .name("pin")
                    .map(|p| p.as_str().split_whitespace().collect()),
            });
        }
    }
    out.sort_by_key(|o| o.start);
    out
}

/// "525 U.S. at 161" -> the full cite in `fulls` with the same volume and reporter.
fn resolve_short<'a>(vol_rep: &str, fulls: &'a [String]) -> Option<&'a String> {
    let key = compare_key(vol_rep);
    fulls.iter().find(|f| {
        let mut parts: Vec<&str> = f.split_whitespace().collect();
        parts.pop();
        compare_key(&parts.join(" ")) == key
    })
}

// ── quotation extraction ───────────────────────────────────────────────────

struct RawQuote {
    start: usize,
    end: usize,
    text: String,
}

static QUOTE_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#""([^"]{12,3000})"|“([^”]{12,3000})”"#).ok());

fn extract_quotes(flat: &str) -> Vec<RawQuote> {
    let Some(re) = QUOTE_RE.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(flat)
        .filter_map(|c| {
            let m = c.get(0)?;
            let inner = c.get(1).or_else(|| c.get(2))?.as_str();
            (words(inner).len() >= MIN_QUOTE_WORDS).then(|| RawQuote {
                start: m.start(),
                end: m.end(),
                text: inner.trim().to_string(),
            })
        })
        .collect()
}

// ── the check ──────────────────────────────────────────────────────────────

pub(crate) async fn check(
    text: &str,
    verdicts: &[CitationVerdict],
    warnings: &mut Vec<String>,
) -> (Vec<QuoteCheck>, Option<QuoteSummary>, Vec<ShortCite>) {
    let flat = flatten(text);
    let occ = occurrences(&flat);
    let fulls: Vec<String> = occ
        .iter()
        .filter(|o| matches!(o.kind, Kind::Full))
        .map(|o| o.cite.clone())
        .collect();

    // Short cites: each must point back to a full citation, or it cannot be
    // verified at all (and an orphan short cite is a fabrication tell).
    let mut shorts = Vec::new();
    for o in occ.iter().filter(|o| matches!(o.kind, Kind::Short)) {
        let earlier: Vec<String> = occ
            .iter()
            .filter(|f| matches!(f.kind, Kind::Full) && f.start < o.start)
            .map(|f| f.cite.clone())
            .collect();
        let resolves_to = resolve_short(&o.cite, &earlier)
            .or_else(|| resolve_short(&o.cite, &fulls))
            .cloned();
        let note = match &resolves_to {
            None => Some(
                "no full citation with this volume and reporter appears in the text, so this short \
                 cite points at nothing that can be checked"
                    .to_string(),
            ),
            Some(f) if !earlier.contains(f) => Some(format!("the full citation ({f}) only appears after this short form")),
            _ => None,
        };
        shorts.push(ShortCite {
            text: flat[o.start..o.end].to_string(),
            resolves_to,
            note,
        });
    }

    let quotes = extract_quotes(&flat);
    if quotes.is_empty() {
        return (Vec::new(), None, shorts);
    }
    if quotes.len() > MAX_QUOTES {
        warnings.push(format!(
            "{} quotations found; checked the first {MAX_QUOTES}",
            quotes.len()
        ));
    }

    let mut cache: HashMap<String, Option<OpinionText>> = HashMap::new();
    let mut out = Vec::new();
    let mut last_full: Option<String> = None;
    let mut cursor = 0;
    for q in quotes.iter().take(MAX_QUOTES) {
        // Keep "the last full citation before here" current, so Id. resolves.
        for o in occ
            .iter()
            .filter(|o| o.start >= cursor && o.start < q.start)
        {
            update_last(&mut last_full, o, &fulls);
        }
        cursor = q.start;

        let (cite, pin, how) = attribute(&flat, q, &occ, &fulls, last_full.as_deref(), verdicts);
        let mut check = QuoteCheck {
            quote: clamp(&q.text, 400),
            attributed_to: cite.clone(),
            attribution: how.to_string(),
            pin: pin.clone(),
            ..Default::default()
        };
        let Some(cite) = cite else {
            check.status = "unchecked".into();
            check.note = "no citation could be tied to this quotation, so there is nothing to check it against".into();
            out.push(check);
            continue;
        };
        let verdict = verdicts.iter().find(|v| {
            compare_key(&v.citation) == compare_key(&cite)
                || v.normalized.as_deref().map(compare_key) == Some(compare_key(&cite))
        });
        check.case_name = verdict.and_then(|v| v.case_name.clone());
        if let Some(v) = verdict {
            if !v.exists && matches!(v.status, 404 | 400) {
                check.status = "unchecked".into();
                check.note = format!("attributed to {cite}, which does not resolve to any case — the quotation cannot come from it");
                out.push(check);
                continue;
            }
        }

        if !cache.contains_key(&cite) {
            let loaded = if cache.len() >= MAX_OPINIONS {
                None
            } else {
                load_opinion(&cite, warnings).await
            };
            cache.insert(cite.clone(), loaded);
        }
        let Some(op) = cache.get(&cite).and_then(|o| o.as_ref()) else {
            check.status = "unchecked".into();
            check.note = format!(
                "no free full text for {cite} (the Caselaw Access Project ends around 2020; with \
                 COURTLISTENER_TOKEN set the canonical text is used). Not checked — not a finding."
            );
            out.push(check);
            continue;
        };
        check.source_url = Some(op.url.clone());
        judge(&mut check, q, op, verdict);
        out.push(check);
    }

    let mut summary = QuoteSummary {
        quotes: out.len(),
        ..Default::default()
    };
    for c in &out {
        match c.status.as_str() {
            "verbatim" => summary.verbatim += 1,
            "close" => summary.close += 1,
            "not_found" => summary.not_found += 1,
            _ => summary.unchecked += 1,
        }
        if c.pin_check.as_deref() == Some("mismatch") {
            summary.pin_mismatches += 1;
        }
    }
    if summary.not_found > 0 {
        warnings.push(format!(
            "{} quotation(s) were NOT FOUND in the opinion they are attributed to — the failure \
             courts sanction even when the case itself is real. Each carries the closest passage \
             the court actually wrote, when there was one.",
            summary.not_found
        ));
    }
    if summary.pin_mismatches > 0 {
        warnings.push(format!(
            "{} quotation(s) were found, but not on the page the pin cite gives",
            summary.pin_mismatches
        ));
    }
    (out, Some(summary), shorts)
}

fn update_last(last: &mut Option<String>, o: &Occurrence, fulls: &[String]) {
    match o.kind {
        Kind::Full => *last = Some(o.cite.clone()),
        Kind::Short => {
            if let Some(f) = resolve_short(&o.cite, fulls) {
                *last = Some(f.clone());
            }
        }
        Kind::Id => {}
    }
}

/// Which case is this quotation from, and at what pin?
fn attribute(
    flat: &str,
    q: &RawQuote,
    occ: &[Occurrence],
    fulls: &[String],
    last_full: Option<&str>,
    verdicts: &[CitationVerdict],
) -> (Option<String>, Option<String>, &'static str) {
    // 1. A citation right after the quotation — the normal form. Stop at the
    //    next quotation so one cite is not handed to two quotes.
    let next_quote = QUOTE_RE
        .as_ref()
        .and_then(|re| re.find(&flat[q.end..]))
        .map(|m| q.end + m.start())
        .unwrap_or(flat.len());
    let horizon = next_quote.min(q.end + 320);
    if let Some(o) = occ.iter().find(|o| o.start >= q.end && o.start < horizon) {
        match o.kind {
            Kind::Full => return (Some(o.cite.clone()), o.pin.clone(), "following citation"),
            Kind::Short => {
                if let Some(f) = resolve_short(&o.cite, fulls) {
                    return (Some(f.clone()), o.pin.clone(), "short cite");
                }
            }
            Kind::Id => {
                if let Some(f) = last_full {
                    return (Some(f.to_string()), o.pin.clone(), "id.");
                }
            }
        }
    }

    // 2. The case named in the sentence that introduces the quotation
    //    ("As the Court put it in Tseng, '...'").
    let lead_start = flat[..q.start]
        .rfind(['.', ';'])
        .map(|i| i + 1)
        .unwrap_or(0)
        .max(q.start.saturating_sub(300));
    let lead: Vec<String> = name_tokens(&flat[lead_start..q.start]);
    if !lead.is_empty() {
        for v in verdicts.iter().filter(|v| v.exists) {
            let names: Vec<String> = v
                .cited_as
                .iter()
                .chain(v.case_name.iter())
                .flat_map(|n| name_tokens(n))
                .filter(|t| t.len() >= 4)
                .collect();
            if names.iter().any(|n| lead.contains(n)) {
                return (
                    Some(v.normalized.clone().unwrap_or_else(|| v.citation.clone())),
                    None,
                    "case named in sentence",
                );
            }
        }
    }

    // 3. The nearest citation before it, within the same paragraph-ish span.
    if let Some(o) = occ
        .iter()
        .rev()
        .find(|o| o.end <= q.start && q.start - o.end < 600)
    {
        match o.kind {
            Kind::Full => return (Some(o.cite.clone()), None, "preceding citation"),
            Kind::Short => {
                if let Some(f) = resolve_short(&o.cite, fulls) {
                    return (Some(f.clone()), None, "preceding citation");
                }
            }
            Kind::Id => {
                if let Some(f) = last_full {
                    return (Some(f.to_string()), None, "preceding citation");
                }
            }
        }
    }
    (None, None, "none")
}

/// Align, grade, and check the pin.
fn judge(
    check: &mut QuoteCheck,
    q: &RawQuote,
    op: &OpinionText,
    verdict: Option<&CitationVerdict>,
) {
    let segments = quote_segments(&q.text);
    let total: usize = segments.iter().map(Vec::len).sum();
    if total == 0 {
        check.status = "unchecked".into();
        check.note = "the quotation has no checkable words".into();
        return;
    }
    let mut matched = 0usize;
    let mut contiguous = true;
    let mut span: Option<(usize, usize)> = None;
    let mut prev_end = 0usize;
    let mut in_order = true;
    for seg in &segments {
        if seg.is_empty() {
            continue;
        }
        let Some(a) = align(seg, &op.words) else {
            contiguous = false;
            continue;
        };
        matched += a.matches;
        if a.matches != seg.len() || a.end - a.start + 1 != seg.len() {
            contiguous = false;
        }
        if a.start < prev_end {
            in_order = false;
        }
        prev_end = a.end;
        let (lo, hi) = widen(&a, seg, &op.words);
        span = Some(match span {
            None => (lo, hi),
            Some((s, e)) => (s.min(lo), e.max(hi)),
        });
    }
    let score = matched as f32 / total as f32;
    check.match_score = Some((score * 100.0).round() / 100.0);

    if let Some((s, e)) = span {
        let (s_byte, e_byte) = (op.words[s].start, op.words[e].end);
        check.found_text = Some(clamp(&op.text[s_byte..e_byte], 700));
        let mut pages: Vec<u32> = op.words[s..=e].iter().filter_map(|w| w.page).collect();
        pages.dedup();
        check.found_pages = pages;
        check.opinion_part = op.words[s].part.map(|i| op.parts[i].clone());
    }

    let real = verdict
        .and_then(|v| v.case_name.clone())
        .unwrap_or_else(|| "the cited opinion".into());
    if score >= 0.999 && contiguous && in_order {
        check.status = "verbatim".into();
        check.note = format!("found word for word in {real}");
    } else if score >= 0.75 {
        check.status = "close".into();
        check.note =
            format!(
            "found in {real}, but not word for word ({}% of the words, in order{}). Compare the \
             brief with found_text — quote the court exactly, or mark the alterations.",
            (score * 100.0).round(),
            if in_order { "" } else { " — but the segments appear out of order" }
        );
    } else {
        check.status = "not_found".into();
        check.note = if score >= 0.35 && check.found_text.is_some() {
            format!(
                "NOT FOUND in {real}: at best {}% of the words appear together. found_text is the \
                 closest passage the court actually wrote — the quotation as written is not in \
                 this opinion.",
                (score * 100.0).round()
            )
        } else {
            check.found_text = None;
            check.found_pages.clear();
            check.opinion_part = None;
            format!("NOT FOUND in {real}: no passage in the opinion resembles this quotation")
        };
    }
    if verdict.is_some_and(|v| v.name_mismatch) {
        check.note.push_str(" (Note: the citation itself is misattributed — this was checked against the case actually at that page.)");
    }
    if check
        .opinion_part
        .as_deref()
        .is_some_and(|p| p != "majority" && p != "opinion" && p != "per curiam")
    {
        check.note.push_str(&format!(
            " The passage is in the {} — not the opinion of the court.",
            check.opinion_part.as_deref().unwrap_or_default()
        ));
    }

    // Pin cite.
    if let (Some(pin), true) = (check.pin.as_deref(), check.status != "not_found") {
        check.pin_check = Some(if !op.pages_known || check.found_pages.is_empty() {
            "unknown".into()
        } else if let Some((lo, hi)) = pin_range(pin) {
            let (first, last) = (
                *check.found_pages.first().unwrap(),
                *check.found_pages.last().unwrap(),
            );
            if lo <= last && hi >= first {
                "ok".into()
            } else {
                check.note.push_str(&format!(
                    " Pin cite says {pin}, but the passage is on {}.",
                    pages_label(&check.found_pages)
                ));
                "mismatch".into()
            }
        } else {
            "unknown".into()
        });
    }
}

/// Widen an alignment to the quotation's full length. If the quotation's
/// first (last) word turns up just beyond the aligned stretch, stop there; a
/// reworded ending is often a different word count ("airline" for "air
/// carrier"), and the reader needs to see all of the court's version.
fn widen(a: &Alignment, seg: &[String], words: &[Word]) -> (usize, usize) {
    let last = words.len() - 1;
    let hi = if a.tail_missing == 0 {
        a.end
    } else {
        let reach = (a.end + a.tail_missing + 3).min(last);
        seg.last()
            .and_then(|w| (a.end + 1..=reach).find(|&j| same(w, &words[j].norm)))
            .unwrap_or((a.end + a.tail_missing).min(last))
    };
    let lo = if a.head_missing == 0 {
        a.start
    } else {
        let reach = a.start.saturating_sub(a.head_missing + 3);
        seg.first()
            .and_then(|w| (reach..a.start).rev().find(|&j| same(w, &words[j].norm)))
            .unwrap_or(a.start.saturating_sub(a.head_missing))
    };
    (lo, hi)
}

fn pages_label(pages: &[u32]) -> String {
    match pages {
        [one] => format!("page {one}"),
        [first, .., last] => format!("pages {first}-{last}"),
        [] => "an unknown page".into(),
    }
}

/// "161" -> (161,161); "161-62" -> (161,162); "1099-1104" -> (1099,1104).
fn pin_range(pin: &str) -> Option<(u32, u32)> {
    let mut parts = pin.split(['-', '–']).map(str::trim);
    let lo_s = parts.next()?;
    let lo: u32 = lo_s.parse().ok()?;
    let hi = match parts.next() {
        None => lo,
        Some(h) if h.len() < lo_s.len() => {
            let prefix = &lo_s[..lo_s.len() - h.len()];
            format!("{prefix}{h}").parse().ok()?
        }
        Some(h) => h.parse().ok()?,
    };
    Some((lo, hi.max(lo)))
}

// ── opinion text ───────────────────────────────────────────────────────────

struct Word {
    norm: String,
    start: usize,
    end: usize,
    page: Option<u32>,
    part: Option<usize>,
}

struct OpinionText {
    text: String,
    words: Vec<Word>,
    parts: Vec<String>,
    pages_known: bool,
    url: String,
}

const PAGE_OPEN: char = '\u{E000}';
const PAGE_CLOSE: char = '\u{E001}';
const PART_OPEN: char = '\u{E002}';
const PART_CLOSE: char = '\u{E003}';

async fn load_opinion(cite: &str, warnings: &mut Vec<String>) -> Option<OpinionText> {
    let parsed = courtlistener::parse_cite(cite)?;
    // 1. CAP static HTML: key-free, unthrottled, with page labels.
    let page: u32 = parsed.page.parse().ok()?;
    let volume: u32 = parsed.volume.parse().ok()?;
    let reporter = courtlistener::slug_reporter(&parsed.reporter);
    if !reporter.is_empty() {
        let url = format!("https://static.case.law/{reporter}/{volume}/html/{page:04}-01.html");
        if let Ok(resp) = shared_client::GENERAL.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(html) = resp.text().await {
                    if let Some(op) = from_cap_html(&html, page, url) {
                        return Some(op);
                    }
                }
            }
        }
    }
    // 2. With a token, the canonical CourtListener text (no page data). Without
    //    one, the remaining key-free rungs are a bot-walled page and a search
    //    snippet that would spend the anonymous budget on a fragment.
    if courtlistener::has_token() {
        let resp = crate::legal::caselaw::fetch_opinion(crate::legal::caselaw::OpinionRequest {
            id: None,
            cite: Some(cite.to_string()),
            max_chars: 2_000_000,
        })
        .await
        .ok()?;
        if resp.text_source == "search_snippet" || resp.text_source == "none" {
            return None;
        }
        let text = resp.text?;
        let url = resp.text_source_url.or(resp.url).unwrap_or_default();
        return Some(from_plain(&text, url));
    }
    let _ = warnings;
    None
}

static PAGE_LABEL_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"<a\b[^>]*class="page-label"[^>]*>\*?(\d+)</a>"#).ok());
static PAGE_INDEX_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"data-citation-index="(\d+)""#).ok());
static FOOTNOTE_MARK_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(?s)<a\b[^>]*class="footnotemark"[^>]*>.*?</a>"#).ok());
static PART_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r#"<article\b[^>]*class="opinion"[^>]*data-type="([a-z-]+)"[^>]*>"#).ok()
});
static FIRSTPAGE_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"data-firstpage="(\d+)""#).ok());

/// CAP HTML -> words tagged with reporter page and opinion part.
fn from_cap_html(html: &str, cited_page: u32, url: String) -> Option<OpinionText> {
    // Page labels carry a citation index: 1 is the reporter the file is filed
    // under. If the cited page is not that reporter's first page, the brief is
    // citing a parallel reporter and these labels do not number its pages.
    let first = FIRSTPAGE_RE
        .as_ref()?
        .captures(html)
        .and_then(|c| c[1].parse::<u32>().ok());
    let pages_known = first == Some(cited_page);

    let html = PAGE_LABEL_RE
        .as_ref()?
        .replace_all(html, |c: &regex::Captures| {
            let whole = c.get(0).map(|m| m.as_str()).unwrap_or_default();
            let idx = PAGE_INDEX_RE
                .as_ref()
                .and_then(|r| r.captures(whole))
                .map(|i| i[1].to_string())
                .unwrap_or_else(|| "1".into());
            if idx == "1" {
                format!(" {PAGE_OPEN}{}{PAGE_CLOSE} ", &c[1])
            } else {
                " ".to_string()
            }
        });
    let html = FOOTNOTE_MARK_RE.as_ref()?.replace_all(&html, "");
    let html = PART_RE.as_ref()?.replace_all(&html, |c: &regex::Captures| {
        format!(" {PART_OPEN}{}{PART_CLOSE} ", &c[1])
    });
    let text = strip_markup(&html);
    let text = html_escape::decode_html_entities(&text).to_string();
    let mut op = tokenize(
        &text,
        if pages_known { Some(cited_page) } else { None },
        url,
    );
    op.pages_known = pages_known;
    (!op.words.is_empty()).then_some(op)
}

fn from_plain(text: &str, url: String) -> OpinionText {
    tokenize(text, None, url)
}

/// Walk the text once: strip markers into page/part state, and record each
/// word's byte span in the cleaned text so a match can be quoted back exactly.
fn tokenize(marked: &str, first_page: Option<u32>, url: String) -> OpinionText {
    let mut text = String::with_capacity(marked.len());
    let mut words = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut page = first_page;
    let mut part: Option<usize> = None;
    let mut chars = marked.chars().peekable();
    let mut cur = String::new();
    let mut cur_start = 0usize;

    let flush = |cur: &mut String,
                 start: usize,
                 end: usize,
                 words: &mut Vec<Word>,
                 page: Option<u32>,
                 part: Option<usize>| {
        let norm: String = cur
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if !norm.is_empty() {
            words.push(Word {
                norm,
                start,
                end,
                page,
                part,
            });
        }
        cur.clear();
    };

    while let Some(ch) = chars.next() {
        if ch == PAGE_OPEN || ch == PART_OPEN {
            let close = if ch == PAGE_OPEN {
                PAGE_CLOSE
            } else {
                PART_CLOSE
            };
            let mut label = String::new();
            for c in chars.by_ref() {
                if c == close {
                    break;
                }
                label.push(c);
            }
            // A word hyphenated across a page break ("cen- *170 trally") is
            // one word: keep it open and carry on after the marker.
            let split_word = ch == PAGE_OPEN && cur.ends_with('-');
            if !split_word {
                let end = text.len();
                flush(&mut cur, cur_start, end, &mut words, page, part);
            }
            if ch == PAGE_OPEN {
                if first_page.is_some() {
                    page = label.parse().ok().or(page);
                }
            } else {
                parts.push(label);
                part = Some(parts.len() - 1);
            }
            continue;
        }
        let is_word = ch.is_alphanumeric() || ch == '\'' || ch == '’' || ch == '-';
        if ch.is_whitespace() && cur.ends_with('-') {
            // Inside a hyphenated line or page break; the word continues.
            text.push(ch);
            continue;
        }
        if is_word {
            if cur.is_empty() {
                cur_start = text.len();
            }
            cur.push(ch);
        } else {
            let end = text.len();
            flush(&mut cur, cur_start, end, &mut words, page, part);
        }
        text.push(ch);
    }
    let end = text.len();
    flush(&mut cur, cur_start, end, &mut words, page, part);
    OpinionText {
        text,
        words,
        parts,
        pages_known: false,
        url,
    }
}

// ── alignment ──────────────────────────────────────────────────────────────

/// Normalised words of a string, the same way the opinion is tokenised.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’' || c == '-'))
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Split a quotation at its ellipses, and drop bracketed alterations: "[t]he"
/// keeps its letter, "[the Convention]" is the brief's own words and is not
/// expected in the opinion.
fn quote_segments(q: &str) -> Vec<Vec<String>> {
    static ELLIPSIS: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\.\s?\.\s?\.|…").ok());
    static BRACKET_WORDS: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\[[^\]]{2,}\]").ok());
    let Some(ell) = ELLIPSIS.as_ref() else {
        return vec![words(q)];
    };
    ell.split(q)
        .map(|seg| {
            let seg = BRACKET_WORDS
                .as_ref()
                .map(|r| r.replace_all(seg, " ").to_string())
                .unwrap_or_else(|| seg.to_string());
            let seg = seg.replace(['[', ']'], "");
            let mut w = words(&seg);
            w.truncate(MAX_QUOTE_WORDS);
            w
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Same word, allowing the OCR noise of digitised reporters.
fn same(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (la, lb) = (a.chars().count(), b.chars().count());
    if la < 5 || lb < 5 || la.abs_diff(lb) > 2 {
        return false;
    }
    let max = if la.max(lb) >= 8 { 2 } else { 1 };
    edit_distance(a, b) <= max
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

struct Alignment {
    start: usize,
    end: usize,
    matches: usize,
    /// Quotation words before and after the aligned stretch that found no
    /// partner — used to widen the reported passage to the whole quotation,
    /// so a divergent ending still shows the court's own words.
    head_missing: usize,
    tail_missing: usize,
}

/// Local alignment of the quotation (fully) against the best stretch of the
/// opinion. Word-level Smith-Waterman: a matching word scores, a substituted,
/// missing or extra word costs, and the best-scoring stretch wins. Each cell
/// carries its start and match count, so no traceback matrix is needed.
fn align(q: &[String], t: &[Word]) -> Option<Alignment> {
    const MATCH: i32 = 3;
    const MISMATCH: i32 = -2;
    const GAP: i32 = -2;
    let m = q.len();
    if m == 0 || t.is_empty() {
        return None;
    }
    #[derive(Clone, Copy, Default)]
    struct Cell {
        score: i32,
        start: usize,
        matches: usize,
        qstart: usize,
    }
    let mut prev = vec![Cell::default(); m + 1];
    let mut cur = vec![Cell::default(); m + 1];
    let mut best = Cell::default();
    let mut best_end = 0usize;
    let mut best_qend = 0usize;
    for (j, w) in t.iter().enumerate() {
        cur[0] = Cell {
            score: 0,
            start: j + 1,
            matches: 0,
            qstart: 0,
        };
        for i in 1..=m {
            let hit = same(&q[i - 1], &w.norm);
            let diag = prev[i - 1];
            let mut c = Cell {
                score: diag.score + if hit { MATCH } else { MISMATCH },
                start: if diag.score == 0 && diag.matches == 0 {
                    j
                } else {
                    diag.start
                },
                matches: diag.matches + usize::from(hit),
                qstart: if diag.score == 0 && diag.matches == 0 {
                    i - 1
                } else {
                    diag.qstart
                },
            };
            let up = cur[i - 1]; // quotation word missing from the opinion
            if up.score + GAP > c.score {
                c = Cell {
                    score: up.score + GAP,
                    ..up
                };
            }
            let left = prev[i]; // opinion has an extra word
            if left.score + GAP > c.score {
                c = Cell {
                    score: left.score + GAP,
                    ..left
                };
            }
            if c.score <= 0 {
                c = Cell {
                    score: 0,
                    start: j + 1,
                    matches: 0,
                    qstart: i,
                };
            }
            cur[i] = c;
            if c.score > best.score {
                best = c;
                best_end = j;
                best_qend = i - 1;
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    (best.matches > 0).then(|| Alignment {
        start: best.start.min(best_end),
        end: best_end,
        matches: best.matches,
        head_missing: best.qstart,
        tail_missing: m - 1 - best_qend.min(m - 1),
    })
}

fn clamp(s: &str, n: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n {
        t
    } else {
        format!("{}…", t.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: &str = r##"<section class="casebody" data-firstpage="155" data-lastpage="181">
      <article class="opinion" data-type="majority"><p>The Court of Appeals looked to our precedent. It further recognizes
      that the Convention <a id="p161" href="#p161" data-label="161" data-citation-index="1" class="page-label">*161</a>
      een-trally endeavors to foster uniformity in the law of international air travel.<a class="footnotemark" href="#f1">7</a>
      Recourse to local law would undermine the uniform regulation of international air carrier liability.</p></article>
      <article class="opinion" data-type="dissent"><p>I would hold that the Convention does not preclude a personal injury action.</p></article></section>"##;

    fn op() -> OpinionText {
        from_cap_html(CAP, 155, "u".into()).unwrap()
    }

    fn quote(text: &str, pin: Option<&str>) -> QuoteCheck {
        let q = RawQuote {
            start: 0,
            end: 0,
            text: text.into(),
        };
        let mut c = QuoteCheck {
            pin: pin.map(str::to_string),
            ..Default::default()
        };
        judge(&mut c, &q, &op(), None);
        c
    }

    #[test]
    fn verbatim_quote_through_ocr_noise_with_a_good_pin() {
        let c = quote(
            "centrally endeavors to foster uniformity in the law of international air travel",
            Some("161"),
        );
        assert_eq!(c.status, "verbatim", "{c:?}");
        assert_eq!(c.found_pages, vec![161]);
        assert_eq!(c.pin_check.as_deref(), Some("ok"));
        assert_eq!(c.opinion_part.as_deref(), Some("majority"));
    }

    #[test]
    fn a_wrong_pin_is_caught() {
        let c = quote(
            "Recourse to local law would undermine the uniform regulation",
            Some("158"),
        );
        assert_eq!(c.status, "verbatim");
        assert_eq!(c.pin_check.as_deref(), Some("mismatch"), "{c:?}");
        assert!(c.note.contains("page 161"), "{}", c.note);
    }

    #[test]
    fn an_invented_quote_is_not_found() {
        let c = quote(
            "the Convention's preemptive effect on local law extends to all claims arising from bankruptcy proceedings",
            None,
        );
        assert_eq!(c.status, "not_found", "{c:?}");
    }

    #[test]
    fn a_reworded_quote_is_close_and_shows_the_real_words() {
        let c = quote("Recourse to local law would undermine the uniform regulation of international airline liability", None);
        assert_eq!(c.status, "close", "{c:?}");
        assert!(c.found_text.unwrap().contains("air carrier liability"));
    }

    #[test]
    fn ellipses_and_brackets_are_honoured() {
        let c = quote("[R]ecourse to local law . . . undermine[s] the uniform regulation of international air carrier liability", None);
        assert_ne!(c.status, "not_found", "{c:?}");
        let segs = quote_segments("[t]he Convention [as amended] . . . applies");
        assert_eq!(
            segs,
            vec![
                vec!["the".to_string(), "convention".into()],
                vec!["applies".to_string()]
            ]
        );
    }

    #[test]
    fn a_dissent_is_flagged() {
        let c = quote(
            "the Convention does not preclude a personal injury action",
            None,
        );
        assert_eq!(c.opinion_part.as_deref(), Some("dissent"));
        assert!(c.note.contains("dissent"), "{}", c.note);
    }

    #[test]
    fn a_word_split_across_a_page_break_is_one_word() {
        let html = r##"<section data-firstpage="155"><p>the Convention een- <a data-label="170" data-citation-index="1" class="page-label">*170</a> trally endeavors to foster uniformity</p></section>"##;
        let op = from_cap_html(html, 155, "u".into()).unwrap();
        let w: Vec<&str> = op.words.iter().map(|w| w.norm.as_str()).collect();
        assert_eq!(
            w,
            vec![
                "the",
                "convention",
                "eentrally",
                "endeavors",
                "to",
                "foster",
                "uniformity"
            ]
        );
        assert_eq!(op.words[3].page, Some(170));
    }

    #[test]
    fn pin_ranges_expand_abbreviated_ends() {
        assert_eq!(pin_range("161"), Some((161, 161)));
        assert_eq!(pin_range("161-62"), Some((161, 162)));
        assert_eq!(pin_range("1099-1104"), Some((1099, 1104)));
    }

    #[test]
    fn attributes_quotes_to_following_short_and_named_cites() {
        let text = r#"El Al Israel Airlines, Ltd. v. Tsui Yuan Tseng, 525 U.S. 155 (1999). As the Court put it in Tseng, "recourse to local law would undermine the uniform regulation." 525 U.S. at 161. Later: "the Convention centrally endeavors to foster uniformity." Id. at 169. Finally "four words long here" Zicherman v. Korean Air Lines Co., 516 U.S. 217, 230 (1996)."#;
        let flat = flatten(text);
        let occ = occurrences(&flat);
        let fulls: Vec<String> = occ
            .iter()
            .filter(|o| matches!(o.kind, Kind::Full))
            .map(|o| o.cite.clone())
            .collect();
        assert_eq!(fulls, vec!["525 U.S. 155", "516 U.S. 217"]);
        let qs = extract_quotes(&flat);
        assert_eq!(qs.len(), 3);
        let (c, pin, how) = attribute(&flat, &qs[0], &occ, &fulls, Some("525 U.S. 155"), &[]);
        assert_eq!(
            (c.as_deref(), pin.as_deref(), how),
            (Some("525 U.S. 155"), Some("161"), "short cite")
        );
        let (c, pin, how) = attribute(&flat, &qs[1], &occ, &fulls, Some("525 U.S. 155"), &[]);
        assert_eq!(
            (c.as_deref(), pin.as_deref(), how),
            (Some("525 U.S. 155"), Some("169"), "id.")
        );
        let (c, pin, how) = attribute(&flat, &qs[2], &occ, &fulls, Some("525 U.S. 155"), &[]);
        assert_eq!(
            (c.as_deref(), pin.as_deref(), how),
            (Some("516 U.S. 217"), Some("230"), "following citation")
        );
    }

    #[test]
    fn a_string_cite_is_not_read_as_a_pin() {
        let occ = occurrences("Nken v. Holder, 556 U.S. 418, 612 F.3d 1099, 1104 (2010)");
        let fulls: Vec<_> = occ
            .iter()
            .filter(|o| matches!(o.kind, Kind::Full))
            .collect();
        assert_eq!(fulls.len(), 2);
        assert_eq!(fulls[0].pin, None);
        assert_eq!(fulls[1].pin.as_deref(), Some("1104"));
    }

    #[test]
    fn ocr_tolerance_has_limits() {
        assert!(same("centrally", "eentrally"));
        assert!(!same("liability", "stability") || edit_distance("liability", "stability") <= 2);
        assert!(!same("air", "aid"));
    }
}
