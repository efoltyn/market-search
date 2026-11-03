//! Section-to-section comparison between two filings (this 10-Q's risk
//! factors against last quarter's).
//!
//! Deterministic text comparison only, and every string returned is exact
//! text from one of the two documents. The unit is the prose paragraph
//! (table rows are numbers; xbrl covers them), located by the sentences in it:
//!
//! 1. Identical after normalizing whitespace, quotes, dashes and case:
//!    unchanged, wherever it sits in the section (a moved paragraph is not
//!    a change).
//! 2. Identical after masking numbers, dates and period words ("third
//!    quarter", "June 27, 2026", "$4.2 billion", "12%"): figures_only, counted.
//! 3. The rest are paired by shared words (one-to-one, best pair first). A
//!    sentence counts as new only when no sentence of the whole base section
//!    masks to the same text, and as removed only when none of this section's
//!    does, so a sentence moved between paragraphs is neither.

use super::super::*;
use super::html::{BlockKind, ParsedDoc};
use super::sections::title_key;
use regex::Regex;
use std::sync::LazyLock;

/// One prose paragraph of a section.
#[derive(Clone, Debug)]
pub(crate) struct Para {
    pub text: String,
    /// Nearest subheading above it inside the section.
    pub under: Option<String>,
}

/// Prose paragraphs of blocks `[start, end)`: data-table rows, page furniture
/// and page breaks left out; a sentence broken across a page break joined.
pub(crate) fn section_paragraphs(doc: &ParsedDoc, start: usize, end: usize) -> Vec<Para> {
    let end = end.min(doc.blocks.len());
    // The first copy of a running header ("Item 1A" atop each page) is not
    // flagged as furniture; its text matches the copies that are.
    let mut running: HashMap<String, usize> = HashMap::new();
    for b in doc.blocks.iter().filter(|b| b.furniture && b.text.chars().count() <= 80) {
        *running.entry(title_key(&b.text)).or_default() += 1;
    }
    let mut out: Vec<Para> = Vec::new();
    let mut heading: Option<String> = None;
    let mut page_gap = false;
    for b in doc.blocks.iter().take(end).skip(start) {
        let running_copy = || b.text.chars().count() <= 80 && running.get(&title_key(&b.text)).is_some_and(|&n| n >= 2);
        // A bare "Item 1B, 1C" line is a page header, never prose.
        if b.furniture || b.kind == BlockKind::PageBreak || (page_gap && running_copy()) || BARE_ITEM_RE.is_match(b.text.trim()) {
            page_gap = true;
            continue;
        }
        if b.kind == BlockKind::Row && !b.layout {
            page_gap = false;
            continue;
        }
        let text = b.text.trim();
        if !text.chars().any(char::is_alphabetic) {
            continue;
        }
        // A sentence cut by a page break: the previous block stops without
        // punctuation and is longer than a short title (typesetters rarely
        // leave a heading at the foot of a page), or this one starts in lowercase.
        if page_gap {
            if let Some(prev) = out.last_mut() {
                let mid_sentence = !ends_sentence(&prev.text)
                    && (prev.text.split_whitespace().count() > 6 || text.chars().next().is_some_and(char::is_lowercase));
                if mid_sentence {
                    // Not a heading after all.
                    if heading.as_deref() == Some(prev.text.as_str()) {
                        heading = prev.under.clone();
                    }
                    prev.text.push(' ');
                    prev.text.push_str(text);
                    page_gap = false;
                    continue;
                }
            }
        }
        page_gap = false;
        out.push(Para { text: text.to_string(), under: heading.clone() });
        if is_heading(text) {
            heading = Some(text.to_string());
        }
    }
    out
}

fn ends_sentence(t: &str) -> bool {
    let t = t.trim_end_matches(['"', '\'', '”', '’', ')']);
    t.ends_with(['.', '!', '?', ':', ';'])
}

/// A short title line ("Macroeconomic and Industry Risks", "Liquidity and
/// Capital Resources"), not a sentence.
fn is_heading(t: &str) -> bool {
    t.chars().count() <= 120
        && t.split_whitespace().count() <= 16
        && !t.trim_end().ends_with(['.', ',', ';', ':'])
        && t.chars().next().is_some_and(|c| c.is_uppercase() || c.is_ascii_digit())
}

/// Whitespace collapsed; curly quotes, dashes and non-breaking spaces made
/// plain; trademark signs dropped; lowercase. Comparison key only, never returned.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' | '`' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201F}' | '\u{2033}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' => '-',
            '®' | '™' | '©' | '℠' => continue,
            c if c.is_whitespace() => ' ',
            c => c,
        };
        if c == ' ' {
            if !space {
                out.push(' ');
            }
            space = true;
            continue;
        }
        // "word ," -> "word,": a layout space before punctuation.
        if matches!(c, ',' | '.' | ';' | ':' | ')' | '%') && out.ends_with(' ') {
            out.pop();
        }
        out.extend(c.to_lowercase());
        space = false;
    }
    out.trim_end().to_string()
}

static BARE_ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:part\s+[ivx1-4]+\s*[,.\-–—]?\s*)?items?\s+\d{1,2}[a-d]?(?:\s*(?:,|and|&)\s*\d{1,2}[a-d]?)*\.?$")
        .expect("bare item regex")
});
static MONTH_DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // "may" only before a day number: otherwise it is the verb.
    Regex::new(r"\b(?:january|february|march|april|june|july|august|september|october|november|december)\b|\bmay(\s+\d)")
        .expect("month regex")
});
static PERIOD_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:first|second|third|fourth)(\s+(?:fiscal\s+)?(?:quarter|half))\b|\bq[1-4]\b").expect("period regex")
});
static NUMBER_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve)\b").expect("number word regex")
});
static FIGURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\(?[$€£¥]?\s?\d[\d,]*(?:\.\d+)?\)?(?:\s*(?:%|percent\b|percentage points?\b|basis points?\b|bps\b))?(?:\s*(?:thousand|million|billion|trillion)\b)?",
    )
    .expect("figure regex")
});

static FIGURE_RUN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:‹d›\s*)?#(?:\s*(?:,|;|and|or|to|through|-)?\s*(?:and\s+)?(?:‹d›\s*)?#)*").expect("figure run regex")
});

/// `normalize`d text with every figure, date and period word replaced by a
/// placeholder: "the third quarter of 2026, up 12%" -> "the ‹p› quarter of #, up #".
/// A run of them ("December 31, 2025 and September 30, 2026"; "$21.3 billion
/// and $22.5 billion") is one placeholder, so a list that grows by a period
/// is still figures only.
fn mask(norm: &str) -> String {
    let s = MONTH_DATE_RE.replace_all(norm, "‹d›$1");
    let s = PERIOD_WORD_RE.replace_all(&s, "‹p›$1");
    let s = NUMBER_WORD_RE.replace_all(&s, "#");
    let s = FIGURE_RE.replace_all(&s, "#");
    FIGURE_RUN_RE.replace_all(&s, "#").into_owned()
}

/// Words that end in a period without ending a sentence.
const ABBREVIATIONS: &[&str] = &[
    "inc", "corp", "co", "ltd", "llc", "plc", "no", "nos", "vs", "mr", "ms", "mrs", "dr", "st", "jr", "sr", "jan", "feb",
    "mar", "apr", "jun", "jul", "aug", "sep", "sept", "oct", "nov", "dec", "approx", "fig", "sec", "art", "para", "dept",
    "est", "u.s", "e.g", "i.e", "u.k", "n.a", "s.a", "n.v", "l.p", "et al",
];

/// Sentences of a paragraph, exact text. A period ends a sentence only when
/// followed by a space and a capital, digit, quote or bracket, and the word
/// before it is not an abbreviation (Inc., No., e.g.) or a dotted initialism
/// (U.S., L.P.) or a single letter (an initial).
pub(crate) fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        let (_, c) = chars[i];
        if matches!(c, '.' | '!' | '?') {
            // Closing quotes / brackets stay with the sentence.
            let mut j = i + 1;
            while j < chars.len() && matches!(chars[j].1, '"' | '\'' | '”' | '’' | ')') {
                j += 1;
            }
            let mut k = j;
            while k < chars.len() && chars[k].1.is_whitespace() {
                k += 1;
            }
            let boundary = k > j
                && k < chars.len()
                && {
                    let n = chars[k].1;
                    n.is_uppercase() || n.is_ascii_digit() || matches!(n, '"' | '“' | '(' | '[' | '•')
                }
                && (c != '.' || !abbreviation_before(text, chars[i].0));
            if boundary {
                let end = chars.get(j).map(|x| x.0).unwrap_or(text.len());
                let s = text[start..end].trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
                start = chars[k].0;
                i = k;
                continue;
            }
        }
        i += 1;
    }
    let s = text[start..].trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out
}

/// Is the word ending at byte `dot` (a '.') an abbreviation?
fn abbreviation_before(text: &str, dot: usize) -> bool {
    let before = &text[..dot];
    let word_start = before
        .rfind(|c: char| c.is_whitespace() || c == '(' || c == '"' || c == '“')
        .map(|p| p + before[p..].chars().next().map(char::len_utf8).unwrap_or(1))
        .unwrap_or(0);
    let word = before[word_start..].to_ascii_lowercase();
    if word.is_empty() {
        return false;
    }
    // "U.S", "L.P", "e.g": an inner dot; "A": an initial.
    if word.contains('.') || (word.chars().count() == 1 && word.chars().all(|c| c.is_ascii_alphabetic())) {
        return true;
    }
    if ABBREVIATIONS.contains(&word.as_str()) {
        return true;
    }
    // "et al."
    word == "al" && before[..word_start].trim_end().to_ascii_lowercase().ends_with("et")
}

struct Prepared<'a> {
    para: &'a Para,
    norm: String,
    masked: String,
    /// (exact sentence, masked key)
    sentences: Vec<(String, String)>,
    words: HashSet<String>,
}

fn prepare(p: &Para) -> Prepared<'_> {
    let norm = normalize(&p.text);
    let masked = mask(&norm);
    let sentences = split_sentences(&p.text)
        .into_iter()
        .map(|s| {
            let k = mask(&normalize(&s));
            (s, k)
        })
        .collect();
    let words = masked
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(str::to_string)
        .collect();
    Prepared { para: p, norm, masked, sentences, words }
}

/// Shared words over total words (Dice). Used only to pair a changed
/// paragraph with its earlier version; never reported.
fn overlap(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.iter().filter(|w| b.contains(*w)).count();
    2.0 * shared as f64 / (a.len() + b.len()) as f64
}

const PAIR_MIN_OVERLAP: f64 = 0.5;

/// The paragraph's first words, for locating a change.
fn opening_words(text: &str, n: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= n {
        words.join(" ")
    } else {
        format!("{} ...", words[..n].join(" "))
    }
}

/// Compare a section's paragraphs (`cur`) against the base's. `list_removed`
/// false: base paragraphs absent from `cur` are counted in `not_restated`
/// instead of listed (a 10-Q that restates only the risk factors it changed).
/// `base` and `chars` are left for the caller.
pub(crate) fn diff_paragraphs(cur: &[Para], base: &[Para], list_removed: bool) -> FilingSectionDiff {
    let cur: Vec<Prepared> = cur.iter().map(prepare).collect();
    let base: Vec<Prepared> = base.iter().map(prepare).collect();
    let mut cur_pair: Vec<Option<usize>> = vec![None; cur.len()];
    let mut base_used = vec![false; base.len()];
    let mut diff = FilingSectionDiff { paragraphs: [cur.len(), base.len()], ..Default::default() };

    // 1. identical text, anywhere in the section.
    let mut by_norm: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, b) in base.iter().enumerate().rev() {
        by_norm.entry(b.norm.as_str()).or_default().push(j);
    }
    for (i, c) in cur.iter().enumerate() {
        if let Some(j) = by_norm.get_mut(c.norm.as_str()).and_then(|v| v.pop()) {
            cur_pair[i] = Some(j);
            base_used[j] = true;
            diff.unchanged += 1;
        }
    }
    // 2. identical once figures are masked.
    let mut by_mask: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, b) in base.iter().enumerate().rev().filter(|(j, _)| !base_used[*j]) {
        by_mask.entry(b.masked.as_str()).or_default().push(j);
    }
    let mut figures_under: Vec<String> = Vec::new();
    for (i, c) in cur.iter().enumerate() {
        if cur_pair[i].is_some() {
            continue;
        }
        if let Some(j) = by_mask.get_mut(c.masked.as_str()).and_then(|v| v.pop()) {
            cur_pair[i] = Some(j);
            base_used[j] = true;
            diff.figures_only += 1;
            let at = c.para.under.clone().unwrap_or_else(|| opening_words(&c.para.text, 8));
            if !figures_under.contains(&at) {
                figures_under.push(at);
            }
        }
    }
    diff.figures_only_under = figures_under;

    // 3. pair the rest by shared words, best pair first (ties: document order).
    // Templated paragraphs ("North America sales increased ..." / "International
    // sales increased ...") pair with their own earlier version: same opening
    // words or same subheading break near-ties.
    let opening = |p: &Prepared| p.masked.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
    let mut cands: Vec<(f64, usize, usize)> = Vec::new();
    for (i, c) in cur.iter().enumerate().filter(|(i, _)| cur_pair[*i].is_none()) {
        for (j, b) in base.iter().enumerate().filter(|(j, _)| !base_used[*j]) {
            let s = overlap(&c.words, &b.words);
            if s >= PAIR_MIN_OVERLAP {
                let mut score = s;
                if opening(c) == opening(b) {
                    score += 0.2;
                }
                if c.para.under.is_some() && c.para.under == b.para.under {
                    score += 0.1;
                }
                cands.push((score, i, j));
            }
        }
    }
    cands.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut cur_sim: Vec<Option<usize>> = vec![None; cur.len()];
    let mut base_sim: Vec<Option<usize>> = vec![None; base.len()];
    for (_, i, j) in cands {
        if cur_sim[i].is_none() && base_sim[j].is_none() {
            cur_sim[i] = Some(j);
            base_sim[j] = Some(i);
        }
    }

    // Sentence keys of each whole section: a sentence is new / removed only
    // if the other section has no sentence with the same masked text.
    let base_keys: HashSet<&str> = base.iter().flat_map(|b| b.sentences.iter().map(|s| s.1.as_str())).collect();
    let cur_keys: HashSet<&str> = cur.iter().flat_map(|c| c.sentences.iter().map(|s| s.1.as_str())).collect();
    let new_in = |p: &Prepared| -> Vec<String> {
        p.sentences.iter().filter(|s| !base_keys.contains(s.1.as_str())).map(|s| s.0.clone()).collect()
    };
    let gone_in = |p: &Prepared| -> Vec<String> {
        p.sentences.iter().filter(|s| !cur_keys.contains(s.1.as_str())).map(|s| s.0.clone()).collect()
    };

    for (i, c) in cur.iter().enumerate() {
        if cur_pair[i].is_some() {
            continue;
        }
        let added = new_in(c);
        let removed = cur_sim[i].map(|j| gone_in(&base[j])).unwrap_or_default();
        if added.is_empty() && removed.is_empty() {
            diff.moved += 1;
        } else if cur_sim[i].is_none() && added.len() == c.sentences.len() {
            diff.added.push(FilingDiffParagraph { under: c.para.under.clone(), text: c.para.text.clone() });
        } else {
            diff.changed.push(FilingDiffChange {
                under: c.para.under.clone(),
                at: opening_words(&c.para.text, 15),
                added_sentences: added,
                removed_sentences: removed,
            });
        }
    }
    let mut not_restated = 0usize;
    for (j, b) in base.iter().enumerate() {
        if base_used[j] || base_sim[j].is_some() {
            continue;
        }
        let gone = gone_in(b);
        if gone.is_empty() {
            continue; // its sentences live on elsewhere: counted on the current side
        }
        if !list_removed {
            not_restated += 1;
        } else if gone.len() == b.sentences.len() {
            diff.removed.push(FilingDiffParagraph { under: b.para.under.clone(), text: b.para.text.clone() });
        } else {
            diff.changed.push(FilingDiffChange {
                under: b.para.under.clone(),
                at: format!("(base) {}", opening_words(&b.para.text, 15)),
                added_sentences: Vec::new(),
                removed_sentences: gone,
            });
        }
    }
    if !list_removed {
        diff.not_restated = Some(not_restated);
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paras(texts: &[&str]) -> Vec<Para> {
        texts.iter().map(|t| Para { text: t.to_string(), under: None }).collect()
    }

    #[test]
    fn sentence_split_keeps_abbreviations_together() {
        let s = split_sentences(
            "The Company sells in the U.S. and abroad through Example Inc. and its affiliates, e.g. resellers. \
             See Note No. 5 for details. Revenue grew. J. Smith joined in 2024.",
        );
        assert_eq!(
            s,
            vec![
                "The Company sells in the U.S. and abroad through Example Inc. and its affiliates, e.g. resellers.",
                "See Note No. 5 for details.",
                "Revenue grew.",
                "J. Smith joined in 2024.",
            ]
        );
        // Decimals and closing quotes.
        let s = split_sentences("Margin was 45.2% (the \"Margin.\") Costs rose.");
        assert_eq!(s, vec!["Margin was 45.2% (the \"Margin.\")", "Costs rose."]);
    }

    #[test]
    fn figures_and_period_words_only_are_not_reported() {
        let base = paras(&[
            "Net sales for the second quarter of 2026 were $94.0 billion, up 8% compared to the three months ended March 29, 2025.",
        ]);
        let cur = paras(&[
            "Net sales for the third quarter of 2026 were $97.3 billion, up 10% compared to the nine months ended June 28, 2025.",
        ]);
        let d = diff_paragraphs(&cur, &base, true);
        assert_eq!(d.figures_only, 1);
        assert!(d.added.is_empty() && d.removed.is_empty() && d.changed.is_empty());
        // A real word change is not figures-only.
        let cur = paras(&[
            "Net sales for the third quarter of 2026 were $97.3 billion, down 10% compared to the nine months ended June 28, 2025.",
        ]);
        let d = diff_paragraphs(&cur, &base, true);
        assert_eq!(d.figures_only, 0);
        assert_eq!(d.changed.len(), 1);
        // A list of figures that grows by one period.
        let base = paras(&["Capitalized video costs as of December 31, 2025 and June 30, 2026 were $21.3 billion."]);
        let cur = paras(&[
            "Capitalized video costs as of December 31, 2025 and September 30, 2026 were $21.3 billion and $22.5 billion.",
        ]);
        assert_eq!(diff_paragraphs(&cur, &base, true).figures_only, 1);
    }

    #[test]
    fn added_sentence_inside_identical_paragraph() {
        let base = paras(&[
            "The Company depends on component suppliers. Some components are available from a single source. Prices may change.",
        ]);
        let cur = paras(&[
            "The Company depends on component suppliers. Some components are available from a single source. \
             Industry-wide shortages of memory have increased costs and constrained supply. Prices may change.",
        ]);
        let d = diff_paragraphs(&cur, &base, true);
        assert_eq!(d.changed.len(), 1);
        assert_eq!(
            d.changed[0].added_sentences,
            vec!["Industry-wide shortages of memory have increased costs and constrained supply."]
        );
        assert!(d.changed[0].removed_sentences.is_empty());
        assert!(d.added.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn moved_paragraph_is_not_add_and_remove() {
        let base = paras(&[
            "Risks Related to Operations",
            "The Company faces intense competition in all its markets.",
            "The Company's operations are subject to natural disasters.",
        ]);
        let cur = paras(&[
            "Risks Related to Operations",
            "The Company’s operations are subject to natural  disasters.",
            "The Company faces intense competition in all its markets.",
        ]);
        let d = diff_paragraphs(&cur, &base, true);
        assert_eq!(d.unchanged, 3);
        assert!(d.added.is_empty() && d.removed.is_empty() && d.changed.is_empty());
        // A paragraph split in two is rearranged, not new.
        let base = paras(&["Supply may be constrained. Costs may rise as a result of tariffs."]);
        let cur = paras(&["Supply may be constrained.", "Costs may rise as a result of tariffs."]);
        let d = diff_paragraphs(&cur, &base, true);
        assert_eq!(d.moved, 2);
        assert!(d.added.is_empty() && d.removed.is_empty() && d.changed.is_empty());
    }

    #[test]
    fn sentence_cut_by_page_break_is_joined() {
        let html = "<html><body><p>Risks Related to Security</p>\
            <p>As previously disclosed in our Form 8-K filed with the Securities and Exchange</p>\
            <p>12</p><hr style=\"page-break-after:always\"/><p>Item 1A</p>\
            <p>Commission, a threat actor accessed certain systems.</p>\
            <p>Next paragraph about something else entirely.</p></body></html>";
        let doc = super::super::html::parse_filing_html(html);
        let p = section_paragraphs(&doc, 0, doc.blocks.len());
        let texts: Vec<&str> = p.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Risks Related to Security",
                "As previously disclosed in our Form 8-K filed with the Securities and Exchange Commission, a threat actor accessed certain systems.",
                "Next paragraph about something else entirely.",
            ]
        );
        assert_eq!(p[1].under.as_deref(), Some("Risks Related to Security"));
        assert_eq!(p[2].under.as_deref(), Some("Risks Related to Security"));
    }

    #[test]
    fn new_and_dropped_paragraphs_listed_in_order() {
        let base = paras(&["Alpha risk is described here.", "Beta risk is described here in full."]);
        let cur = paras(&[
            "Alpha risk is described here.",
            "Tariffs imposed on imported goods could reduce gross margin materially.",
        ]);
        let d = diff_paragraphs(&cur, &base, true);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.removed[0].text, "Beta risk is described here in full.");
        // Delta mode: base paragraphs not restated are counted, not listed.
        let d = diff_paragraphs(&cur, &base, false);
        assert!(d.removed.is_empty());
        assert_eq!(d.not_restated, Some(1));
    }
}

