//! State statutes from official sites whose section pages sit at a predictable URL.
//!
//! Most legislatures publish their code as one server-rendered page per section
//! (or per chapter) at an address you can build from the citation. That is
//! enough for a generic fetcher: one URL rule per state, one shared extractor.
//! Each rule here was probed against the live site before it was added.
//!
//! The extractor is deliberately heuristic — navigation chrome differs on every
//! site, and a per-state parser for fifty layouts would never be finished. It
//! finds the occurrence of the section number that is followed by the most
//! prose and reads from there. That is right on every probed site, but it is a
//! heuristic, so every response carries the official `source_url` and a note
//! saying the text was extracted from a page rather than parsed from an API:
//! the reader can always check the page itself.

use super::{normalize_state, StateStatuteRequest, StateStatuteResponse};
use crate::legal::{clamp_text, shared_client};
use crate::{Error, Result};
use chrono::{Datelike, Utc};
use regex::Regex;
use std::sync::LazyLock;

const DEFAULT_MAX_CHARS: usize = 20_000;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// How the page addresses the law.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    /// One section per page.
    Section,
    /// One chapter per page; the section has to be sliced out of it.
    Chapter,
    /// South Dakota's JSON API: `{CatchLine, Html, ...}`.
    SdJson,
}

struct Rule {
    code: &'static str,
    name: &'static str,
    source: &'static str,
    page: Page,
    /// Example citation, shown in the error when a section can't be parsed.
    example: &'static str,
    url: fn(&str, Option<&str>) -> Option<String>,
}

pub(super) const STATES: &[&str] = &[
    "ak", "az", "dc", "fl", "ks", "me", "mi", "mn", "mo", "nc", "ne", "nv", "oh", "or", "ri", "sd",
    "va", "wa", "wv",
];

static RULES: &[Rule] = &[
    Rule {
        code: "ak",
        name: "Alaska",
        source: "Alaska Legislature (akleg.gov)",
        page: Page::Section,
        example: "09.10.070",
        url: |s, _| {
            Some(format!(
                "https://www.akleg.gov/basis/statutes.asp?media=print&secStart={s}&secEnd={s}"
            ))
        },
    },
    Rule {
        code: "az",
        name: "Arizona",
        source: "Arizona Legislature (azleg.gov)",
        page: Page::Section,
        example: "12-542",
        url: |s, _| {
            // 12-821.01 -> /ars/12/00821-01.htm; the dotted form 404s.
            let (title, rest) = s.split_once('-')?;
            let (num, suffix) = match rest.split_once('.') {
                Some((n, x)) => (n, format!("-{x}")),
                None => (rest, String::new()),
            };
            let num: u32 = num.parse().ok()?;
            Some(format!(
                "https://www.azleg.gov/ars/{title}/{num:05}{suffix}.htm"
            ))
        },
    },
    Rule {
        code: "dc",
        name: "District of Columbia",
        source: "DC Council Law Library (code.dccouncil.gov)",
        page: Page::Section,
        example: "12-301",
        url: |s, _| {
            Some(format!(
                "https://code.dccouncil.gov/us/dc/council/code/sections/{s}"
            ))
        },
    },
    Rule {
        code: "fl",
        name: "Florida",
        source: "Florida Senate (flsenate.gov)",
        page: Page::Section,
        example: "95.11",
        url: |s, _| {
            Some(format!(
                "https://www.flsenate.gov/Laws/Statutes/{}/{s}",
                Utc::now().year()
            ))
        },
    },
    Rule {
        code: "ks",
        name: "Kansas",
        source: "Kansas Revisor of Statutes (ksrevisor.gov)",
        page: Page::Section,
        example: "60-513",
        url: |s, _| {
            // 60-513 is chapter 60, article 5, section 13 -> ch60/060_005_0013.
            let (ch, rest) = s.split_once('-')?;
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let tail = &rest[digits.len()..];
            if digits.len() < 3 {
                return None;
            }
            let (art, sec) = digits.split_at(digits.len() - 2);
            let ch_n: u32 = ch.parse().ok()?;
            let art_n: u32 = art.parse().ok()?;
            let sec_n: u32 = sec.parse().ok()?;
            Some(format!("https://www.ksrevisor.gov/statutes/chapters/ch{ch_n}/{ch_n:03}_{art_n:03}_{sec_n:04}{tail}.html"))
        },
    },
    Rule {
        code: "me",
        name: "Maine",
        source: "Maine Legislature (legislature.maine.gov)",
        page: Page::Section,
        example: "14-752 (title 14, section 752) or chapter=\"14\" section=\"752\"",
        url: |s, ch| {
            let (title, sec) = match ch {
                Some(t) => (t.to_string(), s.to_string()),
                None => {
                    let (t, x) = s.split_once(['-', ' ', '§'])?;
                    (t.to_string(), x.trim_start_matches(['§', ' ']).to_string())
                }
            };
            Some(format!(
                "https://legislature.maine.gov/statutes/{title}/title{title}sec{sec}.html"
            ))
        },
    },
    Rule {
        code: "mi",
        name: "Michigan",
        source: "Michigan Legislature (legislature.mi.gov)",
        page: Page::Section,
        example: "600.5805",
        url: |s, _| {
            Some(format!(
                "https://www.legislature.mi.gov/Laws/MCL?objectName=mcl-{}",
                s.replace('.', "-")
            ))
        },
    },
    Rule {
        code: "mn",
        name: "Minnesota",
        source: "Minnesota Revisor of Statutes (revisor.mn.gov)",
        page: Page::Section,
        example: "541.07",
        url: |s, _| Some(format!("https://www.revisor.mn.gov/statutes/cite/{s}")),
    },
    Rule {
        code: "mo",
        name: "Missouri",
        source: "Missouri Revisor of Statutes (revisor.mo.gov)",
        page: Page::Section,
        example: "516.105",
        url: |s, _| {
            Some(format!(
                "https://revisor.mo.gov/main/OneSection.aspx?section={s}"
            ))
        },
    },
    Rule {
        code: "nc",
        name: "North Carolina",
        source: "North Carolina General Assembly (ncleg.gov)",
        page: Page::Section,
        example: "1-52",
        url: |s, _| {
            let (ch, _) = s.split_once('-')?;
            Some(format!("https://www.ncleg.gov/EnactedLegislation/Statutes/HTML/BySection/Chapter_{ch}/GS_{s}.html"))
        },
    },
    Rule {
        code: "ne",
        name: "Nebraska",
        source: "Nebraska Legislature (nebraskalegislature.gov)",
        page: Page::Section,
        example: "25-208",
        url: |s, _| {
            Some(format!(
                "https://nebraskalegislature.gov/laws/statutes.php?statute={s}"
            ))
        },
    },
    Rule {
        code: "nv",
        name: "Nevada",
        source: "Nevada Legislature (leg.state.nv.us)",
        page: Page::Chapter,
        example: "11.190",
        url: |s, _| {
            Some(format!(
                "https://www.leg.state.nv.us/NRS/NRS-{}.html",
                pad_chapter(s.split_once('.')?.0)
            ))
        },
    },
    Rule {
        code: "oh",
        name: "Ohio",
        source: "Ohio Legislative Service Commission (codes.ohio.gov)",
        page: Page::Section,
        example: "2305.10",
        url: |s, _| {
            Some(format!(
                "https://codes.ohio.gov/ohio-revised-code/section-{s}"
            ))
        },
    },
    Rule {
        code: "or",
        name: "Oregon",
        source: "Oregon Legislature (oregonlegislature.gov)",
        page: Page::Chapter,
        example: "12.110",
        url: |s, _| {
            Some(format!(
                "https://www.oregonlegislature.gov/bills_laws/ors/ors{}.html",
                pad_chapter(s.split_once('.')?.0)
            ))
        },
    },
    Rule {
        code: "ri",
        name: "Rhode Island",
        source: "Rhode Island General Assembly (rilegislature.gov)",
        page: Page::Section,
        example: "9-1-14",
        url: |s, _| {
            let mut parts = s.splitn(3, '-');
            let title = parts.next()?;
            let chapter = parts.next()?;
            parts.next()?;
            Some(format!("https://webserver.rilegislature.gov/Statutes/TITLE{title}/{title}-{chapter}/{s}.htm"))
        },
    },
    Rule {
        code: "sd",
        name: "South Dakota",
        source: "South Dakota Legislature (sdlegislature.gov API)",
        page: Page::SdJson,
        example: "15-2-14",
        url: |s, _| {
            Some(format!(
                "https://sdlegislature.gov/api/Statutes/Statute/{s}"
            ))
        },
    },
    Rule {
        code: "va",
        name: "Virginia",
        source: "Virginia Legislative Information System (law.lis.virginia.gov)",
        page: Page::Section,
        example: "8.01-243",
        url: |s, _| Some(format!("https://law.lis.virginia.gov/vacode/{s}/")),
    },
    Rule {
        code: "wa",
        name: "Washington",
        source: "Washington Legislature (app.leg.wa.gov)",
        page: Page::Section,
        example: "4.16.080",
        url: |s, _| Some(format!("https://app.leg.wa.gov/RCW/default.aspx?cite={s}")),
    },
    Rule {
        code: "wv",
        name: "West Virginia",
        source: "West Virginia Legislature (code.wvlegislature.gov)",
        page: Page::Section,
        example: "55-2-12",
        url: |s, _| Some(format!("https://code.wvlegislature.gov/{s}/")),
    },
];

/// `12` -> `012`, `41A` -> `041A`: Oregon and Nevada pad the chapter number.
fn pad_chapter(ch: &str) -> String {
    let digits: String = ch.chars().take_while(|c| c.is_ascii_digit()).collect();
    let tail = &ch[digits.len()..];
    match digits.parse::<u32>() {
        Ok(n) => format!("{n:03}{tail}"),
        Err(_) => ch.to_string(),
    }
}

pub(super) fn name(code: &str) -> Option<&'static str> {
    RULES.iter().find(|r| r.code == code).map(|r| r.name)
}

/// Normalise a citation as a model will type it: "§ 95.11", "Fla. Stat. 95.11",
/// "RCW 4.16.080". Keep the last token that looks like a section number.
fn clean_section(raw: &str) -> String {
    static SEC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[0-9][0-9A-Za-z]*(?:[.\-:][0-9A-Za-z]+)*").unwrap());
    let raw = raw.trim();
    SEC.find_iter(raw)
        .last()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| raw.to_string())
}

pub(super) async fn fetch_statute(req: StateStatuteRequest) -> Result<StateStatuteResponse> {
    let code = normalize_state(&req.state);
    let rule = RULES
        .iter()
        .find(|r| r.code == code)
        .ok_or_else(|| Error::InvalidInput(format!("no templated statute source for {code:?}")))?;
    // Maine's "14-752" carries the title in the section, so only strip
    // decoration there; elsewhere take the bare section number.
    let section = if code == "me" && req.chapter.is_none() {
        req.section
            .trim()
            .trim_start_matches(['§', ' '])
            .to_string()
    } else {
        clean_section(&req.section)
    };
    let url = (rule.url)(&section, req.chapter.as_deref()).ok_or_else(|| {
        Error::InvalidInput(format!(
            "could not build a {} statute address from section {:?}. Expected e.g. {}.",
            rule.name, req.section, rule.example
        ))
    })?;

    let mut out = StateStatuteResponse {
        generated_at: Utc::now(),
        state: code.clone(),
        citation: Some(format!("{} § {section}", rule.name)),
        heading: None,
        text: None,
        chars: 0,
        truncated: false,
        repealed: None,
        source: Some(rule.source.to_string()),
        source_url: Some(url.clone()),
        warnings: vec![
            "text extracted from the official web page, not a structured API; check source_url if the \
             wording matters (pin cites, subsection numbering)"
                .to_string(),
        ],
    };

    let Some(body) = get(&url, &mut out.warnings).await else {
        return Ok(out);
    };
    let max_chars = if req.max_chars == 0 {
        DEFAULT_MAX_CHARS
    } else {
        req.max_chars
    };

    let (heading, text) = match rule.page {
        Page::SdJson => match sd_json(&body) {
            Some(x) => x,
            None => {
                out.warnings.push("sdlegislature: response was not the expected JSON — section probably does not exist".into());
                return Ok(out);
            }
        },
        Page::Section | Page::Chapter => {
            let page_text = page_to_text(&body);
            let display = if code == "me" {
                section
                    .rsplit(['-', ' ', '§'])
                    .next()
                    .unwrap_or(&section)
                    .to_string()
            } else {
                section.clone()
            };
            match extract(&page_text, &display, rule.page == Page::Chapter) {
                Some(t) => (None, t),
                None => {
                    out.warnings.push(format!(
                        "the page loaded but section {display} does not appear in it — the citation is probably wrong \
                         or the section is not in this chapter. Expected format: {}",
                        rule.example
                    ));
                    return Ok(out);
                }
            }
        }
    };

    if text.trim().len() < 40 {
        out.warnings
            .push("the source returned almost no text for this section".into());
    }
    out.repealed = Some(looks_repealed(&text));
    out.heading = heading.or_else(|| first_sentence(&text, &section));
    out.chars = text.chars().count();
    let (clamped, truncated) = clamp_text(&text, max_chars);
    out.text = Some(clamped);
    out.truncated = truncated;
    Ok(out)
}

async fn get(url: &str, warnings: &mut Vec<String>) -> Option<String> {
    let mut resp = None;
    for attempt in 0..2 {
        match shared_client::GENERAL.get(url).send().await {
            // One polite retry on a rate limit; these are small legislative
            // servers and a second burst would only make it worse.
            Ok(r) if r.status().as_u16() == 429 && attempt == 0 => {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(e) => {
                warnings.push(format!("request failed: {e}"));
                return None;
            }
        }
    }
    let resp = resp?;
    let status = resp.status();
    if !status.is_success() {
        warnings.push(format!(
            "HTTP {status} from the official site{}",
            if status.as_u16() == 404 {
                " — the section probably does not exist at that number"
            } else {
                ""
            }
        ));
        return None;
    }
    if resp
        .content_length()
        .is_some_and(|n| n as usize > MAX_BODY_BYTES)
    {
        warnings.push("page too large to read".into());
        return None;
    }
    // `.text()` honours the charset header — Alaska serves ISO-8859-1 and
    // Oregon windows-1252, both of which mangle as UTF-8.
    match resp.text().await {
        Ok(t) => Some(t),
        Err(e) => {
            warnings.push(format!("body read failed: {e}"));
            None
        }
    }
}

/// HTML to plain text with the page furniture removed.
fn page_to_text(html: &str) -> String {
    static FURNITURE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)<(script|style|nav|header|footer|noscript|select)\b.*?</(script|style|nav|header|footer|noscript|select)>")
            .unwrap()
    });
    let html = FURNITURE.replace_all(html, " ");
    let text = crate::legal::strip_markup(&html);
    let text = html_escape::decode_html_entities(&text);
    // Word-exported pages (Oregon, Nevada) pad with non-breaking spaces and
    // replacement characters between a section number and its catchline.
    // DC prints section numbers with a non-breaking hyphen (12‑301), which a
    // citation typed with a plain hyphen would never match.
    text.replace(
        ['\u{a0}', '\u{fffd}', '\u{2002}', '\u{2003}', '\u{2009}'],
        " ",
    )
    .replace(['\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}'], "-")
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}

/// Find the body of `section` in a page's text.
///
/// The section number appears several times on a typical page — breadcrumb,
/// title bar, previous/next links, history notes — and only one occurrence
/// starts the law. Score each occurrence by how much running prose follows it
/// and take the best. On a chapter page, stop at the next section of the same
/// chapter.
fn extract(text: &str, section: &str, chapter_page: bool) -> Option<String> {
    let needle = section;
    // (start, heading-like, score). A heading-like occurrence is followed by a
    // catchline ("60-513. Actions limited...") rather than running text, which
    // is what separates the section's own start from a cross-reference to it
    // ("...in NRS 11.190 shall be deemed...") or a history note ("60-513; L. 1968").
    let mut candidates: Vec<(usize, bool, usize)> = Vec::new();
    let mut from = 0;
    while let Some(off) = text[from..].find(needle) {
        let at = from + off;
        from = at + needle.len();
        // Must not be the middle of a longer number (12.110 inside 12.1100).
        let before = text[..at].chars().next_back();
        let after = text[from..].chars().next();
        if before.is_some_and(|c| c.is_ascii_alphanumeric())
            || after.is_some_and(|c| c.is_ascii_digit() || c.is_ascii_alphabetic())
        {
            continue;
        }
        // Score only up to the next mention of the same number: a breadcrumb
        // or link list is followed almost at once by the number again, the
        // law itself is followed by its own text.
        let window_end = char_floor(text, at + 600);
        let until_next = text[from..window_end]
            .find(needle)
            .map(|i| from + i)
            .unwrap_or(window_end);
        let score = prose_score(&text[at..until_next]);
        let heading = heading_like(&text[from..]);
        candidates.push((at, heading, score));
    }
    // Prefer heading-like occurrences; among those, the most prose wins.
    let pool: Vec<_> = if candidates.iter().any(|c| c.1) {
        candidates.iter().filter(|c| c.1).collect()
    } else {
        candidates.iter().collect()
    };
    let start = pool.iter().max_by_key(|c| (c.2, std::cmp::Reverse(c.0)))?.0;
    let mut body = &text[start..];
    if chapter_page {
        if let Some(end) = next_section_start(body, section) {
            body = &body[..end];
        }
    }
    Some(body.trim().to_string())
}

fn heading_like(after: &str) -> bool {
    // At least one separator, so "60-513(b)" (a subsection cite in an
    // annotation) does not count as the section starting.
    let rest = after.trim_start_matches([' ', '.', ':', '|', '-', '—', '–']);
    rest.len() < after.len()
        && rest
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase() || c == '(')
}

fn char_floor(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Lower-case words and sentence punctuation: statute text has plenty, menus
/// and link lists have almost none.
fn prose_score(s: &str) -> usize {
    let words = s
        .split_whitespace()
        .filter(|w| {
            w.len() > 2
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || matches!(c, ',' | ';' | '.'))
        })
        .count();
    words + s.matches(". ").count() * 2 + s.matches(';').count()
}

/// On a chapter page, where does the next section begin? Sections start with
/// the chapter number, a dot, and digits, followed by a capitalised catchline.
fn next_section_start(body: &str, section: &str) -> Option<usize> {
    let chapter = section.split('.').next()?;
    let re = Regex::new(&format!(
        r"(?:NRS )?\b{}\.\d+[A-Za-z]?\b\s+[A-Z]",
        regex::escape(chapter)
    ))
    .ok()?;
    let own = format!("{section} ");
    let next = re.find_iter(body).map(|m| m.start()).find(|&i| {
        i > section.len() + 40 && !body[i..].trim_start_matches("NRS ").starts_with(&own)
    });
    next
}

fn sd_json(body: &str) -> Option<(Option<String>, String)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let html = v.get("Html").and_then(|h| h.as_str()).unwrap_or_default();
    let heading = v
        .get("CatchLine")
        .and_then(|c| c.as_str())
        .map(|c| c.trim().to_string());
    Some((heading, page_to_text(html)))
}

fn looks_repealed(text: &str) -> bool {
    let head: String = text
        .chars()
        .take(300)
        .collect::<String>()
        .to_ascii_lowercase();
    head.contains("repealed") || head.contains("[repealed") || head.contains("(repealed")
}

/// The catchline is usually the first sentence after the section number.
fn first_sentence(text: &str, section: &str) -> Option<String> {
    let rest = text.split_once(section).map(|(_, r)| r).unwrap_or(text);
    let rest = rest.trim_start_matches(['.', ' ', '-', '—', ':']);
    let end = rest
        .find(". ")
        .map(|i| i + 1)
        .unwrap_or(rest.len().min(160));
    let s: String = rest[..end].chars().take(200).collect();
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(code: &str, sec: &str, ch: Option<&str>) -> Option<String> {
        let r = RULES.iter().find(|r| r.code == code).unwrap();
        (r.url)(sec, ch)
    }

    #[test]
    fn every_listed_state_has_a_rule() {
        for s in STATES {
            assert!(
                RULES.iter().any(|r| r.code == *s),
                "{s} listed without a rule"
            );
        }
        assert_eq!(STATES.len(), RULES.len());
    }

    #[test]
    fn builds_the_probed_addresses() {
        assert_eq!(
            url("az", "12-542", None).unwrap(),
            "https://www.azleg.gov/ars/12/00542.htm"
        );
        assert_eq!(
            url("az", "12-821.01", None).unwrap(),
            "https://www.azleg.gov/ars/12/00821-01.htm"
        );
        assert_eq!(
            url("ks", "60-513", None).unwrap(),
            "https://www.ksrevisor.gov/statutes/chapters/ch60/060_005_0013.html"
        );
        assert_eq!(
            url("ks", "60-1901", None).unwrap(),
            "https://www.ksrevisor.gov/statutes/chapters/ch60/060_019_0001.html"
        );
        assert_eq!(
            url("mi", "600.5805", None).unwrap(),
            "https://www.legislature.mi.gov/Laws/MCL?objectName=mcl-600-5805"
        );
        assert_eq!(
            url("ri", "9-1-14", None).unwrap(),
            "https://webserver.rilegislature.gov/Statutes/TITLE9/9-1/9-1-14.htm"
        );
        assert_eq!(
            url("or", "12.110", None).unwrap(),
            "https://www.oregonlegislature.gov/bills_laws/ors/ors012.html"
        );
        assert_eq!(
            url("nv", "41A.035", None).unwrap(),
            "https://www.leg.state.nv.us/NRS/NRS-041A.html"
        );
        assert_eq!(
            url("me", "14-752", None).unwrap(),
            "https://legislature.maine.gov/statutes/14/title14sec752.html"
        );
        assert_eq!(
            url("me", "752", Some("14")).unwrap(),
            "https://legislature.maine.gov/statutes/14/title14sec752.html"
        );
    }

    #[test]
    fn cleans_citations_the_way_a_model_types_them() {
        assert_eq!(clean_section("Fla. Stat. § 95.11"), "95.11");
        assert_eq!(clean_section("RCW 4.16.080"), "4.16.080");
        assert_eq!(clean_section("N.C. Gen. Stat. 1-52"), "1-52");
        assert_eq!(clean_section("2305.10"), "2305.10");
    }

    #[test]
    fn picks_the_body_not_the_breadcrumb() {
        let page = "Home > Title 4 > 4.16.080 Print 4.16.070 << 4.16.080 >> 4.16.090 \
                    RCW 4.16.080 Actions limited to three years. The following actions shall be \
                    commenced within three years: (1) an action for waste or trespass upon real property; \
                    (2) an action for taking, detaining, or injuring personal property.";
        let body = extract(page, "4.16.080", false).unwrap();
        assert!(body.starts_with("4.16.080 Actions limited"), "{body}");
    }

    #[test]
    fn slices_one_section_out_of_a_chapter_page() {
        let page = "12.100 Action on official act 12.110 Actions for certain injuries 12.115 Action for \
                    negligent injury 12.110 Actions for certain injuries to person. (1) An action for \
                    assault, battery, false imprisonment, or for any injury to the person or rights of \
                    another, not arising on contract, shall be commenced within two years. 12.115 Action \
                    for negligent injury to person or property. (1) In no event shall any action be commenced.";
        let body = extract(page, "12.110", true).unwrap();
        assert!(body.contains("within two years"), "{body}");
        assert!(!body.contains("12.115"), "{body}");
    }

    #[test]
    fn prefers_the_heading_over_a_history_note_or_cross_reference() {
        let page = "60-513 Revisor 60-513. Actions limited to two years. (a) The following actions shall be \
                    brought within two years. History: 60-513; L. 1968, ch. 6, § 1; L. 1976, ch. 254, § 1; \
                    L. 1987, ch. 222, § 1; and a long note, with words, that the scorer would like; very much.";
        let body = extract(page, "60-513", false).unwrap();
        assert!(body.starts_with("60-513. Actions limited"), "{body}");
        let nv = "NRS 11.190 Periods of limitation. Actions may only be commenced as follows. More text here \
                  NRS 11.200 Computation of time. The time in NRS 11.190 shall be deemed to date from the last \
                  transaction or the last item charged or last credit given, and so on, with plenty of words.";
        let body = extract(nv, "11.190", true).unwrap();
        assert!(body.starts_with("11.190 Periods"), "{body}");
        assert!(!body.contains("Computation"), "{body}");
    }

    #[test]
    fn survives_multibyte_text_near_the_window_edge() {
        let page = format!(
            "12-301 {} § 12-301. Time limitations. Actions shall be brought.",
            "§ ".repeat(400)
        );
        assert!(extract(&page, "12-301", false).is_some());
    }

    #[test]
    fn does_not_match_inside_a_longer_number() {
        let page =
            "12.1100 Something else entirely, with plenty of words in it for the scorer to like.";
        assert!(extract(page, "12.110", false).is_none());
    }
}
