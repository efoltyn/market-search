//! Filing structure: the table of contents and each section's block range.
//!
//! Works on the blocks produced by `html.rs`. Three ways to find the top-level
//! sections, tried in order:
//!
//! 1. "Item N." headings (the standard 10-K / 10-Q / 8-K / 20-F layout),
//!    skipping the filing's own table-of-contents rows (rows that end in a
//!    page number) and keeping, per item, the heading with the most text
//!    behind it. Items the body never heads (JPMorgan's MD&A) are placed from
//!    the TOC: its page numbers are mapped onto the body through the running
//!    page footers.
//! 2. Filers that drop "Item N." headings (Intel, GE: a cross-reference index
//!    instead) list their real headings in the TOC table; those entries are
//!    located in the body (page + heading text) and mapped onto items.
//! 3. Canonical item titles matched directly against short heading lines.
//!
//! Inside the financial statements item, each primary statement, the notes
//! container, every numbered note, and the auditor's report get their own
//! sections. An item that only cross-references pages ("appears on pages
//! 46-160") gets a `<id>_referenced` section with those pages.

use super::html::{BlockKind, ParsedDoc};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormFamily {
    TenK,
    TenQ,
    EightK,
    TwentyF,
    Other,
}

pub(crate) fn form_family(form: &str) -> FormFamily {
    let f = form.trim().to_ascii_uppercase();
    let base = f.split('/').next().unwrap_or(&f);
    match base {
        "10-K" | "10-K405" | "10-KT" | "10-KSB" => FormFamily::TenK,
        "10-Q" | "10-QT" | "10-QSB" => FormFamily::TenQ,
        "8-K" | "6-K" | "8-K12B" | "8-K12G3" => FormFamily::EightK,
        "20-F" | "40-F" => FormFamily::TwentyF,
        _ => FormFamily::Other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SectionKind {
    Cover,
    Item,
    Statement,
    Notes,
    Note,
    Report,
    Subsection,
    /// The pages an item only cross-references ("appears on pages 46-160").
    Reference,
    Signatures,
    Document,
}

impl SectionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SectionKind::Cover => "cover",
            SectionKind::Item => "item",
            SectionKind::Statement => "statement",
            SectionKind::Notes => "notes",
            SectionKind::Note => "note",
            SectionKind::Report => "report",
            SectionKind::Subsection => "subsection",
            SectionKind::Reference => "reference",
            SectionKind::Signatures => "signatures",
            SectionKind::Document => "document",
        }
    }

    pub(crate) fn level(self) -> u8 {
        match self {
            SectionKind::Cover
            | SectionKind::Item
            | SectionKind::Signatures
            | SectionKind::Document => 1,
            SectionKind::Statement
            | SectionKind::Notes
            | SectionKind::Report
            | SectionKind::Subsection
            | SectionKind::Reference => 2,
            SectionKind::Note => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Section {
    pub id: String,
    pub title: String,
    pub kind: SectionKind,
    pub parent: Option<String>,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug)]
pub(crate) struct Structure {
    /// Document order; a parent precedes its children.
    pub sections: Vec<Section>,
    pub family: FormFamily,
    /// How the top-level sections were found.
    pub method: &'static str,
    /// Clean-text size of each section, by id.
    pub chars: HashMap<String, usize>,
}

struct ItemDef {
    id: &'static str,
    num: &'static str,
    title: &'static str,
}

const TEN_K_ITEMS: &[ItemDef] = &[
    ItemDef { id: "item1", num: "1", title: "Business" },
    ItemDef { id: "item1a", num: "1a", title: "Risk Factors" },
    ItemDef { id: "item1b", num: "1b", title: "Unresolved Staff Comments" },
    ItemDef { id: "item1c", num: "1c", title: "Cybersecurity" },
    ItemDef { id: "item2", num: "2", title: "Properties" },
    ItemDef { id: "item3", num: "3", title: "Legal Proceedings" },
    ItemDef { id: "item4", num: "4", title: "Mine Safety Disclosures" },
    ItemDef {
        id: "item5",
        num: "5",
        title: "Market for Registrant's Common Equity, Related Stockholder Matters and Issuer Purchases of Equity Securities",
    },
    ItemDef { id: "item6", num: "6", title: "[Reserved]" },
    ItemDef {
        id: "item7",
        num: "7",
        title: "Management's Discussion and Analysis of Financial Condition and Results of Operations",
    },
    ItemDef { id: "item7a", num: "7a", title: "Quantitative and Qualitative Disclosures About Market Risk" },
    ItemDef { id: "item8", num: "8", title: "Financial Statements and Supplementary Data" },
    ItemDef {
        id: "item9",
        num: "9",
        title: "Changes in and Disagreements with Accountants on Accounting and Financial Disclosure",
    },
    ItemDef { id: "item9a", num: "9a", title: "Controls and Procedures" },
    ItemDef { id: "item9b", num: "9b", title: "Other Information" },
    ItemDef {
        id: "item9c",
        num: "9c",
        title: "Disclosure Regarding Foreign Jurisdictions that Prevent Inspections",
    },
    ItemDef { id: "item10", num: "10", title: "Directors, Executive Officers and Corporate Governance" },
    ItemDef { id: "item11", num: "11", title: "Executive Compensation" },
    ItemDef {
        id: "item12",
        num: "12",
        title: "Security Ownership of Certain Beneficial Owners and Management and Related Stockholder Matters",
    },
    ItemDef {
        id: "item13",
        num: "13",
        title: "Certain Relationships and Related Transactions, and Director Independence",
    },
    ItemDef { id: "item14", num: "14", title: "Principal Accountant Fees and Services" },
    ItemDef { id: "item15", num: "15", title: "Exhibits and Financial Statement Schedules" },
    ItemDef { id: "item16", num: "16", title: "Form 10-K Summary" },
];

const TEN_Q_ITEMS: &[ItemDef] = &[
    ItemDef { id: "part1_item1", num: "1", title: "Financial Statements" },
    ItemDef {
        id: "part1_item2",
        num: "2",
        title: "Management's Discussion and Analysis of Financial Condition and Results of Operations",
    },
    ItemDef { id: "part1_item3", num: "3", title: "Quantitative and Qualitative Disclosures About Market Risk" },
    ItemDef { id: "part1_item4", num: "4", title: "Controls and Procedures" },
    ItemDef { id: "part2_item1", num: "1", title: "Legal Proceedings" },
    ItemDef { id: "part2_item1a", num: "1a", title: "Risk Factors" },
    ItemDef { id: "part2_item2", num: "2", title: "Unregistered Sales of Equity Securities and Use of Proceeds" },
    ItemDef { id: "part2_item3", num: "3", title: "Defaults Upon Senior Securities" },
    ItemDef { id: "part2_item4", num: "4", title: "Mine Safety Disclosures" },
    ItemDef { id: "part2_item5", num: "5", title: "Other Information" },
    ItemDef { id: "part2_item6", num: "6", title: "Exhibits" },
];

static ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*items?\s*(\d{1,2})\s*([a-d])?\b(?:\s*(?:and|&|,)\s*\d{1,2}[a-d]?\b)?\s*[.:\-–—]*\s*(.*)$",
    )
    .expect("item regex")
});
static ITEM_8K_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*item\s*(\d{1,2}\.\d{2})\b\s*[.:\-–—]*\s*(.*)$").expect("8-K item regex")
});
static PART_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*part\s+(iv|i{1,3}|[1-4])\b\s*[.:\-–—,]?\s*(.*)$").expect("part regex")
});
static NOTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*note\s*(\d{1,2})\s*[.:\-–—)]*\s*(.*)$").expect("note regex")
});
static PAGES_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bpages?\s+(\d{1,3})\s*(?:-|–|—|to|through)\s*(\d{1,3})\b").expect("pages regex")
});
static NUMBERED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(\d{1,2})\s*[.:\-–—)]\s*(\S.{1,150})$").expect("numbered heading regex")
});

/// Lowercase words only: "Management’s Discussion & Analysis (MD&A)" ->
/// "management s discussion analysis md a".
pub(crate) fn title_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                out.push(l);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim_end().to_string()
}

fn slug(s: &str) -> String {
    let k = title_key(s);
    let mut out: String = k
        .split(' ')
        .filter(|w| !w.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("_");
    if out.is_empty() {
        out = "section".to_string();
    }
    out
}

/// Page reference in a TOC row's last cell: "12", "12-14", "Pages 3 - 30",
/// "Page 45", "F-3", "iii", "Not applicable".
fn is_page_ref(t: &str) -> bool {
    let s = t.trim().to_ascii_lowercase();
    if s.is_empty() {
        return false;
    }
    if matches!(s.as_str(), "not applicable" | "n/a" | "none" | "page") {
        return true;
    }
    let s = s
        .strip_prefix("pages")
        .or_else(|| s.strip_prefix("page"))
        .unwrap_or(&s)
        .trim();
    // "2,345" is a figure, not the page list "2, 345".
    let thousands = s.contains(',')
        && s.split(',').skip(1).all(|g| g.len() == 3 && g.chars().all(|c| c.is_ascii_digit()));
    if thousands {
        return false;
    }
    let parts: Vec<&str> = s
        .split(|c: char| matches!(c, '-' | '–' | '—' | ',') || c.is_whitespace())
        .filter(|p| !p.is_empty() && *p != "and" && *p != "to")
        .collect();
    !parts.is_empty()
        && parts.len() <= 6
        && parts.iter().all(|p| {
            (p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()))
                || (p.len() <= 6 && p.chars().all(|c| matches!(c, 'i' | 'v' | 'x' | 'l')))
                || (p.len() <= 4 && p.len() >= 2
                    && p.starts_with(|c: char| c.is_ascii_alphabetic())
                    && p[1..].chars().all(|c| c.is_ascii_digit()))
                || *p == "f"
                || *p == "a"
        })
}

/// Blocks that belong to the filing's own table of contents / cross-reference
/// index (never a section start).
fn toc_blocks(doc: &ParsedDoc) -> Vec<bool> {
    let blocks = &doc.blocks;
    let mut toc = vec![false; blocks.len()];
    let mut per_table: HashMap<u32, (usize, usize)> = HashMap::new();
    for (i, b) in blocks.iter().enumerate() {
        match b.kind {
            BlockKind::Row => {
                // Title cell(s) + a page reference; a data row ("Net income |
                // 21,155 | ... | 29") has figures in its other cells.
                let body = &b.cells[..b.cells.len().saturating_sub(1)];
                let row_toc = b.cells.len() >= 2
                    && b.cells.len() <= 5
                    && b.cells.last().is_some_and(|c| is_page_ref(c))
                    && body.iter().any(|c| c.chars().any(|ch| ch.is_alphabetic()))
                    && !body.iter().any(|c| super::html::is_numeric_cell(c));
                if row_toc {
                    toc[i] = true;
                }
                let e = per_table.entry(b.table).or_insert((0, 0));
                e.1 += 1;
                if row_toc {
                    e.0 += 1;
                }
            }
            BlockKind::Para => {
                // "Item 7. Management's Discussion ....... 25"
                let t = b.text.trim();
                let lower = t.to_ascii_lowercase();
                // Needs a title between the label and the page: "NOTE 1" alone is
                // a heading whose title sits on the next line.
                let words = t.split_whitespace().count();
                if (lower.starts_with("item") || lower.starts_with("part") || lower.starts_with("note"))
                    && t.len() <= 200
                    && words >= 4
                {
                    if let Some(last) = t.split_whitespace().last() {
                        let leader = t.contains("....") || t.contains(". . .");
                        if last.len() <= 3
                            && last.chars().all(|c| c.is_ascii_digit())
                            && (leader || words <= 20)
                            && !t.ends_with('.')
                        {
                            toc[i] = true;
                        }
                    }
                }
            }
            BlockKind::PageBreak => {}
        }
    }
    // A table where three or more rows end in page references is a TOC (or a
    // cross-reference index): every row of it is TOC, page number or not.
    for (i, b) in blocks.iter().enumerate() {
        if b.kind == BlockKind::Row {
            if let Some((tocish, total)) = per_table.get(&b.table) {
                if *tocish >= 3 && tocish * 10 >= total * 4 {
                    toc[i] = true;
                }
            }
        }
    }
    toc
}

fn is_heading_like(doc: &ParsedDoc, toc: &[bool], i: usize, max_chars: usize) -> bool {
    let b = &doc.blocks[i];
    if b.furniture || toc[i] || b.kind == BlockKind::PageBreak {
        return false;
    }
    if b.kind == BlockKind::Row && !b.layout && b.cells.len() > 3 {
        return false;
    }
    let t = b.heading_text();
    let n = t.chars().count();
    n > 0 && n <= max_chars
}

fn next_content_block(doc: &ParsedDoc, from: usize) -> Option<usize> {
    (from..doc.blocks.len())
        .find(|&j| !doc.blocks[j].furniture && doc.blocks[j].kind != BlockKind::PageBreak)
}

fn prev_content_block(doc: &ParsedDoc, before: usize) -> Option<usize> {
    (0..before)
        .rev()
        .find(|&j| !doc.blocks[j].furniture && doc.blocks[j].kind != BlockKind::PageBreak)
}

/// The heading's own title, or (when the heading is just "Item 7." / "Note 5")
/// the short line right after it.
fn heading_title(doc: &ParsedDoc, i: usize, rest: &str) -> String {
    let rest = rest.trim().trim_start_matches(['.', ':', '-', '–', '—']).trim();
    if !rest.is_empty() {
        return rest.to_string();
    }
    if let Some(j) = next_content_block(doc, i + 1) {
        let t = doc.blocks[j].heading_text();
        let t = t.trim();
        let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
        let caps = letters.len() >= 3 && letters.iter().all(|c| c.is_uppercase());
        if !t.is_empty()
            && t.chars().count() <= 150
            && !ITEM_RE.is_match(t)
            && !NOTE_RE.is_match(t)
            && (!t.ends_with('.') || caps)
        {
            return t.trim_end_matches('.').trim().to_string();
        }
        // "NOTE 1." followed by "BASIS OF PRESENTATION. Our financial ...".
        if let Some(h) = run_in_heading(t) {
            return h.to_string();
        }
    }
    String::new()
}

/// Cover-page check boxes: "Item 17 ☐ Item 18 ☒" (20-F), "Item 17 o Item 18 x".
fn is_checkbox_rest(rest: &str) -> bool {
    let first = rest.split_whitespace().next().unwrap_or("");
    matches!(first, "☐" | "☑" | "☒" | "□" | "■" | "✓" | "✔" | "o" | "x" | "X" | "þ" | "ý" | "¨")
}

fn looks_like_cross_reference(rest: &str) -> bool {
    let first = rest
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "of" | "in" | "to" | "and" | "or" | "above" | "below" | "herein" | "for" | "this" | "that"
            | "is" | "are" | "was" | "were" | "has" | "have" | "the" | "under" | "within"
    )
}

/// Map a section title onto a canonical item (used when headings omit "Item N.").
fn map_title_to_item(family: FormFamily, title: &str) -> Option<&'static str> {
    let k = title_key(title);
    let k = k.as_str();
    let starts = |p: &str| k.starts_with(p);
    let mda = starts("management s discussion and analysis")
        || starts("managements discussion and analysis")
        || starts("management discussion and analysis")
        || k == "md a";
    match family {
        FormFamily::TenQ => {
            if mda {
                Some("part1_item2")
            } else if (k.contains("financial statements") && !k.contains("notes to") && !k.contains("statements of"))
                || starts("financial information")
            {
                Some("part1_item1")
            } else if starts("quantitative and qualitative disclosure") {
                Some("part1_item3")
            } else if starts("controls and procedures") {
                Some("part1_item4")
            } else if starts("legal proceedings") {
                Some("part2_item1")
            } else if k == "risk factors" || (starts("risk factors") && k.len() <= 24) {
                Some("part2_item1a")
            } else if starts("unregistered sales of equity securities")
                || starts("issuer purchases of equity securities")
            {
                Some("part2_item2")
            } else if starts("defaults upon senior securities") {
                Some("part2_item3")
            } else if starts("mine safety disclosure") {
                Some("part2_item4")
            } else if starts("other information") || starts("rule 10b5 1 trading arrangements") {
                Some("part2_item5")
            } else if starts("exhibits") {
                Some("part2_item6")
            } else {
                None
            }
        }
        FormFamily::TenK => {
            if mda {
                Some("item7")
            } else if k == "business" || starts("business overview") || k == "our business" || k == "our businesses" {
                Some("item1")
            } else if k == "risk factors" || (starts("risk factors") && k.len() <= 24) {
                Some("item1a")
            } else if starts("unresolved staff comments") {
                Some("item1b")
            } else if starts("cybersecurity") {
                Some("item1c")
            } else if k == "properties" {
                Some("item2")
            } else if starts("legal proceedings") {
                Some("item3")
            } else if starts("mine safety disclosure") {
                Some("item4")
            } else if starts("market for registrant s common equity") || starts("market for the registrant s common equity") {
                Some("item5")
            } else if starts("quantitative and qualitative disclosure") {
                Some("item7a")
            } else if starts("financial statements and supplementary data")
                || (k.contains("financial statements") && !k.contains("notes to") && !k.contains("statements of")
                    && !k.contains("schedule") && !k.contains("exhibit"))
            {
                Some("item8")
            } else if starts("changes in and disagreements with accountants") {
                Some("item9")
            } else if starts("controls and procedures") {
                Some("item9a")
            } else if starts("other information") {
                Some("item9b")
            } else if starts("disclosure regarding foreign jurisdictions") {
                Some("item9c")
            } else if starts("directors executive officers and corporate governance") {
                Some("item10")
            } else if starts("executive compensation") {
                Some("item11")
            } else if starts("security ownership of certain beneficial owners") {
                Some("item12")
            } else if starts("certain relationships and related transactions") {
                Some("item13")
            } else if starts("principal accountant fees and services") || starts("principal accounting fees") {
                Some("item14")
            } else if starts("exhibits and financial statement schedules") || starts("exhibit and financial statement schedules") {
                Some("item15")
            } else if starts("form 10 k summary") {
                Some("item16")
            } else {
                None
            }
        }
        _ => None,
    }
}

fn canonical_items(family: FormFamily) -> &'static [ItemDef] {
    match family {
        FormFamily::TenK => TEN_K_ITEMS,
        FormFamily::TenQ => TEN_Q_ITEMS,
        _ => &[],
    }
}

fn canonical_title(family: FormFamily, id: &str) -> Option<&'static str> {
    canonical_items(family).iter().find(|d| d.id == id).map(|d| d.title)
}

fn canonical_index(family: FormFamily, id: &str) -> Option<usize> {
    let items = canonical_items(family);
    if items.is_empty() {
        // 20-F / other: order by item number then letter.
        let digits: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
        let letter = id.chars().last().filter(|c| c.is_ascii_alphabetic()).map(|c| c as usize).unwrap_or(0);
        return digits.parse::<usize>().ok().map(|n| n * 100 + letter);
    }
    items.iter().position(|d| d.id == id)
}

fn item_display_title(family: FormFamily, id: &str, num: &str, title: &str) -> String {
    let num_disp = num.to_ascii_uppercase();
    let t = if title.trim().is_empty() {
        canonical_title(family, id).unwrap_or("").to_string()
    } else {
        title.trim().trim_end_matches('.').to_string()
    };
    match family {
        FormFamily::TenQ => {
            let part = if id.starts_with("part2") { "II" } else { "I" };
            format!("Part {part}, Item {num_disp}. {t}")
        }
        _ => format!("Item {num_disp}. {t}"),
    }
}

struct Candidate {
    block: usize,
    id: String,
    num: String,
    title: String,
}

fn find_item_headings(doc: &ParsedDoc, toc: &[bool], family: FormFamily) -> Vec<Candidate> {
    let mut cands: Vec<Candidate> = Vec::new();
    let mut current_part: u8 = 1;
    let mut seen_part2 = false;
    for i in 0..doc.blocks.len() {
        if !is_heading_like(doc, toc, i, 260) {
            continue;
        }
        let text = doc.blocks[i].heading_text().to_string();
        if let Some(c) = PART_RE.captures(&text) {
            if text.chars().count() <= 120 {
                let p = c.get(1).map(|m| m.as_str().to_ascii_lowercase()).unwrap_or_default();
                current_part = match p.as_str() {
                    "ii" | "2" => 2,
                    "iii" | "3" => 3,
                    "iv" | "4" => 4,
                    _ => 1,
                };
                if current_part == 2 {
                    seen_part2 = true;
                }
                // "PART I — Item 1. Financial Statements" on one line: fall through.
                let rest = c.get(2).map(|m| m.as_str()).unwrap_or("");
                if !ITEM_RE.is_match(rest) {
                    continue;
                }
            }
        }
        let item_text = match PART_RE.captures(&text) {
            Some(c) if text.chars().count() <= 200 => c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default(),
            _ => text.clone(),
        };
        let Some(c) = ITEM_RE.captures(&item_text) else {
            continue;
        };
        let num = format!(
            "{}{}",
            c.get(1).map(|m| m.as_str()).unwrap_or(""),
            c.get(2).map(|m| m.as_str().to_ascii_lowercase()).unwrap_or_default()
        );
        let rest = c.get(3).map(|m| m.as_str()).unwrap_or("");
        if looks_like_cross_reference(rest) || is_checkbox_rest(rest) {
            continue;
        }
        let words = item_text.split_whitespace().count();
        if item_text.trim_end().ends_with('.') && words > 25 {
            continue;
        }
        let title = heading_title(doc, i, rest);
        let id = match family {
            FormFamily::TenQ => {
                let mapped = map_title_to_item(family, &title);
                let part = if let Some(m) = mapped.filter(|m| m.ends_with(&format!("_item{num}"))) {
                    if m.starts_with("part2") { 2 } else { 1 }
                } else if matches!(num.as_str(), "1a" | "5" | "6") || (seen_part2 && current_part >= 2) {
                    2
                } else {
                    1
                };
                format!("part{part}_item{num}")
            }
            _ => format!("item{num}"),
        };
        if family == FormFamily::TenK && !TEN_K_ITEMS.iter().any(|d| d.id == id) {
            continue;
        }
        if family == FormFamily::TenQ && !TEN_Q_ITEMS.iter().any(|d| d.id == id) {
            continue;
        }
        cands.push(Candidate { block: i, id, num, title });
    }
    cands
}

/// Text size between consecutive candidates (cheap proxy: rendered chars).
fn spans(doc: &ParsedDoc, starts: &[usize]) -> Vec<usize> {
    let mut prefix = Vec::with_capacity(doc.blocks.len() + 1);
    let mut acc = 0usize;
    prefix.push(0);
    for b in &doc.blocks {
        if !b.furniture && b.kind != BlockKind::PageBreak {
            acc += b.text.len() + 1;
        }
        prefix.push(acc);
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let e = starts.get(k + 1).copied().unwrap_or(doc.blocks.len());
        out.push(prefix[e.max(s + 1).min(doc.blocks.len())] - prefix[s + 1]);
    }
    out
}

fn select_items(doc: &ParsedDoc, cands: Vec<Candidate>) -> Vec<Candidate> {
    if cands.is_empty() {
        return cands;
    }
    let starts: Vec<usize> = cands.iter().map(|c| c.block).collect();
    let sp = spans(doc, &starts);
    // Per id: the candidate with the most text behind it (a TOC line that
    // slipped through has almost none). Order is left to the document: banks
    // (BAC, JPM, Citi) print MD&A before the financial statements.
    let mut best: HashMap<String, usize> = HashMap::new();
    for (k, c) in cands.iter().enumerate() {
        match best.get(&c.id) {
            Some(&b) if sp[b] >= sp[k] => {}
            _ => {
                best.insert(c.id.clone(), k);
            }
        }
    }
    let mut chosen: Vec<usize> = best.values().copied().collect();
    chosen.sort_by_key(|&k| cands[k].block);
    // A tiny candidate in a dense run of "Item" lines before the first
    // substantial section is a leftover table-of-contents line, not a heading
    // (20-F Items 1-2 are tiny but stand alone).
    let first_strong = chosen
        .iter()
        .filter(|&&k| sp[k] >= 2000)
        .map(|&k| cands[k].block)
        .min()
        .unwrap_or(0);
    let all_blocks: Vec<usize> = cands.iter().map(|c| c.block).collect();
    chosen.retain(|&k| {
        let b = cands[k].block;
        let dense = all_blocks.iter().filter(|&&o| o > b && o <= b + 12).count() >= 3;
        !(b < first_strong && sp[k] < 300 && dense)
    });
    let mut taken = vec![false; cands.len()];
    for k in chosen {
        taken[k] = true;
    }
    cands
        .into_iter()
        .enumerate()
        .filter(|(k, _)| taken[*k])
        .map(|(_, c)| c)
        .collect()
}

fn is_statement_title_key(k: &str) -> bool {
    statement_kind(k).is_some()
}

/// Primary statement type from a heading's title key.
fn statement_kind(k: &str) -> Option<&'static str> {
    if k.contains("notes to") || k.contains("schedule") || k.contains("index to") || k.contains(" see ") {
        return None;
    }
    // MD&A headings that borrow statement words ("Consolidated balance sheets
    // and cash flows analysis", "Selected income statement data", "Refer to
    // Consolidated Statements of ... on page 168").
    if [
        "analysis", "discussion", "highlights", "average", "review", "overview", "supplemental",
        "selected", "refer", " data", " page", "summary", "key metrics", "obligor", "guarantor",
        "parent company", "summarized", "issuer", "condensed financial information",
    ]
    .iter()
    .any(|w| k.contains(w))
    {
        return None;
    }
    let has = |s: &str| k.contains(s);
    // The phrase has to lead the heading (company name prefix allowed).
    let lead = ["statement", "balance sheet"]
        .iter()
        .filter_map(|p| k.find(p))
        .min()
        .unwrap_or(usize::MAX);
    // Long legal names lead some titles ("Taiwan Semiconductor Manufacturing
    // Company Limited and Subsidiaries CONSOLIDATED STATEMENTS OF ...").
    if lead > 100 {
        return None;
    }
    // ... and not be the object of a sentence fragment ("Add: Off-balance
    // sheet exposures", "Total balance sheet", "per the statement of").
    let before = k[..lead].split_whitespace().last().unwrap_or("");
    if matches!(
        before,
        "off" | "add" | "less" | "total" | "see" | "per" | "in" | "from" | "on" | "of" | "and" | "to" | "the" | "our" | "a"
    ) {
        return None;
    }
    if has("cash flow") {
        return Some("cash_flow");
    }
    if has("balance sheet") || has("financial position") || has("financial condition") || has("statements of condition") || has("statement of condition") {
        return Some("balance_sheet");
    }
    let combined_ops = has("operations and comprehensive")
        || has("income and comprehensive")
        || has("earnings and comprehensive")
        || has("loss and comprehensive")
        || has("loss and other comprehensive")
        || has("profit or loss")
        || has("profit and loss");
    if has("comprehensive") && !combined_ops {
        return Some("comprehensive_income");
    }
    if has("stockholders") || has("shareholders") || has("shareowners") || has("equity")
        || has("partners capital") || has("members capital") || has("net assets")
        || has("stockholders deficit") || has("shareholders deficit")
    {
        return Some("equity");
    }
    if has("operations") || has("income") || has("earnings") || has("statements of loss") || has("statement of loss")
        || has("profit or loss") || has("profit and loss")
    {
        return Some("income_statement");
    }
    None
}

fn followed_by_table(doc: &ParsedDoc, i: usize) -> bool {
    let head = &doc.blocks[i];
    let mut seen = 0;
    let mut same_table_rows = 0;
    let mut j = i + 1;
    while j < doc.blocks.len() && seen < 8 {
        let b = &doc.blocks[j];
        j += 1;
        if b.furniture || b.kind == BlockKind::PageBreak {
            continue;
        }
        if b.kind == BlockKind::Row && !b.layout {
            // The title can be the first row of the statement's own table
            // (Bank of America, GE): its data rows follow in the same table.
            if head.kind == BlockKind::Row && b.table == head.table {
                same_table_rows += 1;
                if same_table_rows >= 2 {
                    return true;
                }
                continue;
            }
            return true;
        }
        seen += 1;
    }
    false
}

/// A company-name line ("AMAZON.COM, INC.") right above a statement title
/// belongs to that statement, not to the end of the previous one.
fn pull_up_caption(doc: &ParsedDoc, start: usize, floor: usize) -> usize {
    let Some(p) = prev_content_block(doc, start) else {
        return start;
    };
    if p < floor {
        return start;
    }
    let b = &doc.blocks[p];
    let t = b.heading_text();
    let t = t.trim();
    let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
    let all_caps = !letters.is_empty() && letters.iter().all(|c| c.is_uppercase());
    let registrant = doc
        .dei
        .get("dei:EntityRegistrantName")
        .map(|(name, _)| title_key(name))
        .filter(|k| !k.is_empty());
    let is_registrant = registrant.is_some_and(|r| {
        let k = title_key(t);
        k == r || (k.starts_with(&r) && k.len() <= r.len() + 12)
    });
    let short_para = b.kind == BlockKind::Para && t.chars().count() <= 60;
    if short_para && ((all_caps && !t.chars().any(|c| c.is_ascii_digit())) || is_registrant) {
        p
    } else {
        start
    }
}

fn detect_financial_children(
    doc: &ParsedDoc,
    toc: &[bool],
    parent: &Section,
    taken_starts: &[usize],
) -> Vec<Section> {
    let (lo, hi) = (parent.start, parent.end);
    let mut out: Vec<Section> = Vec::new();
    // Statements: the first heading of each kind that is followed by a table
    // and has a statement's worth of text behind it (an index of the
    // statements, "Consolidated Balance Sheets as of ...", has a line).
    let mut cands: Vec<(usize, &'static str)> = Vec::new();
    for i in lo..hi {
        if !is_heading_like(doc, toc, i, 300) {
            continue;
        }
        let key = title_key(&doc.blocks[i].heading_text());
        if let Some(kind) = statement_kind(&key) {
            if followed_by_table(doc, i) {
                cands.push((i, kind));
            }
        }
    }
    // Text behind each candidate up to the next one (or the container's end).
    let cand_spans: Vec<usize> = cands
        .iter()
        .enumerate()
        .map(|(k, c)| {
            let end = cands.get(k + 1).map(|n| n.0).unwrap_or(hi).min(hi);
            doc.blocks[c.0 + 1..end.max(c.0 + 1)]
                .iter()
                .filter(|b| !b.furniture && b.kind != BlockKind::PageBreak)
                .map(|b| b.text.len() + 1)
                .sum()
        })
        .collect();
    let mut seen_kinds: Vec<&'static str> = Vec::new();
    for (k, &(i, kind)) in cands.iter().enumerate() {
        if seen_kinds.contains(&kind) || cand_spans[k] < 120 {
            continue;
        }
        seen_kinds.push(kind);
        let start = pull_up_caption(doc, i, lo);
        // A title that is the first row of the statement's table: just the
        // cell with the title, not the column headers next to it.
        let b = &doc.blocks[i];
        let title = if b.kind == BlockKind::Row && b.cells.len() > 1 {
            b.cells
                .iter()
                .find(|c| statement_kind(&title_key(c)).is_some())
                .cloned()
                .unwrap_or_else(|| b.cells[0].clone())
        } else {
            b.heading_text().trim().to_string()
        };
        out.push(Section {
            id: kind.to_string(),
            title,
            kind: SectionKind::Statement,
            parent: Some(parent.id.clone()),
            start,
            end: hi,
        });
    }
    let last_statement = out.iter().map(|s| s.start).max().unwrap_or(lo);

    // Notes container heading.
    let mut notes_header: Option<usize> = None;
    for i in last_statement.max(lo)..hi {
        if !is_heading_like(doc, toc, i, 160) {
            continue;
        }
        let text = doc.blocks[i].heading_text().to_string();
        let key = title_key(&text);
        if let Some(p) = key.find("notes to") {
            let prefix = &key[..p];
            let sentence = text.trim_end().ends_with('.')
                || prefix.contains("see")
                || prefix.contains("accompanying")
                || prefix.contains("refer");
            if p <= 40 && !sentence && key.contains("financial statements") && !key.contains("continued") {
                notes_header = Some(i);
                break;
            }
        }
    }

    let notes_title_block = notes_header;
    let notes_header = notes_header.map(|h| pull_up_caption(doc, h, last_statement.max(lo)));
    // Numbered notes: "Note 5 — Debt", "NOTE 5. DEBT", "Note 5 : | Goodwill".
    let notes_from = notes_header.unwrap_or(last_statement);
    let mut notes: Vec<(usize, u32, String)> = Vec::new();
    let mut expected = 1u32;
    for i in notes_from..hi {
        if !is_heading_like(doc, toc, i, 200) {
            continue;
        }
        let text = doc.blocks[i].heading_text().to_string();
        let Some(c) = NOTE_RE.captures(&text) else {
            continue;
        };
        let Ok(num) = c.get(1).map(|m| m.as_str()).unwrap_or("").parse::<u32>() else {
            continue;
        };
        let rest = c.get(2).map(|m| m.as_str()).unwrap_or("");
        if looks_like_cross_reference(rest) || rest.to_ascii_lowercase().contains("continued") {
            continue;
        }
        if num != expected && !(num > expected && num <= expected + 3 && notes.len() >= 2) {
            continue;
        }
        let title = heading_title(doc, i, rest);
        if title.ends_with('.') && title.split_whitespace().count() > 14 {
            continue;
        }
        notes.push((i, num, title));
        expected = num + 1;
    }
    {
        // Filers that number notes without the word "Note" ("1. GENERAL",
        // IFRS style). Used when it finds clearly more notes than the "Note N"
        // scan, which can latch onto table footnotes ("Note 1: Including ...").
        let mut numbered: Vec<(usize, u32, String)> = Vec::new();
        let mut expected = 1u32;
        for i in notes_from..hi {
            if !is_heading_like(doc, toc, i, 170) {
                continue;
            }
            let text = doc.blocks[i].heading_text().to_string();
            let Some(c) = NUMBERED_RE.captures(&text) else {
                continue;
            };
            let Ok(num) = c.get(1).map(|m| m.as_str()).unwrap_or("").parse::<u32>() else {
                continue;
            };
            let title = c.get(2).map(|m| m.as_str().trim()).unwrap_or("").to_string();
            let starts_upper = title.chars().next().is_some_and(|ch| ch.is_uppercase());
            // A few missed headings (split across lines, inside a table) must
            // not end the scan.
            let small_gap = numbered.len() >= 3 && num > expected && num <= expected + 3;
            if !(num == expected || small_gap) || !starts_upper || title.ends_with('.') || title.split_whitespace().count() > 14 {
                continue;
            }
            numbered.push((i, num, title));
            expected = num + 1;
        }
        if numbered.len() > notes.len() + 1 {
            notes = numbered;
        }
    }

    if notes.is_empty() {
        // Last resort: the inline-XBRL note text blocks ("...DisclosureTextBlock")
        // mark where each note starts even when its heading format is unusual.
        let mut spans: Vec<(usize, String)> = doc
            .text_blocks
            .iter()
            .filter(|t| t.start >= notes_from && t.start < hi && t.end > t.start)
            .filter(|t| {
                let c = t.concept.rsplit(':').next().unwrap_or("");
                c.ends_with("TextBlock")
                    && !c.contains("Policy")
                    && !c.contains("Table")
                    && !c.contains("Details")
                    && !c.contains("Schedule")
            })
            .map(|t| (t.start, t.concept.clone()))
            .collect();
        spans.sort();
        spans.dedup_by_key(|s| s.0);
        for (k, (start, concept)) in spans.into_iter().enumerate() {
            let first = next_content_block(doc, start)
                .map(|j| doc.blocks[j].heading_text().trim().to_string())
                .filter(|t| t.chars().count() <= 150 && !t.ends_with('.'));
            let title = first.unwrap_or_else(|| {
                let local = concept.rsplit(':').next().unwrap_or(&concept);
                local
                    .trim_end_matches("TextBlock")
                    .trim_end_matches("Disclosure")
                    .to_string()
            });
            notes.push((start, (k + 1) as u32, title));
        }
    }

    // Auditor's report.
    let mut report: Option<usize> = None;
    for i in lo..hi {
        if !is_heading_like(doc, toc, i, 160) {
            continue;
        }
        let key = title_key(&doc.blocks[i].heading_text());
        if key.starts_with("report of independent registered public accounting firm")
            || key.starts_with("reports of independent registered public accounting firm")
        {
            if !out.iter().any(|s| s.start <= i && i < s.start + 2) {
                report = Some(i);
                break;
            }
        }
    }
    if let Some(r) = report {
        out.push(Section {
            id: "auditor_report".to_string(),
            title: doc.blocks[r].heading_text().trim().to_string(),
            kind: SectionKind::Report,
            parent: Some(parent.id.clone()),
            start: r,
            end: hi,
        });
    }

    // Level-2 ends: next level-2 sibling (statements, notes container,
    // report, or a pre-existing subsection like Intel's "Key Terms").
    let notes_start = notes_header.or_else(|| notes.first().map(|n| n.0));
    let mut l2_starts: Vec<usize> = out.iter().map(|s| s.start).collect();
    if let Some(ns) = notes_start {
        l2_starts.push(ns);
    }
    // What follows the last note in an annual report (supplementary data,
    // glossary, schedules) is not part of it.
    if let Some(last_note) = notes.last().map(|n| n.0) {
        const AFTER_NOTES: &[&str] = &[
            "supplementary information",
            "supplementary data",
            "supplementary financial",
            "selected quarterly",
            "quarterly financial data",
            "quarterly results",
            "glossary",
            "exhibit index",
            "index to exhibits",
            "schedule i ",
            "schedule ii",
            "valuation and qualifying accounts",
            "management s report on internal control",
            "report of management",
            "distribution of assets",
            "signatures",
        ];
        if let Some(t) = (last_note + 1..hi).find(|&i| {
            is_heading_like(doc, toc, i, 220) && {
                let k = format!("{} ", title_key(&doc.blocks[i].heading_text()));
                AFTER_NOTES.iter().any(|w| k.starts_with(w))
            }
        }) {
            l2_starts.push(t);
        }
    }
    l2_starts.extend(taken_starts.iter().copied().filter(|&s| s > lo && s < hi));
    l2_starts.sort_unstable();
    l2_starts.dedup();
    let next_after = |s: usize| l2_starts.iter().copied().find(|&x| x > s).unwrap_or(hi);
    for s in out.iter_mut() {
        s.end = next_after(s.start);
    }
    if let Some(ns) = notes_start {
        // The notes run until the next level-2 sibling after the last note.
        let last_note = notes.last().map(|n| n.0).unwrap_or(ns);
        let notes_end = l2_starts.iter().copied().find(|&x| x > last_note.max(ns)).unwrap_or(hi);
        let title = notes_title_block
            .map(|h| doc.blocks[h].heading_text().trim().to_string())
            .unwrap_or_else(|| "Notes to Financial Statements".to_string());
        out.push(Section {
            id: "notes".to_string(),
            title,
            kind: SectionKind::Notes,
            parent: Some(parent.id.clone()),
            start: ns,
            end: notes_end,
        });
        for (k, (i, num, title)) in notes.iter().enumerate() {
            let end = notes.get(k + 1).map(|n| n.0).unwrap_or(notes_end);
            let clean = title.trim().trim_end_matches(['.', ':']).trim();
            out.push(Section {
                id: format!("note:{num}"),
                title: if clean.is_empty() {
                    format!("Note {num}")
                } else {
                    format!("Note {num}. {clean}")
                },
                kind: SectionKind::Note,
                parent: Some("notes".to_string()),
                start: *i,
                end: end.min(notes_end),
            });
        }
    }
    out.sort_by_key(|s| (s.start, s.kind.level()));
    out
}

/// Detect the filing's sections.
pub(crate) fn detect_structure(doc: &ParsedDoc, form: &str) -> Structure {
    let family = form_family(form);
    let n = doc.blocks.len();
    let toc = toc_blocks(doc);
    let mut method = "none";
    let mut top: Vec<Section> = Vec::new();
    let mut subsections: Vec<Section> = Vec::new();

    if family == FormFamily::EightK {
        let mut seen: Vec<String> = Vec::new();
        for i in 0..n {
            if !is_heading_like(doc, &toc, i, 300) {
                continue;
            }
            let text = doc.blocks[i].heading_text().to_string();
            let Some(c) = ITEM_8K_RE.captures(&text) else {
                continue;
            };
            let num = c.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let rest = c.get(2).map(|m| m.as_str()).unwrap_or("");
            if looks_like_cross_reference(rest) {
                continue;
            }
            let id = format!("item{num}");
            if seen.contains(&id) {
                continue;
            }
            seen.push(id.clone());
            let title = heading_title(doc, i, rest);
            top.push(Section {
                id,
                title: format!("Item {num}. {}", title.trim_end_matches('.')).trim().to_string(),
                kind: SectionKind::Item,
                parent: None,
                start: i,
                end: n,
            });
        }
        if !top.is_empty() {
            method = "item_headings";
        }
    } else {
        let cands = find_item_headings(doc, &toc, family);
        let chosen = select_items(doc, cands);
        if chosen.len() >= 2 || (chosen.len() == 1 && family == FormFamily::Other) {
            method = "item_headings";
            for c in chosen {
                top.push(Section {
                    title: item_display_title(family, &c.id, &c.num, &c.title),
                    id: c.id,
                    kind: SectionKind::Item,
                    parent: None,
                    start: c.block,
                    end: n,
                });
            }
            if matches!(family, FormFamily::TenK | FormFamily::TenQ) {
                let before = top.len();
                fill_missing_items(doc, &toc, family, &mut top);
                if top.len() > before {
                    method = "item_headings+toc";
                }
            }
        } else if matches!(family, FormFamily::TenK | FormFamily::TenQ) {
            let (items, subs) = sections_from_toc_titles(doc, &toc, family);
            if items.len() >= 2 {
                method = "toc_titles";
                top = items;
                subsections = subs;
            } else {
                let items = sections_from_title_match(doc, &toc, family);
                if items.len() >= 2 {
                    method = "title_match";
                    top = items;
                }
            }
        }
    }

    // Signatures close the last item.
    let last_item_start = top.iter().map(|s| s.start).max().unwrap_or(0);
    for i in last_item_start..n {
        if !is_heading_like(doc, &toc, i, 40) {
            continue;
        }
        let key = title_key(&doc.blocks[i].heading_text());
        if key == "signatures" || key == "signature" {
            if i > last_item_start {
                top.push(Section {
                    id: "signatures".to_string(),
                    title: "Signatures".to_string(),
                    kind: SectionKind::Signatures,
                    parent: None,
                    start: i,
                    end: n,
                });
            }
            break;
        }
    }

    top.sort_by_key(|s| s.start);
    for k in 0..top.len() {
        top[k].end = top.get(k + 1).map(|s| s.start).unwrap_or(n);
    }

    // The financial statements item: statements, notes, auditor's report.
    let fs_id = match family {
        FormFamily::TenK => Some("item8"),
        FormFamily::TenQ => Some("part1_item1"),
        FormFamily::TwentyF => ["item18", "item17", "item8"]
            .into_iter()
            .find(|id| top.iter().any(|s| s.id == *id)),
        _ => None,
    };
    let mut children: Vec<Section> = Vec::new();
    let fs_parent = fs_id.and_then(|id| top.iter().find(|s| s.id == id).cloned());
    let container = fs_parent.clone().or_else(|| {
        // No item for the statements (or no items at all): search the whole
        // document after its table of contents, under a synthetic parent.
        if matches!(family, FormFamily::EightK) {
            return None;
        }
        let after_toc = front_toc_end(doc, &toc);
        Some(Section {
            id: "financial_statements".to_string(),
            title: "Financial Statements".to_string(),
            kind: SectionKind::Item,
            parent: None,
            start: after_toc.min(n),
            end: n,
        })
    });
    if let Some(parent) = container.as_ref() {
        let sub_starts: Vec<usize> = subsections
            .iter()
            .filter(|s| s.parent.as_deref() == Some(parent.id.as_str()))
            .map(|s| s.start)
            .collect();
        children = detect_financial_children(doc, &toc, parent, &sub_starts);
        if fs_parent.is_some() && !children.iter().any(|c| c.kind == SectionKind::Statement) {
            // The statements item is only a cross-reference ("see pages 150-
            // 300", JPMorgan's 10-K): read the statements where they really
            // are, under the item that contains them.
            let probe = Section {
                id: "financial_statements".to_string(),
                title: String::new(),
                kind: SectionKind::Item,
                parent: None,
                start: front_toc_end(doc, &toc),
                end: n,
            };
            let found = detect_financial_children(doc, &toc, &probe, &[]);
            // The section holding most of the statements (an MD&A cash-flow
            // table is a lone hit; the real set comes together).
            let mut per_host: Vec<(usize, usize, usize)> = Vec::new(); // (top index, count, first start)
            for c in found.iter().filter(|c| c.kind == SectionKind::Statement) {
                if let Some(t) = top.iter().position(|s| s.start <= c.start && c.start < s.end) {
                    match per_host.iter_mut().find(|h| h.0 == t) {
                        Some(h) => {
                            h.1 += 1;
                            h.2 = h.2.min(c.start);
                        }
                        None => per_host.push((t, 1, c.start)),
                    }
                }
            }
            let best = per_host.into_iter().max_by_key(|h| (h.1, h.2));
            let first = best.map(|h| h.2);
            let host = best.map(|h| top[h.0].clone());
            match host {
                Some(host) if host.kind == SectionKind::Item => {
                    let sub_starts: Vec<usize> = subsections
                        .iter()
                        .filter(|s| s.parent.as_deref() == Some(host.id.as_str()))
                        .map(|s| s.start)
                        .collect();
                    children = detect_financial_children(doc, &toc, &host, &sub_starts);
                }
                Some(host) => {
                    // Statements printed after the signatures (20-F F-pages):
                    // give them their own top-level section.
                    let f = first.unwrap_or(host.start);
                    let start = found
                        .iter()
                        .map(|c| c.start)
                        .filter(|&st| st >= host.start)
                        .min()
                        .unwrap_or(f);
                    let start = (host.start + 1..start)
                        .rev()
                        .take(400)
                        .find(|&i| {
                            is_heading_like(doc, &toc, i, 120) && {
                                let k = title_key(&doc.blocks[i].heading_text());
                                k.starts_with("index to consolidated financial statements")
                                    || k.starts_with("index to financial statements")
                                    || k.starts_with("report of independent registered public accounting firm")
                            }
                        })
                        .unwrap_or(start);
                    let end = top.iter().map(|s| s.start).filter(|&x| x > start).min().unwrap_or(n);
                    let fs = Section {
                        id: "financial_statements".to_string(),
                        title: "Financial Statements (as printed after the filing's main body)".to_string(),
                        kind: SectionKind::Item,
                        parent: None,
                        start,
                        end,
                    };
                    for s in top.iter_mut() {
                        if s.id == host.id {
                            s.end = start;
                        }
                    }
                    children = detect_financial_children(doc, &toc, &fs, &[]);
                    top.push(fs);
                    top.sort_by_key(|s| s.start);
                }
                None => {}
            }
        }
        if fs_parent.is_none() && !children.is_empty() {
            // Synthesize the statements item from the first statement onwards.
            let start = children.iter().map(|c| c.start).min().unwrap_or(parent.start);
            let end = top
                .iter()
                .map(|s| s.start)
                .filter(|&s| s > start)
                .min()
                .unwrap_or(n);
            let id = fs_id.unwrap_or("financial_statements").to_string();
            let title = fs_id
                .and_then(|i| canonical_title(family, i))
                .map(|t| item_display_title(family, fs_id.unwrap_or(""), if family == FormFamily::TenK { "8" } else { "1" }, t))
                .unwrap_or_else(|| "Financial Statements".to_string());
            for c in children.iter_mut() {
                c.end = c.end.min(end);
                if c.parent.as_deref() == Some("financial_statements") || c.parent.as_deref() == Some(parent.id.as_str()) {
                    if c.kind != SectionKind::Note {
                        c.parent = Some(id.clone());
                    }
                }
            }
            // An item that starts inside the synthesized range is dropped (it
            // was a heading inside the statements, e.g. a note subsection).
            top.retain(|s| !(s.start > start && s.start < end && s.kind == SectionKind::Item));
            top.push(Section {
                id,
                title,
                kind: SectionKind::Item,
                parent: None,
                start,
                end,
            });
            top.sort_by_key(|s| s.start);
            for k in 0..top.len() {
                top[k].end = top.get(k + 1).map(|s| s.start).unwrap_or(n);
            }
        }
    }

    // Cover: everything before the first section.
    let first = top.iter().map(|s| s.start).min().unwrap_or(n);
    let mut all: Vec<Section> = Vec::new();
    if top.is_empty() {
        all.push(Section {
            id: "document".to_string(),
            title: "Full document".to_string(),
            kind: SectionKind::Document,
            parent: None,
            start: 0,
            end: n,
        });
    } else if first > 0 {
        all.push(Section {
            id: "cover".to_string(),
            title: "Cover page and table of contents".to_string(),
            kind: SectionKind::Cover,
            parent: None,
            start: 0,
            end: first,
        });
    }
    // Items that only point elsewhere ("Management's discussion and analysis
    // ... appears on pages 46-160", integrated annual reports): the pages they
    // name, so section=mda returns the discussion, not the pointer.
    let pages = page_starts(doc);
    let mut references: Vec<Section> = Vec::new();
    for s in top.iter().filter(|s| s.kind == SectionKind::Item) {
        if doc.chars_range(s.start, s.end) > 1500 {
            continue;
        }
        let text = doc.text_range(s.start, s.end);
        let Some(c) = PAGES_REF_RE.captures(&text) else {
            continue;
        };
        let (Some(p1), Some(p2)) = (
            c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()),
            c.get(2).and_then(|m| m.as_str().parse::<u32>().ok()),
        ) else {
            continue;
        };
        let Some(&ps) = pages.get(&p1) else {
            continue;
        };
        let pe = pages
            .iter()
            .filter(|(p, _)| **p > p2)
            .map(|(_, b)| *b)
            .min()
            .unwrap_or(n);
        if pe <= ps || (ps >= s.start && ps < s.end) {
            continue;
        }
        references.push(Section {
            id: format!("{}_referenced", s.id),
            title: format!("{} (pages {p1}-{p2}, as referenced)", s.title),
            kind: SectionKind::Reference,
            parent: Some(s.id.clone()),
            start: ps,
            end: pe,
        });
    }

    // Clip children to their parents' final ranges.
    let parent_range: HashMap<String, (usize, usize)> =
        top.iter().map(|s| (s.id.clone(), (s.start, s.end))).collect();
    for c in children.iter_mut().chain(subsections.iter_mut()) {
        if let Some(pid) = c.parent.as_ref() {
            if let Some((_, pe)) = parent_range.get(pid) {
                c.end = c.end.min(*pe);
            }
        }
    }
    all.extend(top);
    all.extend(children);
    all.extend(references);
    all.extend(subsections.into_iter().filter(|s| s.parent.as_ref().is_some_and(|p| parent_range.contains_key(p))));
    all.retain(|s| s.end > s.start);
    all.sort_by_key(|s| (s.start, s.kind.level()));
    let chars = all
        .iter()
        .map(|s| (s.id.clone(), doc.chars_range(s.start, s.end)))
        .collect();
    Structure {
        sections: all,
        family,
        method,
        chars,
    }
}

/// End of the front table of contents (first TOC table, or the first run of
/// TOC lines), so a closing cross-reference index doesn't count.
fn front_toc_end(doc: &ParsedDoc, toc: &[bool]) -> usize {
    let Some(first) = toc.iter().position(|t| *t) else {
        return 0;
    };
    let table = doc.blocks[first].table;
    let mut end = first + 1;
    let mut i = first + 1;
    while i < doc.blocks.len() {
        let b = &doc.blocks[i];
        let same_table = table != 0 && b.kind == BlockKind::Row && b.table == table;
        if toc[i] || same_table {
            end = i + 1;
        } else if !(b.furniture || b.kind == BlockKind::PageBreak) && i > end + 3 {
            break;
        }
        i += 1;
    }
    end
}

/// A paragraph that opens with its own heading in capitals: "MANAGEMENT'S
/// DISCUSSION AND ANALYSIS ... (MD&A). The consolidated ..." -> the capitals.
fn run_in_heading(text: &str) -> Option<&str> {
    let t = text.trim_start();
    let (end, _) = t
        .char_indices()
        .take(240)
        .find(|(i, c)| *c == '.' && t[i + 1..].starts_with(' '))?;
    let head = t[..end].trim();
    let letters: Vec<char> = head.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() < 6 || head.split_whitespace().count() < 2 {
        return None;
    }
    if letters.iter().all(|c| c.is_uppercase()) {
        Some(head)
    } else {
        None
    }
}

/// Title key of a block that can serve as a heading: a short standalone line,
/// or the capitalized run-in heading that opens a paragraph.
fn block_heading_key(doc: &ParsedDoc, toc: &[bool], i: usize) -> Option<String> {
    let b = &doc.blocks[i];
    if b.furniture || toc[i] || b.kind == BlockKind::PageBreak {
        return None;
    }
    if is_heading_like(doc, toc, i, 200) {
        return Some(title_key(&b.heading_text()));
    }
    if b.kind == BlockKind::Para {
        return run_in_heading(&b.text).map(title_key);
    }
    None
}

/// "item 7 management s discussion" -> "management s discussion".
fn strip_item_prefix(key: &str) -> &str {
    let mut k = key;
    for prefix in ["part ", "item ", "items "] {
        if let Some(rest) = k.strip_prefix(prefix) {
            k = rest.split_once(' ').map(|(_, r)| r).unwrap_or("");
        }
    }
    k
}

fn titles_match(block_key: &str, entry_key: &str) -> bool {
    let b = strip_parenthetical(strip_item_prefix(block_key));
    let e = strip_parenthetical(strip_item_prefix(entry_key));
    if b.is_empty() || e.is_empty() {
        return false;
    }
    b == e
        || (b.starts_with(&e) && e.len() >= 6)
        || (e.starts_with(&b) && b.len() >= 12)
}

/// Page number -> first block of that page, from the page numbers printed in
/// the running footers (they are furniture in the text, but they map the
/// table of contents onto the body).
fn page_starts(doc: &ParsedDoc) -> HashMap<u32, usize> {
    let blocks = &doc.blocks;
    let page_num = |b: &super::html::Block| -> Option<u32> {
        // Only running footers (already flagged as furniture) count; a TOC row
        // ending in "34" right before a break is not a page number.
        if !b.furniture || b.kind == BlockKind::PageBreak || b.text.chars().count() > 160 {
            return None;
        }
        // Footers put the number last ("2025 FORM 10-K 5") or, on facing
        // pages, first ("4 2025 FORM 10-K").
        let (first, last) = match b.kind {
            BlockKind::Row => (
                b.cells.first().map(|c| c.trim().to_string())?,
                b.cells.last().map(|c| c.trim().to_string())?,
            ),
            _ => (
                b.text.split_whitespace().next()?.to_string(),
                b.text.split_whitespace().last()?.to_string(),
            ),
        };
        let as_page = |t: &str| -> Option<u32> {
            if !t.is_empty() && t.len() <= 3 && t.chars().all(|c| c.is_ascii_digit()) {
                t.parse().ok()
            } else {
                None
            }
        };
        as_page(&last).or_else(|| as_page(&first))
    };
    let mut map = HashMap::new();
    let mut start = 0usize;
    let mut last_page = 0u32;
    for i in 0..blocks.len() {
        if blocks[i].kind != BlockKind::PageBreak {
            continue;
        }
        let mut j = i;
        let mut looked = 0;
        while j > start && looked < 4 {
            j -= 1;
            looked += 1;
            if let Some(p) = page_num(&blocks[j]) {
                // Page numbers only go up; anything else is a table cell.
                if p > last_page {
                    map.entry(p).or_insert(start);
                    last_page = p;
                }
                break;
            }
        }
        start = i + 1;
    }
    map
}

#[derive(Debug)]
struct TocEntry {
    title: String,
    page: Option<u32>,
    item: Option<String>,
}

fn first_number(s: &str) -> Option<u32> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// The filing's front table of contents as entries (title, page, item id).
fn parse_front_toc(doc: &ParsedDoc, toc: &[bool], family: FormFamily) -> Vec<TocEntry> {
    let Some(first) = toc.iter().position(|t| *t) else {
        return Vec::new();
    };
    let end = front_toc_end(doc, toc);
    let mut out = Vec::new();
    let mut part = 1u8;
    for i in first..end {
        let b = &doc.blocks[i];
        if !toc[i] {
            continue;
        }
        let mut cells: Vec<&str> = if b.kind == BlockKind::Row {
            b.cells.iter().map(String::as_str).collect()
        } else {
            vec![b.text.as_str()]
        };
        let mut page = None;
        while let Some(last) = cells.last() {
            if is_page_ref(last) {
                page = first_number(last).or(page);
                cells.pop();
            } else {
                break;
            }
        }
        let mut title = cells.join(" ").trim().to_string();
        if b.kind == BlockKind::Para {
            // "Item 7. Management's Discussion ....... 25"
            if let Some((head, tail)) = title.rsplit_once(' ') {
                if tail.chars().all(|c| c.is_ascii_digit()) {
                    page = tail.parse().ok();
                    title = head.trim_end_matches(['.', ' ']).to_string();
                }
            }
        }
        if title.is_empty() || title_key(&title) == "page" {
            continue;
        }
        if let Some(c) = PART_RE.captures(&title) {
            let p = c.get(1).map(|m| m.as_str().to_ascii_lowercase()).unwrap_or_default();
            part = match p.as_str() {
                "ii" | "2" => 2,
                "iii" | "3" => 3,
                "iv" | "4" => 4,
                _ => 1,
            };
            let rest = c.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            if !ITEM_RE.is_match(&rest) {
                continue;
            }
            title = rest;
        }
        let (item, clean) = if let Some(c) = ITEM_RE.captures(&title) {
            let num = format!(
                "{}{}",
                c.get(1).map(|m| m.as_str()).unwrap_or(""),
                c.get(2).map(|m| m.as_str().to_ascii_lowercase()).unwrap_or_default()
            );
            let rest = c.get(3).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            let id = match family {
                FormFamily::TenQ => {
                    let mapped = map_title_to_item(family, &rest).filter(|m| m.ends_with(&format!("_item{num}")));
                    match mapped {
                        Some(m) => Some(m.to_string()),
                        None if matches!(num.as_str(), "1a" | "5" | "6") => Some(format!("part2_item{num}")),
                        None => Some(format!("part{part}_item{num}")),
                    }
                }
                _ => Some(format!("item{num}")),
            }
            .filter(|id| match family {
                FormFamily::TenK => TEN_K_ITEMS.iter().any(|d| d.id == id),
                FormFamily::TenQ => TEN_Q_ITEMS.iter().any(|d| d.id == id),
                _ => true,
            });
            (id, rest)
        } else {
            (map_title_to_item(family, &title).map(str::to_string), title.clone())
        };
        out.push(TocEntry { title: clean, page, item });
    }
    out
}

/// Where a TOC entry starts in the body: its heading on the page the TOC
/// names (or anywhere after `from` when it names none). Falls back to the
/// top of that page when the heading text can't be matched.
fn locate_toc_entry(
    doc: &ParsedDoc,
    toc: &[bool],
    pages: &HashMap<u32, usize>,
    e: &TocEntry,
    from: usize,
) -> Option<(usize, bool)> {
    let key = title_key(&e.title);
    if key.is_empty() {
        return None;
    }
    let n = doc.blocks.len();
    if let Some(&ps) = e.page.and_then(|p| pages.get(&p)) {
        let pe = e
            .page
            .and_then(|p| pages.get(&(p + 1)).copied())
            .filter(|&x| x > ps)
            .unwrap_or((ps + 400).min(n));
        for i in ps..pe.min(n) {
            if let Some(k) = block_heading_key(doc, toc, i) {
                if titles_match(&k, &key) {
                    return Some((i, true));
                }
            }
        }
        return Some((ps, false));
    }
    (from..n)
        .find(|&i| block_heading_key(doc, toc, i).is_some_and(|k| titles_match(&k, &key)))
        .map(|i| (i, true))
}

fn is_signature_or_index(key: &str) -> bool {
    key == "signatures"
        || key == "signature"
        || key.starts_with("exhibit index")
        || key.starts_with("index to exhibits")
}

/// Filers without "Item N." headings (Intel, GE): the TOC entries located in
/// the body (by page and heading text) and mapped onto items; entries that
/// aren't items become subsections of the item before them.
fn sections_from_toc_titles(doc: &ParsedDoc, toc: &[bool], family: FormFamily) -> (Vec<Section>, Vec<Section>) {
    let n = doc.blocks.len();
    let entries = parse_front_toc(doc, toc, family);
    if entries.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let pages = page_starts(doc);
    let toc_end = front_toc_end(doc, toc);
    let mut items: Vec<Section> = Vec::new();
    let mut subs: Vec<Section> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    let mut last_mapped: Option<String> = None;
    let mut cursor = toc_end;
    for e in &entries {
        let key = title_key(&e.title);
        if key.len() < 4
            || is_statement_title_key(&key)
            || (key.contains("notes to") && key.contains("financial statements"))
            || NOTE_RE.is_match(&e.title)
            || is_signature_or_index(&key)
        {
            continue;
        }
        let mapped = e.item.clone().filter(|id| !used.contains(id));
        // A TOC group header with no page of its own ("Risk Factors and Other
        // Key Information") only matters if it names an item.
        if mapped.is_none() && e.page.is_none() {
            continue;
        }
        let Some((start, exact)) = locate_toc_entry(doc, toc, &pages, e, cursor) else {
            continue;
        };
        if start < toc_end {
            continue;
        }
        if exact {
            cursor = start + 1;
        }
        match mapped {
            Some(id) => {
                used.push(id.clone());
                last_mapped = Some(id.clone());
                let def = canonical_items(family).iter().find(|d| d.id == id);
                items.push(Section {
                    title: item_display_title(family, &id, def.map(|d| d.num).unwrap_or(""), &strip_parenthetical_title(&e.title)),
                    id,
                    kind: SectionKind::Item,
                    parent: None,
                    start: pull_up_caption(doc, start, toc_end),
                    end: n,
                });
            }
            None => {
                let sec = Section {
                    id: slug(&e.title),
                    title: e.title.clone(),
                    kind: if last_mapped.is_some() { SectionKind::Subsection } else { SectionKind::Item },
                    parent: last_mapped.clone(),
                    start,
                    end: n,
                };
                if last_mapped.is_some() {
                    subs.push(sec);
                } else {
                    items.push(sec);
                }
            }
        }
    }
    // Only keep the result if it actually mapped onto items.
    if !items.iter().any(|s| canonical_index(family, &s.id).is_some() && s.id.contains("item")) {
        return (Vec::new(), Vec::new());
    }
    items.sort_by_key(|s| s.start);
    items.dedup_by_key(|s| s.start);
    for k in 0..items.len() {
        items[k].end = items.get(k + 1).map(|s| s.start).unwrap_or(n);
    }
    // Subsections end at the next subsection of the same parent or the parent's end.
    subs.sort_by_key(|s| s.start);
    let ends: Vec<usize> = subs
        .iter()
        .enumerate()
        .map(|(k, s)| {
            let parent_end = items
                .iter()
                .find(|p| Some(&p.id) == s.parent.as_ref())
                .map(|p| p.end)
                .unwrap_or(n);
            subs.iter()
                .skip(k + 1)
                .find(|o| o.parent == s.parent)
                .map(|o| o.start)
                .unwrap_or(parent_end)
                .min(parent_end)
        })
        .collect();
    for (s, e) in subs.iter_mut().zip(ends) {
        s.end = e;
    }
    // A subsection must sit inside its parent.
    subs.retain(|s| {
        items
            .iter()
            .find(|p| Some(&p.id) == s.parent.as_ref())
            .is_some_and(|p| s.start >= p.start && s.start < p.end)
    });
    // Unique ids for repeated subsection titles.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for s in subs.iter_mut() {
        let c = counts.entry(s.id.clone()).or_insert(0);
        *c += 1;
        if *c > 1 {
            s.id = format!("{}_{}", s.id, c);
        }
    }
    (items, subs)
}

/// Items the body never heads with "Item N." (JPMorgan prints MD&A and the
/// statements without them) but the TOC lists: placed via the TOC entry, or
/// via its first sub-entry that carries a page number.
fn fill_missing_items(doc: &ParsedDoc, toc: &[bool], family: FormFamily, top: &mut Vec<Section>) {
    let entries = parse_front_toc(doc, toc, family);
    if entries.is_empty() {
        return;
    }
    let pages = page_starts(doc);
    let toc_end = front_toc_end(doc, toc);
    let n = doc.blocks.len();
    for (idx, e) in entries.iter().enumerate() {
        let Some(id) = e.item.as_deref() else {
            continue;
        };
        if top.iter().any(|s| s.id == id) || canonical_index(family, id).is_none() {
            continue;
        }
        let mut located = if e.page.is_some() {
            locate_toc_entry(doc, toc, &pages, e, toc_end)
        } else {
            None
        };
        if located.is_none() {
            // First sub-entry (before the next item entry) that names a page.
            located = entries
                .iter()
                .skip(idx + 1)
                .take_while(|s| s.item.is_none())
                .filter(|s| s.page.is_some())
                .find_map(|s| locate_toc_entry(doc, toc, &pages, s, toc_end));
        }
        let Some((start, _)) = located else {
            continue;
        };
        if start < toc_end || start >= n || top.iter().any(|s| s.start == start) {
            continue;
        }
        let def = canonical_items(family).iter().find(|d| d.id == id);
        top.push(Section {
            id: id.to_string(),
            title: item_display_title(family, id, def.map(|d| d.num).unwrap_or(""), &e.title),
            kind: SectionKind::Item,
            parent: None,
            start: pull_up_caption(doc, start, toc_end),
            end: n,
        });
    }
    top.sort_by_key(|s| s.start);
}

fn strip_parenthetical(key: &str) -> String {
    // title_key already dropped the parentheses; drop a trailing short acronym ("md a").
    let k = key.trim();
    if let Some(stripped) = k.strip_suffix(" md a") {
        return stripped.to_string();
    }
    k.to_string()
}

fn strip_parenthetical_title(t: &str) -> String {
    match t.find('(') {
        Some(p) if p > 10 => t[..p].trim().to_string(),
        _ => t.trim().to_string(),
    }
}

/// Last resort: canonical item titles as standalone heading lines.
fn sections_from_title_match(doc: &ParsedDoc, toc: &[bool], family: FormFamily) -> Vec<Section> {
    let n = doc.blocks.len();
    let after_toc = front_toc_end(doc, toc);
    let mut out: Vec<Section> = Vec::new();
    for i in after_toc..n {
        if !is_heading_like(doc, toc, i, 110) {
            continue;
        }
        let text = doc.blocks[i].heading_text().to_string();
        if text.trim_end().ends_with('.') {
            continue;
        }
        if let Some(id) = map_title_to_item(family, &text) {
            if out.iter().any(|s| s.id == id) {
                continue;
            }
            let def = canonical_items(family).iter().find(|d| d.id == id);
            out.push(Section {
                id: id.to_string(),
                title: item_display_title(family, id, def.map(|d| d.num).unwrap_or(""), &text),
                kind: SectionKind::Item,
                parent: None,
                start: i,
                end: n,
            });
        }
    }
    out.sort_by_key(|s| s.start);
    for k in 0..out.len() {
        out[k].end = out.get(k + 1).map(|s| s.start).unwrap_or(n);
    }
    out
}

// ───────────────────────────── lookup ─────────────────────────────

/// Resolve a requested section (exact TOC id or alias) to a section.
pub(crate) fn resolve_section<'a>(structure: &'a Structure, query: &str) -> Option<&'a Section> {
    let secs = &structure.sections;
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let ql = q.to_ascii_lowercase();
    if let Some(s) = secs.iter().find(|s| s.id == ql) {
        return Some(s);
    }
    let norm = ql.replace([' ', '-'], "_").replace("__", "_");
    if let Some(s) = secs.iter().find(|s| s.id == norm) {
        return Some(s);
    }

    // Notes: "note:5", "note 5", "note:inventories", "note:5-inventories".
    let note_q = ql
        .strip_prefix("note:")
        .or_else(|| ql.strip_prefix("note "))
        .or_else(|| ql.strip_prefix("note_"))
        .or_else(|| ql.strip_prefix("footnote:"));
    if let Some(nq) = note_q {
        return resolve_note(secs, nq.trim());
    }

    let fam = structure.family;
    let alias: Option<Vec<&str>> = match norm.as_str() {
        "mda" | "md&a" | "md_a" | "management_discussion" | "managements_discussion" | "management_s_discussion_and_analysis" => Some(match fam {
            FormFamily::TenK => vec!["item7"],
            FormFamily::TenQ => vec!["part1_item2"],
            FormFamily::TwentyF => vec!["item5"],
            _ => vec!["item7", "part1_item2", "item5"],
        }),
        "risk_factors" | "risks" | "risk" => Some(match fam {
            FormFamily::TenK => vec!["item1a"],
            FormFamily::TenQ => vec!["part2_item1a"],
            FormFamily::TwentyF => vec!["item3"],
            _ => vec!["item1a", "part2_item1a"],
        }),
        "business" => Some(vec!["item1", "item4"]),
        "properties" => Some(vec!["item2"]),
        "cybersecurity" | "cyber" => Some(vec!["item1c", "item16k"]),
        "legal_proceedings" | "legal" | "litigation" => Some(match fam {
            FormFamily::TenQ => vec!["part2_item1"],
            _ => vec!["item3"],
        }),
        "market_risk" | "quantitative_and_qualitative" | "quantitative_qualitative" => Some(match fam {
            FormFamily::TenQ => vec!["part1_item3"],
            FormFamily::TwentyF => vec!["item11"],
            _ => vec!["item7a"],
        }),
        "controls" | "controls_and_procedures" => Some(match fam {
            FormFamily::TenQ => vec!["part1_item4"],
            FormFamily::TwentyF => vec!["item15"],
            _ => vec!["item9a"],
        }),
        "financial_statements" | "statements" | "financials" | "fs" => Some(match fam {
            FormFamily::TenQ => vec!["part1_item1", "financial_statements"],
            FormFamily::TwentyF => vec!["item18", "item17", "item8", "financial_statements"],
            _ => vec!["item8", "financial_statements"],
        }),
        "exhibits" | "exhibit_index" => Some(match fam {
            FormFamily::TenQ => vec!["part2_item6"],
            FormFamily::TwentyF => vec!["item19"],
            FormFamily::EightK => vec!["item9.01"],
            _ => vec!["item15"],
        }),
        "other_information" => Some(match fam {
            FormFamily::TenQ => vec!["part2_item5"],
            _ => vec!["item9b"],
        }),
        "unregistered_sales" | "buybacks" | "repurchases" | "share_repurchases" | "issuer_purchases" => Some(match fam {
            FormFamily::TenQ => vec!["part2_item2"],
            _ => vec!["item5"],
        }),
        "executive_compensation" | "compensation" => Some(vec!["item11"]),
        "income_statement" | "is" | "operations" | "income" | "statement_of_operations" | "statements_of_operations"
        | "p&l" | "pnl" | "earnings" | "statement_of_income" => Some(vec!["income_statement"]),
        "comprehensive_income" | "oci" | "statement_of_comprehensive_income" => {
            Some(vec!["comprehensive_income", "income_statement"])
        }
        "balance_sheet" | "bs" | "balance" | "financial_position" | "financial_condition" | "statement_of_financial_position" => {
            Some(vec!["balance_sheet"])
        }
        "cash_flow" | "cash_flows" | "cashflow" | "cashflows" | "cf" | "statement_of_cash_flows" => {
            Some(vec!["cash_flow"])
        }
        "equity" | "stockholders_equity" | "shareholders_equity" | "changes_in_equity" | "statement_of_equity" => {
            Some(vec!["equity"])
        }
        "notes" | "footnotes" | "notes_to_financial_statements" | "all_notes" => Some(vec!["notes"]),
        "auditor" | "auditors_report" | "auditor_report" | "audit_report" | "audit_opinion" => {
            Some(vec!["auditor_report"])
        }
        "cover" | "cover_page" => Some(vec!["cover"]),
        "signatures" | "signature" => Some(vec!["signatures"]),
        _ => None,
    };
    if let Some(ids) = alias {
        let stub = |id: &str| structure.chars.get(id).copied().unwrap_or(usize::MAX) < 1500;
        for id in &ids {
            if let Some(s) = secs.iter().find(|s| s.id == *id) {
                // An item that only cross-references its pages: give the pages.
                let referenced = format!("{id}_referenced");
                if let Some(r) = secs.iter().find(|r| r.id == referenced) {
                    return Some(r);
                }
                // "See the F-pages": the statements section found elsewhere.
                if stub(id) {
                    if let Some(f) = ids
                        .iter()
                        .filter(|o| *o != id)
                        .find_map(|o| secs.iter().find(|x| x.id == *o && !stub(o)))
                    {
                        return Some(f);
                    }
                }
                return Some(s);
            }
        }
        return None;
    }

    // "item 1a", "item_7", "7a", "part 2 item 1a", "item 2.02".
    let compact: String = norm.replace('_', "");
    let item_q = compact
        .strip_prefix("item")
        .map(str::to_string)
        .or_else(|| {
            if compact.chars().next().is_some_and(|c| c.is_ascii_digit()) && compact.len() <= 5 {
                Some(compact.clone())
            } else {
                None
            }
        });
    if let Some(iq) = item_q {
        let cands = [
            format!("item{iq}"),
            format!("part1_item{iq}"),
            format!("part2_item{iq}"),
        ];
        for c in cands {
            if let Some(s) = secs.iter().find(|s| s.id == c) {
                return Some(s);
            }
        }
    }
    if let Some(rest) = compact.strip_prefix("part") {
        // "part2item1a" / "partiiitem1a"
        let rest = rest.replacen("ii", "2", 1).replacen('i', "1", 1);
        if let Some((p, it)) = rest.split_once("item") {
            let id = format!("part{p}_item{it}");
            if let Some(s) = secs.iter().find(|s| s.id == id) {
                return Some(s);
            }
        }
    }

    // Fuzzy: the query's words inside a section title.
    let qk = title_key(q);
    if qk.len() >= 3 {
        if let Some(s) = secs
            .iter()
            .filter(|s| s.kind != SectionKind::Note)
            .find(|s| title_key(&s.title).contains(&qk))
        {
            return Some(s);
        }
        return resolve_note(secs, q);
    }
    None
}

/// A note that carries the asked-for topic as a subheading rather than in its
/// title ("note:inventories" -> Intel's "Note 5. Other Financial Statement
/// Details", which has an "Inventories" heading inside).
pub(crate) fn resolve_by_subheading<'a>(structure: &'a Structure, doc: &ParsedDoc, query: &str) -> Option<&'a Section> {
    let q = query.trim().to_ascii_lowercase();
    let q = q
        .strip_prefix("note:")
        .or_else(|| q.strip_prefix("note "))
        .unwrap_or(&q)
        .to_string();
    let qk = title_key(&q);
    if qk.len() < 4 {
        return None;
    }
    let singular = qk.trim_end_matches('s').to_string();
    structure
        .sections
        .iter()
        .filter(|s| s.kind == SectionKind::Note)
        .find(|s| {
            (s.start..s.end.min(doc.blocks.len())).any(|i| {
                let b = &doc.blocks[i];
                if b.furniture || b.kind == BlockKind::PageBreak {
                    return false;
                }
                let t = b.heading_text();
                if t.chars().count() > 80 {
                    return false;
                }
                let k = title_key(&t);
                k == qk || k.starts_with(&qk) || (singular.len() >= 4 && k.starts_with(&singular))
            })
        })
}

fn resolve_note<'a>(secs: &'a [Section], nq: &str) -> Option<&'a Section> {
    let notes: Vec<&Section> = secs.iter().filter(|s| s.kind == SectionKind::Note).collect();
    if notes.is_empty() {
        return None;
    }
    let nq = nq.trim();
    // Leading number: "5", "5-inventories", "5. Inventories".
    let digits: String = nq.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let id = format!("note:{}", digits.trim_start_matches('0'));
        if let Some(s) = notes.iter().find(|s| s.id == id) {
            return Some(s);
        }
    }
    let qk = title_key(nq.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | ':' | ' ' | '_')));
    let qk = qk.replace('_', " ");
    if qk.is_empty() {
        return None;
    }
    let note_key = |s: &Section| {
        let t = title_key(&s.title);
        // drop the leading "note N "
        let mut words = t.split(' ');
        let first = words.next().unwrap_or("");
        if first == "note" {
            words.next();
            words.collect::<Vec<_>>().join(" ")
        } else {
            t
        }
    };
    let singular = |w: &str| w.trim_end_matches('s').to_string();
    // exact, prefix, contains, then every query word (singular/plural-insensitive).
    if let Some(s) = notes.iter().find(|s| note_key(s) == qk) {
        return Some(s);
    }
    if let Some(s) = notes.iter().find(|s| note_key(s).starts_with(&qk)) {
        return Some(s);
    }
    if let Some(s) = notes.iter().find(|s| note_key(s).contains(&qk)) {
        return Some(s);
    }
    let qwords: Vec<String> = qk.split(' ').filter(|w| w.len() > 2).map(singular).collect();
    if qwords.is_empty() {
        return None;
    }
    notes.into_iter().find(|s| {
        let words: Vec<String> = note_key(s).split(' ').map(singular).collect();
        qwords.iter().all(|q| words.iter().any(|w| w.starts_with(q.as_str())))
    })
}

#[cfg(test)]
mod tests {
    use super::super::html::parse_filing_html;
    use super::*;

    fn doc_10q() -> String {
        let mut h = String::from("<html><body>");
        h.push_str("<p>FORM 10-Q</p><p>INDEX</p><table>");
        for (item, title, page) in [
            ("Item 1.", "Financial Statements", 3),
            ("Item 2.", "Management's Discussion and Analysis of Financial Condition and Results of Operations", 20),
            ("Item 3.", "Quantitative and Qualitative Disclosures About Market Risk", 30),
            ("Item 4.", "Controls and Procedures", 31),
            ("Item 1.", "Legal Proceedings", 32),
            ("Item 1A.", "Risk Factors", 32),
            ("Item 6.", "Exhibits", 40),
        ] {
            h.push_str(&format!("<tr><td>{item}</td><td>{title}</td><td>{page}</td></tr>"));
        }
        h.push_str("</table><hr style=\"page-break-after:always\"/>");
        h.push_str("<p>PART I. FINANCIAL INFORMATION</p><table><tr><td>Item 1.</td><td>Financial Statements</td></tr></table>");
        h.push_str("<p>ACME CORP</p><p>CONSOLIDATED STATEMENTS OF OPERATIONS</p><p>(in millions)</p>");
        h.push_str("<table><tr><td></td><td>2026</td><td>2025</td></tr><tr><td>Revenue</td><td>$</td><td>100</td><td>$</td><td>90</td></tr><tr><td>Cost of sales</td><td>60</td><td>55</td></tr><tr><td>Operating expenses</td><td>20</td><td>18</td></tr><tr><td>Net income</td><td>$</td><td>20</td><td>$</td><td>17</td></tr></table>");
        h.push_str("<p>ACME CORP</p><p>CONSOLIDATED BALANCE SHEETS</p>");
        h.push_str("<table><tr><td></td><td>2026</td><td>2025</td></tr><tr><td>Inventories</td><td>14,541</td><td>5,718</td></tr><tr><td>Total current assets</td><td>173,857</td><td>147,957</td></tr><tr><td>Total assets</td><td>404,548</td><td>359,241</td></tr></table>");
        h.push_str("<p>CONSOLIDATED STATEMENTS OF CASH FLOWS</p>");
        h.push_str("<table><tr><td></td><td>2026</td><td>2025</td></tr><tr><td>Net income</td><td>10</td><td>9</td></tr><tr><td>Purchases of property and equipment</td><td>(5</td><td>)</td><td>(4</td><td>)</td></tr></table>");
        h.push_str("<p>See accompanying notes.</p>");
        h.push_str("<p>NOTES TO CONSOLIDATED FINANCIAL STATEMENTS</p>");
        h.push_str("<p>Note 1 — ACCOUNTING POLICIES</p><p>We prepare statements under GAAP.</p>");
        h.push_str("<p>Note 2 — INVENTORIES</p><p>Inventories rose because we pre-bought memory.</p>");
        h.push_str("<p>Note 3 — DEBT</p><p>We issued notes. See Note 2 for details.</p>");
        h.push_str("<table><tr><td>Item 2.</td><td>Management's Discussion and Analysis of Financial Condition and Results of Operations</td></tr></table>");
        h.push_str("<p>Revenue grew 11%. Long discussion of results follows here in many words.</p>");
        h.push_str("<table><tr><td>Item 3.</td><td>Quantitative and Qualitative Disclosures About Market Risk</td></tr></table><p>Rates.</p>");
        h.push_str("<table><tr><td>Item 4.</td><td>Controls and Procedures</td></tr></table><p>Effective.</p>");
        h.push_str("<p>PART II. OTHER INFORMATION</p>");
        h.push_str("<table><tr><td>Item 1.</td><td>Legal Proceedings</td></tr></table><p>See Item 1 of Part I, Note 3.</p>");
        h.push_str("<table><tr><td>Item 1A.</td><td>Risk Factors</td></tr></table><p>Many risks exist.</p>");
        h.push_str("<table><tr><td>Item 6.</td><td>Exhibits</td></tr></table><p>31.1 Certification</p>");
        h.push_str("<p>SIGNATURES</p><p>/s/ CFO</p></body></html>");
        h
    }

    #[test]
    fn standard_10q_toc_statements_and_notes() {
        let doc = parse_filing_html(&doc_10q());
        let st = detect_structure(&doc, "10-Q");
        assert_eq!(st.method, "item_headings");
        let ids: Vec<&str> = st.sections.iter().map(|s| s.id.as_str()).collect();
        for want in [
            "cover", "part1_item1", "income_statement", "balance_sheet", "cash_flow", "notes", "note:1",
            "note:2", "note:3", "part1_item2", "part1_item3", "part1_item4", "part2_item1", "part2_item1a",
            "part2_item6", "signatures",
        ] {
            assert!(ids.contains(&want), "missing {want}: {ids:?}");
        }
        // Cash flow statement: exactly the statement, through its last row.
        let cf = resolve_section(&st, "cash_flow").unwrap();
        let text = doc.text_range(cf.start, cf.end);
        assert!(text.starts_with("CONSOLIDATED STATEMENTS OF CASH FLOWS"), "{text}");
        assert!(text.contains("Purchases of property and equipment"));
        assert!(text.contains("(5)"));
        assert!(!text.contains("Note 1"), "{text}");
        // The company caption above the balance sheet moves with it.
        let bs = resolve_section(&st, "bs").unwrap();
        assert!(doc.text_range(bs.start, bs.end).starts_with("ACME CORP\nCONSOLIDATED BALANCE SHEETS"));
        // Notes by name and number.
        let inv = resolve_section(&st, "note:inventories").unwrap();
        assert_eq!(inv.id, "note:2");
        assert_eq!(inv.title, "Note 2. INVENTORIES");
        let inv_text = doc.text_range(inv.start, inv.end);
        assert!(inv_text.contains("pre-bought memory"));
        assert!(!inv_text.contains("DEBT"));
        assert_eq!(resolve_section(&st, "note 3").unwrap().id, "note:3");
        // The last note stops at MD&A.
        let debt = resolve_section(&st, "note:debt").unwrap();
        assert!(!doc.text_range(debt.start, debt.end).contains("Revenue grew"));
        // Aliases.
        assert_eq!(resolve_section(&st, "mda").unwrap().id, "part1_item2");
        assert_eq!(resolve_section(&st, "risk_factors").unwrap().id, "part2_item1a");
        assert_eq!(resolve_section(&st, "Item 1A").unwrap().id, "part2_item1a");
        let notes = resolve_section(&st, "notes").unwrap();
        let notes_text = doc.text_range(notes.start, notes.end);
        assert!(notes_text.contains("Note 1") && notes_text.contains("Note 3"));
        assert!(!notes_text.contains("Revenue grew"));
        // TOC rows never become sections: Part I Item 1 starts after the index.
        let p1 = resolve_section(&st, "part1_item1").unwrap();
        assert!(doc.text_range(p1.start, p1.end).starts_with("Item 1. Financial Statements"));
    }

    /// Dev harness: `ELI_FILING_HTML=/path/doc.htm ELI_FILING_FORM=10-Q
    /// [ELI_FILING_SECTION=cash_flow] cargo test -p eli-core --lib
    /// dump_filing_structure -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dump_filing_structure() {
        let Ok(path) = std::env::var("ELI_FILING_HTML") else {
            return;
        };
        let form = std::env::var("ELI_FILING_FORM").unwrap_or_else(|_| "10-Q".to_string());
        let raw = std::fs::read_to_string(&path).expect("read filing");
        let t0 = std::time::Instant::now();
        let doc = parse_filing_html(&raw);
        let st = detect_structure(&doc, &form);
        eprintln!("method={} sections={} in {:?}", st.method, st.sections.len(), t0.elapsed());
        for s in &st.sections {
            println!(
                "{}{:<22} {:>7}  [{}..{}) {}",
                "  ".repeat((s.kind.level() - 1) as usize),
                s.id,
                doc.chars_range(s.start, s.end),
                s.start,
                s.end,
                s.title
            );
        }
        if let Ok(q) = std::env::var("ELI_FILING_SECTION") {
            for one in q.split(',') {
                match resolve_section(&st, one) {
                    Some(s) => println!("\n===== {} -> {} ({})\n{}", one, s.id, s.title, doc.text_range(s.start, s.end)),
                    None => println!("\n===== {one}: not found"),
                }
            }
        }
    }

    #[test]
    #[ignore]
    fn dump_toc_entries() {
        let Ok(path) = std::env::var("ELI_FILING_HTML") else {
            return;
        };
        let form = std::env::var("ELI_FILING_FORM").unwrap_or_else(|_| "10-Q".to_string());
        let raw = std::fs::read_to_string(&path).expect("read filing");
        let doc = parse_filing_html(&raw);
        let toc = toc_blocks(&doc);
        let fam = form_family(&form);
        let pages = page_starts(&doc);
        let mut pv: Vec<_> = pages.iter().collect();
        pv.sort();
        println!("toc_end={} pages={:?}", front_toc_end(&doc, &toc), &pv[..pv.len().min(400)]);
        for e in parse_front_toc(&doc, &toc, fam) {
            let loc = locate_toc_entry(&doc, &toc, &pages, &e, front_toc_end(&doc, &toc));
            println!("{:?} -> {:?}", e, loc);
        }
    }

    #[test]
    fn page_refs_and_title_keys() {
        assert!(is_page_ref("12"));
        assert!(is_page_ref("Pages 3 - 30"));
        assert!(is_page_ref("Page 45"));
        assert!(is_page_ref("F-3"));
        assert!(is_page_ref("Not applicable"));
        assert!(!is_page_ref("2,345"));
        assert!(!is_page_ref("Financial Statements"));
        assert_eq!(title_key("Management’s Discussion & Analysis (MD&A)"), "management s discussion analysis md a");
        assert_eq!(statement_kind("consolidated statements of operations and comprehensive income loss"), Some("income_statement"));
        assert_eq!(statement_kind("consolidated statements of comprehensive income"), Some("comprehensive_income"));
        assert_eq!(statement_kind("consolidated condensed statements of stockholders equity"), Some("equity"));
        assert_eq!(statement_kind("condensed consolidated balance sheets"), Some("balance_sheet"));
        assert_eq!(statement_kind("notes to consolidated financial statements"), None);
    }
}
