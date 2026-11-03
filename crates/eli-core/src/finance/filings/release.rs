//! Earnings-release tables: the financial statements a company prints in the
//! press release it files as an 8-K / 6-K exhibit (EX-99.x).
//!
//! On earnings night the release is the only document out; the same numbers
//! arrive as XBRL weeks later with the 10-Q / 10-K. This module finds the
//! tables in a release (statement titles via `sections::statement_kind`, other
//! titled tables as subsections), gives each a section id under its exhibit
//! ("ex99.1:cash_flow"), and reads the rows as data: numbers exactly as
//! printed (parentheses = negative), the period from the column headers, the
//! unit from the caption. Comparisons are only ever between two values the
//! company printed: another column of the same release, or the same statement
//! and row label in its previous release. Row labels are never mapped to
//! us-gaap concepts. A table whose columns can't be read with confidence stays
//! out of the data, and a note says so.

use super::html::{is_page_marker, BlockKind, ParsedDoc};
use super::sections::{slug, statement_kind, title_key};
use chrono::{Datelike, NaiveDate};
use eli_finance_types::{FilingXbrl, XbrlChange, XbrlFactRow};
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

// ───────────────────────────── tables ─────────────────────────────

/// A table in a release exhibit, with its caption.
#[derive(Clone, Debug)]
pub(crate) struct ReleaseTable {
    /// "ex99.1:balance_sheet", "ex99.1:segment_information"; the exhibit id
    /// itself for untitled tables (not listed in the TOC).
    pub id: String,
    pub title: String,
    /// Primary statement kind ("income_statement", ...), None for other tables.
    pub statement: Option<&'static str>,
    /// Gets its own TOC entry (statements and titled tables).
    pub listed: bool,
    /// Block range [start, end): title / units lines plus the table rows.
    pub start: usize,
    pub end: usize,
    /// Row blocks of each table in the range (a statement continued on a
    /// second table has two).
    pub tables: Vec<Vec<usize>>,
    /// Caption text (title, units line, header label cells): the units source.
    pub caption: String,
}

/// Row blocks of each data table (two or more rows that carry figures).
fn data_tables(doc: &ParsedDoc) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_table = 0u32;
    let flush = |cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>| {
        let figures = cur.iter().filter(|&&i| row_has_value(doc, i)).count();
        if figures >= 2 {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for (i, b) in doc.blocks.iter().enumerate() {
        if b.kind != BlockKind::Row || b.layout || b.furniture {
            continue;
        }
        if b.table != cur_table {
            flush(&mut cur, &mut out);
            cur_table = b.table;
        }
        cur.push(i);
    }
    flush(&mut cur, &mut out);
    out
}

/// A figure cell outside the label column (years and quarter labels are headers).
fn is_value_cell(text: &str) -> bool {
    !is_year(text) && parse_printed_number(text).is_some()
}

fn is_year(t: &str) -> bool {
    let s = t.trim().trim_end_matches(',');
    s.len() == 4 && s.starts_with(['1', '2']) && s.chars().all(|c| c.is_ascii_digit())
}

fn row_has_value(doc: &ParsedDoc, i: usize) -> bool {
    let b = &doc.blocks[i];
    b.cells
        .iter()
        .zip(b.cols.iter())
        .any(|(c, &(k0, _))| k0 >= 1 && is_value_cell(c))
}

/// The row's label: its cell in the first column.
fn row_label(doc: &ParsedDoc, i: usize) -> Option<&str> {
    let b = &doc.blocks[i];
    match (b.cells.first(), b.cols.first()) {
        (Some(c), Some(&(0, _))) if !c.trim().is_empty() => Some(c.trim()),
        _ => None,
    }
}

fn is_units_line(t: &str) -> bool {
    let l = t.trim().to_ascii_lowercase();
    let l = l.trim_start_matches('(').trim_end_matches(')').trim();
    l == "unaudited"
        || l.starts_with("in millions")
        || l.starts_with("in thousands")
        || l.starts_with("in billions")
        || l.starts_with("$ in ")
        || l.starts_with("dollars in ")
        || l.starts_with("amounts in ")
        || l.starts_with("in u.s. dollars")
        || (t.trim().starts_with('(') && t.chars().count() <= 120 && (l.contains("millions") || l.contains("thousands") || l.contains("unaudited")))
}

/// Lines a table caption never includes: footnotes, rules, sentences.
fn ends_caption(t: &str) -> bool {
    let s = t.trim();
    let words = s.split_whitespace().count();
    s.chars().count() > 200
        || s.chars().all(|c| matches!(c, '_' | '-' | '—' | '=' | ' '))
        || (s.ends_with('.') && words > 10)
        || FOOTNOTE_LINE_RE.is_match(s)
        || s.starts_with('•')
}

static FOOTNOTE_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\(\s*[0-9a-zA-Z]{1,2}\s*\)|[0-9]{1,2}\s|\*+)\s*\S").expect("footnote regex"));

/// A line that can name a table: words, not a units / date / period line.
fn is_title_like(t: &str) -> bool {
    let s = t.trim();
    let n = s.chars().count();
    if !(4..=140).contains(&n) || is_units_line(s) || is_page_marker(s) {
        return false;
    }
    if !s.chars().any(|c| c.is_alphabetic()) {
        return false;
    }
    // A company-name line ("NVIDIA CORPORATION AND SUBSIDIARIES").
    let upper = s.to_ascii_uppercase();
    let upper = upper.trim_end_matches('.').trim_end_matches(" AND SUBSIDIARIES").trim_end_matches(" AND ITS SUBSIDIARIES");
    if [" INC", " CORPORATION", " CORP", " LTD", " LIMITED", " PLC", " N.V", " S.A", " AG", " SE", " CO"]
        .iter()
        .any(|e| upper.ends_with(e))
        && s.split_whitespace().count() <= 8
    {
        return false;
    }
    // "Three Months Ended September 30, 2026", "As of December 31, 2025":
    // nothing left once the period words are gone.
    let k = title_key(s);
    let periodish = k.split(' ').all(|w| {
            w.chars().all(|c| c.is_ascii_digit())
                || month_num(w).is_some_and(|_| w.len() <= 9)
                || QUARTER_RE.is_match(w)
                || matches!(
                    w,
                    "three" | "six" | "nine" | "twelve" | "months" | "month" | "weeks" | "ended" | "ending" | "as" | "of" | "at"
                        | "for" | "the" | "quarter" | "quarters" | "year" | "years" | "to" | "date" | "fiscal" | "and" | "unaudited"
                )
        });
    !(periodish || (s.ends_with(':') && n > 60))
}

/// A title without its units parenthetical, for the section id:
/// "Segment results (In millions)" -> "Segment results".
fn id_title(title: &str) -> String {
    static UNITS_PAREN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\s*\([^)]*(?:million|thousand|billion|unaudited|except)[^)]*\)").expect("units paren regex")
    });
    UNITS_PAREN.replace_all(title, "").trim().to_string()
}

/// Paragraph lines right above a table (nearest last), down to `floor`.
fn caption_above(doc: &ParsedDoc, first: usize, floor: usize) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut j = first;
    let mut seen = 0;
    while j > floor && seen < 6 {
        j -= 1;
        let b = &doc.blocks[j];
        if b.furniture || b.kind == BlockKind::PageBreak {
            continue;
        }
        if b.kind == BlockKind::Row && !b.layout {
            break;
        }
        let t = b.heading_text();
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        // One sentence introducing the table ("The following table
        // summarizes ... (in millions, except per share amounts):") belongs
        // to it; it carries the units, never the title.
        let intro = seen == 0
            && t.chars().count() <= 400
            && (t.ends_with(':') || t.to_ascii_lowercase().contains("following table"));
        if ends_caption(t) && !intro {
            break;
        }
        out.push(j);
        seen += 1;
    }
    out.reverse();
    out
}

/// A table with no column headers at all (the second page of a statement).
fn headerless(doc: &ParsedDoc, rows: &[usize]) -> bool {
    let nhead = header_rows(doc, rows);
    rows[..nhead].iter().all(|&i| doc.blocks[i].cols.iter().all(|c| c.0 == 0))
}

/// Aligned columns that hold figures, left to right.
fn value_columns(doc: &ParsedDoc, rows: &[usize]) -> Vec<u16> {
    let mut ks: Vec<u16> = rows
        .iter()
        .flat_map(|&i| {
            let b = &doc.blocks[i];
            b.cells
                .iter()
                .zip(b.cols.iter())
                .filter(|(c, &(k0, k1))| k0 >= 1 && k0 == k1 && is_value_cell(c))
                .map(|(_, &(k0, _))| k0)
                .collect::<Vec<_>>()
        })
        .collect();
    ks.sort_unstable();
    ks.dedup();
    ks
}

/// Rows before the first figure: titles, units and column headers.
fn header_rows(doc: &ParsedDoc, rows: &[usize]) -> usize {
    rows.iter().position(|&i| row_has_value(doc, i)).unwrap_or(rows.len())
}

/// Find the tables of a release exhibit. `ex_id` is "ex99.1".
pub(crate) fn detect_release_tables(doc: &ParsedDoc, ex_id: &str) -> Vec<ReleaseTable> {
    let raw = data_tables(doc);
    let mut out: Vec<ReleaseTable> = Vec::new();
    let mut seen_kinds: Vec<&'static str> = Vec::new();
    let mut used_ids: HashSet<String> = HashSet::new();
    let mut floor = 0usize;
    let mut k = 0usize;
    while k < raw.len() {
        let rows = &raw[k];
        let first = rows[0];
        let above = caption_above(doc, first, floor);
        let nhead = header_rows(doc, rows);
        // Single-cell rows above the column headers: an in-table title /
        // units line (a label row under the headers is a group heading).
        let in_table: Vec<usize> = rows[..nhead]
            .iter()
            .copied()
            .take_while(|&i| doc.blocks[i].cols.iter().all(|c| c.0 == 0))
            .filter(|&i| doc.blocks[i].cells.len() == 1 && row_label(doc, i).is_some())
            .collect();
        let mut caption = String::new();
        for &i in above.iter().chain(rows[..nhead].iter()) {
            let b = &doc.blocks[i];
            let t = if b.kind == BlockKind::Row { b.cells.first().cloned().unwrap_or_default() } else { b.text.clone() };
            if b.kind == BlockKind::Row && b.cols.first().is_some_and(|c| c.0 != 0) {
                continue;
            }
            caption.push_str(t.trim());
            caption.push('\n');
        }
        let text_of = |i: usize| doc.blocks[i].heading_text().trim().to_string();
        let statement_title = above
            .iter()
            .chain(in_table.iter())
            .copied()
            .find_map(|i| statement_kind(&title_key(&text_of(i))).map(|kind| (i, kind)));
        let (title_block, title, kind) = match statement_title {
            Some((i, kind)) => (Some(i), text_of(i), Some(kind)),
            None => match above.iter().rev().chain(in_table.iter()).copied().find(|&i| is_title_like(&text_of(i))) {
                Some(i) => (Some(i), text_of(i), None),
                None => (None, String::new(), None),
            },
        };
        // Start at the title when it sits above the table (a company-name
        // line over it belongs to no one table).
        let start = match title_block {
            Some(i) if i < first => i,
            _ => first,
        };
        let mut tables = vec![rows.clone()];
        let mut end = rows.last().copied().unwrap_or(first) + 1;
        // A statement continued on the next table: nothing but page furniture,
        // units lines, "(continued)" or the same title in between, and the
        // same columns (or none) heading it.
        if let Some(kind) = kind {
            while let Some(next) = raw.get(k + 1) {
                let gap_ok = (end..next[0]).all(|j| {
                    let b = &doc.blocks[j];
                    if b.furniture || b.kind == BlockKind::PageBreak {
                        return true;
                    }
                    let t = b.heading_text();
                    let t = t.trim();
                    let key = title_key(t);
                    t.is_empty()
                        || is_units_line(t)
                        || key.contains("continued")
                        || statement_kind(&key) == Some(kind)
                });
                let same_cols = {
                    let a = table_columns(doc, &tables[0], Some(kind)).map(|c| c.periods());
                    headerless(doc, next) || matches!((a, table_columns(doc, next, Some(kind)).map(|c| c.periods())), (Ok(x), Ok(y)) if x == y)
                };
                if !(gap_ok && same_cols) {
                    break;
                }
                k += 1;
                tables.push(next.clone());
                end = next.last().copied().unwrap_or(end) + 1;
            }
        }
        floor = end;
        let first_of_kind = kind.is_some_and(|kd| !seen_kinds.contains(&kd));
        let (id, listed, statement) = if first_of_kind {
            let kd = kind.unwrap_or_default();
            seen_kinds.push(kd);
            (format!("{ex_id}:{kd}"), true, kind)
        } else if !title.is_empty() && rows.len() >= 3 {
            let base = format!("{ex_id}:{}", slug(&id_title(&title)));
            let mut id = base.clone();
            let mut n = 2;
            while used_ids.contains(&id) {
                id = format!("{base}_{n}");
                n += 1;
            }
            (id, true, None)
        } else {
            (ex_id.to_string(), false, None)
        };
        if listed {
            used_ids.insert(id.clone());
        }
        out.push(ReleaseTable {
            id,
            title: if title.is_empty() { "Untitled table".to_string() } else { title },
            statement,
            listed,
            start,
            end,
            tables,
            caption,
        });
        k += 1;
    }
    out
}

// ───────────────────────────── periods ─────────────────────────────

/// The period a column's values cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Period {
    /// A balance at a date.
    At(NaiveDate),
    /// A flow over `months` ending on a date; 0 = year to date with the span
    /// not printed.
    Span(u32, NaiveDate),
    /// A fiscal quarter printed only as a label ("Q3 2026").
    Quarter(i32, u8),
}

impl Period {
    pub(crate) fn label(&self) -> String {
        match self {
            Period::At(d) => format!("at {d}"),
            Period::Span(0, d) => format!("YTD ended {d}"),
            Period::Span(m, d) => format!("{m}M ended {d}"),
            Period::Quarter(y, q) => format!("Q{q} {y}"),
        }
    }

    /// The comparison's own period, as `XbrlChange.period` shows it.
    fn short(&self) -> String {
        match self {
            Period::At(d) | Period::Span(_, d) => d.to_string(),
            Period::Quarter(y, q) => format!("Q{q} {y}"),
        }
    }

    fn end(&self) -> Option<NaiveDate> {
        match self {
            Period::At(d) | Period::Span(_, d) => Some(*d),
            Period::Quarter(..) => None,
        }
    }
}

/// What a column header says: a span, a date, a quarter label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeaderPeriod {
    /// Some(0) = year to date, span not printed; None = no span (a balance date).
    pub months: Option<u32>,
    pub date: Option<NaiveDate>,
    pub quarter: Option<(i32, u8)>,
}

static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+(\d{1,2})(?:st|nd|rd|th)?\s*,?\s*(\d{4})\b")
        .expect("date regex")
});
static NUM_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{1,2})/(\d{1,2})/(\d{4})\b").expect("numeric date regex"));
static QUARTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:q([1-4])\s*(?:fy\s*)?'?(\d{4}|\d{2})|([1-4])q\s*(?:fy\s*)?'?(\d{4}|\d{2}))\b").expect("quarter regex")
});
static SPAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(three|six|nine|twelve|thirteen|fourteen|twenty[\s-]six|twenty[\s-]seven|thirty[\s-]nine|forty|fifty[\s-]two|fifty[\s-]three|\d{1,2})[\s-]+(months?|weeks?)\b",
    )
    .expect("span regex")
});

fn span_words(n: &str, unit: &str) -> Option<u32> {
    let n = n.to_ascii_lowercase().replace([' ', '-'], "");
    let v: u32 = match n.as_str() {
        "three" => 3,
        "six" => 6,
        "nine" => 9,
        "twelve" => 12,
        "thirteen" => 13,
        "fourteen" => 14,
        "twentysix" => 26,
        "twentyseven" => 27,
        "thirtynine" => 39,
        "forty" => 40,
        "fiftytwo" => 52,
        "fiftythree" => 53,
        d => d.parse().ok()?,
    };
    if unit.to_ascii_lowercase().starts_with("week") {
        // 13/14-week quarters, 26/27, 39/40, 52/53-week years.
        match v {
            13 | 14 => Some(3),
            26 | 27 => Some(6),
            39 | 40 => Some(9),
            52 | 53 => Some(12),
            _ => None,
        }
    } else {
        matches!(v, 1..=12).then_some(v)
    }
}

fn month_num(m: &str) -> Option<u32> {
    let m = m.to_ascii_lowercase();
    let i = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|x| m.starts_with(x))?;
    Some(i as u32 + 1)
}

/// Read a column header ("Three Months Ended September 27, 2026",
/// "Twelve Months Ended / September 26, / 2026", "As of December 31, 2025",
/// "Q3 2026", "13 Weeks Ended Jul 26, 2026"). None when it names no date or
/// quarter, or names two different ones.
pub(crate) fn header_period(text: &str) -> Option<HeaderPeriod> {
    let lower = text.to_ascii_lowercase();
    let mut dates: Vec<NaiveDate> = Vec::new();
    for c in DATE_RE.captures_iter(text) {
        let m = month_num(&c[1])?;
        let d: u32 = c[2].parse().ok()?;
        let y: i32 = c[3].parse().ok()?;
        dates.push(NaiveDate::from_ymd_opt(y, m, d)?);
    }
    for c in NUM_DATE_RE.captures_iter(text) {
        let (m, d, y) = (c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?);
        dates.push(NaiveDate::from_ymd_opt(y, m, d)?);
    }
    dates.dedup();
    if dates.len() > 1 {
        return None;
    }
    let mut quarters: Vec<(i32, u8)> = Vec::new();
    for c in QUARTER_RE.captures_iter(text) {
        let (q, y) = match (c.get(1), c.get(2), c.get(3), c.get(4)) {
            (Some(q), Some(y), _, _) | (_, _, Some(q), Some(y)) => (q.as_str(), y.as_str()),
            _ => continue,
        };
        let q: u8 = q.parse().ok()?;
        let mut y: i32 = y.parse().ok()?;
        if y < 100 {
            y += 2000;
        }
        quarters.push((y, q));
    }
    quarters.dedup();
    if quarters.len() > 1 {
        return None;
    }
    let mut spans: Vec<u32> = SPAN_RE
        .captures_iter(text)
        .filter_map(|c| span_words(&c[1], &c[2]))
        .collect();
    if lower.contains("year to date") || lower.contains("year-to-date") || lower.split(|c: char| !c.is_alphanumeric()).any(|w| w == "ytd") {
        spans.push(0);
    } else if spans.is_empty() {
        if lower.contains("quarter") {
            spans.push(3);
        } else if lower.contains("year ended") || lower.contains("years ended") || lower.contains("fiscal year") || lower.contains("full year") {
            spans.push(12);
        }
    }
    spans.sort_unstable();
    spans.dedup();
    if spans.len() > 1 {
        return None;
    }
    let date = dates.first().copied();
    let quarter = if date.is_none() { quarters.first().copied() } else { None };
    if date.is_none() && quarter.is_none() {
        return None;
    }
    Some(HeaderPeriod { months: spans.first().copied(), date, quarter })
}

/// Column headers that are not reported values: changes, ratios, non-GAAP
/// and constant-currency restatements, outlooks.
fn is_derived_column(text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    let words: Vec<&str> = l.split(|c: char| !c.is_alphanumeric() && c != '-').filter(|w| !w.is_empty()).collect();
    let has_word = |w: &str| words.contains(&w);
    l.contains('%')
        || l.contains("change")
        || l.contains("variance")
        || l.contains("growth")
        || l.contains("outlook")
        || l.contains("guidance")
        || l.contains("forecast")
        || l.contains("constant currency")
        || l.contains("non-gaap")
        || l.contains("non gaap")
        || l.contains("adjusted")
        || l.contains("trailing")
        || l.contains("pro forma")
        || l.contains("impact")
        || l.contains("effect")
        || l.contains("increase")
        || l.contains("decrease")
        || has_word("vs")
        || has_word("fx")
        || has_word("ttm")
        || has_word("bps")
}

struct Columns {
    /// Aligned column index -> its period.
    by_col: HashMap<u16, Period>,
}

impl Columns {
    fn periods(&self) -> Vec<Period> {
        let mut v: Vec<Period> = self.by_col.values().copied().collect();
        v.sort();
        v
    }
}

/// The table's value columns. Err names why the columns can't be trusted.
fn table_columns(doc: &ParsedDoc, rows: &[usize], kind: Option<&str>) -> Result<Columns, &'static str> {
    let nhead = header_rows(doc, rows);
    let ncols = rows
        .iter()
        .flat_map(|&i| doc.blocks[i].cols.iter().map(|c| c.1))
        .max()
        .unwrap_or(0);
    let mut by_col: HashMap<u16, Period> = HashMap::new();
    let mut seen: HashSet<Period> = HashSet::new();
    let mut undated_values = false;
    for k in 1..=ncols {
        // Header text over this column, top to bottom (cells spanning it included).
        let mut head = String::new();
        for &i in &rows[..nhead] {
            let b = &doc.blocks[i];
            for (c, &(k0, k1)) in b.cells.iter().zip(b.cols.iter()) {
                if k0 >= 1 && k0 <= k && k <= k1 {
                    head.push_str(c);
                    head.push(' ');
                }
            }
        }
        let has_values = rows[nhead..].iter().any(|&i| {
            let b = &doc.blocks[i];
            b.cells.iter().zip(b.cols.iter()).any(|(c, &(k0, k1))| k0 <= k && k <= k1 && k0 >= 1 && is_value_cell(c))
        });
        if !has_values || is_derived_column(&head) {
            continue;
        }
        let Some(hp) = header_period(&head) else {
            undated_values = true;
            continue;
        };
        let period = match (hp.date, hp.quarter, hp.months) {
            (Some(d), _, Some(m)) => Period::Span(m, d),
            (Some(d), _, None) if kind == Some("balance_sheet") => Period::At(d),
            // A date with no span outside a balance sheet: balance or flow is
            // not printed.
            (Some(_), _, None) => {
                undated_values = true;
                continue;
            }
            (None, Some((y, q)), Some(0) | None) | (None, Some((y, q)), Some(3)) => Period::Quarter(y, q),
            _ => {
                undated_values = true;
                continue;
            }
        };
        if !seen.insert(period) {
            return Err("two columns carry the same period");
        }
        by_col.insert(k, period);
    }
    if by_col.is_empty() {
        return Err(if undated_values {
            "its column headers name no span (three months, year ended) or no date"
        } else {
            "no column headers with a date or quarter"
        });
    }
    let dated = by_col.values().any(|p| p.end().is_some());
    let labelled = by_col.values().any(|p| p.end().is_none());
    if dated && labelled {
        return Err("columns mix dates and quarter labels");
    }
    Ok(Columns { by_col })
}

// ───────────────────────────── numbers ─────────────────────────────

/// A figure as printed in a table cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Printed {
    pub value: f64,
    pub percent: bool,
    pub currency: Option<&'static str>,
}

const CURRENCY_PREFIXES: &[(&str, Option<&str>)] = &[
    ("US$", Some("USD")),
    ("NT$", Some("TWD")),
    ("HK$", Some("HKD")),
    ("C$", Some("CAD")),
    ("A$", Some("AUD")),
    ("R$", Some("BRL")),
    ("$", Some("USD")),
    ("€", Some("EUR")),
    ("£", Some("GBP")),
    ("₹", Some("INR")),
    ("RMB", Some("CNY")),
    ("CHF", Some("CHF")),
    ("¥", None),
];

/// "$82,904" -> 82904, "(321)" -> -321, "$(290)" -> -290, "(3)%" -> -3 %,
/// "—" -> 0. None for anything else (text, "n/a", ranges, footnoted figures).
pub(crate) fn parse_printed_number(cell: &str) -> Option<Printed> {
    let mut s: String = cell.trim().chars().filter(|c| !c.is_whitespace()).collect();
    let percent = s.contains('%');
    s = s.replace('%', "");
    let mut currency: Option<&'static str> = None;
    let mut negative = false;
    let strip_currency = |s: &mut String, currency: &mut Option<&'static str>| {
        for (p, code) in CURRENCY_PREFIXES {
            if let Some(rest) = s.strip_prefix(p) {
                *s = rest.to_string();
                if code.is_some() {
                    *currency = *code;
                }
                return;
            }
        }
    };
    strip_currency(&mut s, &mut currency);
    if s.starts_with('(') && s.ends_with(')') && s.len() >= 2 {
        negative = true;
        s = s[1..s.len() - 1].to_string();
        strip_currency(&mut s, &mut currency);
    }
    // "–1,738": some filers print the minus as an en dash.
    if let Some(rest) = s.strip_prefix(['-', '−', '–']) {
        if !rest.is_empty() {
            negative = !negative;
            s = rest.to_string();
        }
    }
    if matches!(s.as_str(), "—" | "–" | "-" | "−") {
        return Some(Printed { value: 0.0, percent, currency });
    }
    let ok = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.')
        && s.chars().filter(|&c| c == '.').count() <= 1
        && s.chars().any(|c| c.is_ascii_digit())
        && !s.starts_with(',')
        && !s.ends_with(',')
        && !s.ends_with('.');
    if !ok {
        return None;
    }
    // Thousands separators sit every three digits.
    let int_part = s.split('.').next().unwrap_or("");
    if int_part.contains(',') && int_part.split(',').skip(1).any(|g| g.len() != 3) {
        return None;
    }
    let v: f64 = s.replace(',', "").parse().ok()?;
    Some(Printed { value: if negative { -v } else { v }, percent, currency })
}

static TRAIL_MARK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\s*(?:\(\s*(?:\d{1,2}|[A-Za-z])\s*\)|\*+|†|‡|[¹²³⁴⁵⁶⁷⁸⁹⁰]+))+\s*:?\s*$").expect("footnote marker regex")
});
static LEAD_MARK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\(\s*\d{1,2}\s*\)\s*").expect("leading marker regex"));

/// The printed label without footnote markers or a trailing colon:
/// "Total net sales (1)" -> "Total net sales".
pub(crate) fn clean_label(label: &str) -> String {
    let s = LEAD_MARK_RE.replace(label.trim(), "");
    let s = TRAIL_MARK_RE.replace(&s, "");
    s.trim().trim_end_matches(':').trim().to_string()
}

/// Matching key for a label across releases: case, whitespace, punctuation and
/// footnote markers folded; nothing fuzzier.
pub(crate) fn normalize_label(label: &str) -> String {
    title_key(&clean_label(label))
}

// ───────────────────────────── units ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShareScale {
    /// Share counts in the caption's main scale.
    Same,
    Exp(i32),
    /// The caption excepts share counts without naming their scale.
    Unstated,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CaptionUnits {
    /// 3 / 6 / 9 for thousands / millions / billions.
    money: Option<i32>,
    shares: ShareScale,
    currency: Option<&'static str>,
}

fn scale_exp(word: &str) -> Option<i32> {
    match word {
        "thousands" | "thousand" | "000s" | "000" => Some(3),
        "millions" | "million" => Some(6),
        "billions" | "billion" => Some(9),
        _ => None,
    }
}

static SCALE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:\bin\s+(?:u\.?s\.?\s+)?(?:\$\s*|dollars\s+|usd\s+)?|(?:€|\$|£|\b(?:eur|usd|rmb)\b)\s*)(thousands?|millions?|billions?)\b|\(\s*\$?\s*(000s|000's|in 000s)\s*\)|\(\$000\)",
    )
        .expect("scale regex")
});
static SHARES_SCALE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)shares?[^;)]*?(?:reflected|presented|stated|shown|expressed|are)\s+in\s+(thousands|millions|billions)").expect("share scale regex")
});
static EXCEPT_SHARES_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)except\b[^;)]*?\bshares?\b").expect("except shares regex")
});

/// Scale and currency from a table's caption: "(In millions, except number of
/// shares, which are reflected in thousands, and per-share amounts)".
pub(crate) fn caption_units(caption: &str) -> CaptionUnits {
    // "(000s)" / "($000)" are thousands.
    let money = SCALE_RE.captures(caption).map(|c| {
        c.get(1)
            .and_then(|m| {
                let w = m.as_str().to_ascii_lowercase();
                scale_exp(w.trim_end_matches('s')).or_else(|| scale_exp(&w))
            })
            .unwrap_or(3)
    });
    // "except per share amounts" excepts no share counts; "except share and
    // per share data" does.
    let without_per_share = caption.to_ascii_lowercase().replace("per share", "").replace("per-share", "");
    let shares = if let Some(c) = SHARES_SCALE_RE.captures(&without_per_share) {
        scale_exp(&c[1].to_ascii_lowercase()).map(ShareScale::Exp).unwrap_or(ShareScale::Unstated)
    } else if EXCEPT_SHARES_RE.is_match(&without_per_share) {
        ShareScale::Unstated
    } else {
        ShareScale::Same
    };
    let lower = caption.to_ascii_lowercase();
    let currency = if lower.contains("u.s. dollars") || lower.contains("us$") || lower.contains('$') {
        Some("USD")
    } else if lower.contains("euro") || caption.contains('€') {
        Some("EUR")
    } else if lower.contains("rmb") || lower.contains("renminbi") {
        Some("CNY")
    } else {
        None
    };
    CaptionUnits { money, shares, currency }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnitKind {
    Money,
    PerShare,
    Shares,
    Percent,
    /// A count the caption excepts from its scale (employees, stores).
    Count,
}

fn is_share_count(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    if s.contains("par value")
        || s.contains('$')
        || s.contains("authorized")
        || (s.contains("per share") && !s.contains("shares used") && !s.contains("weighted"))
    {
        return false;
    }
    (s.contains("weighted") && s.contains("share"))
        || s.contains("shares used")
        || s.contains("shares outstanding")
        || s.contains("average shares")
        || s.contains("number of shares")
        || s.contains("share count")
        || s.starts_with("shares ")
        || s.contains("diluted shares")
        || s.contains("basic shares")
}

fn is_per_share(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("per share")
        || s.contains("per-share")
        || s.contains("per diluted share")
        || s.contains("per basic share")
        || s.contains("per common share")
        || s.contains("per ads")
        || s.contains("per american depositary")
        || s.contains("per ordinary share")
        || s.split(|c: char| !c.is_alphanumeric()).any(|w| w == "eps")
}

const COUNT_WORDS: &[&str] = &["employee", "headcount", "stores", "customers", "subscribers", "users", "locations", "members"];

/// Row-level scale in the label: "Revenue ($B)", "Backlog (in millions)".
fn label_scale(label: &str) -> Option<i32> {
    let l = label.to_ascii_lowercase().replace(' ', "");
    if l.contains("($b)") || l.contains("($billions)") || l.contains("(inbillions)") || l.contains("($inbillions)") {
        Some(9)
    } else if l.contains("($m)") || l.contains("($millions)") || l.contains("(inmillions)") || l.contains("($inmillions)") {
        Some(6)
    } else if l.contains("($k)") || l.contains("(inthousands)") || l.contains("($inthousands)") {
        Some(3)
    } else {
        None
    }
}

fn scale_word(e: i32) -> &'static str {
    match e {
        3 => "thousands",
        6 => "millions",
        9 => "billions",
        _ => "",
    }
}

/// Unit label and the power of ten the values are shown in (None when the
/// caption doesn't say).
fn row_unit(kind: UnitKind, units: &CaptionUnits, label: &str, cell_currency: Option<&'static str>) -> (String, Option<i32>) {
    let cur = cell_currency.or(units.currency);
    match kind {
        UnitKind::Percent => ("%".to_string(), Some(0)),
        UnitKind::Count => ("number as printed".to_string(), Some(0)),
        UnitKind::PerShare => (
            match cur {
                Some(c) => format!("{c} per share"),
                None => "per share".to_string(),
            },
            Some(0),
        ),
        UnitKind::Shares => match (units.shares, units.money) {
            (ShareScale::Exp(e), _) | (ShareScale::Same, Some(e)) => (format!("shares {}", scale_word(e)), Some(e)),
            _ => ("shares (scale not printed)".to_string(), None),
        },
        UnitKind::Money => {
            let scale = label_scale(label).or(units.money);
            let unit = match (cur, scale) {
                (Some(c), Some(e)) => format!("{c} {}", scale_word(e)),
                (Some(c), None) => format!("{c} (scale not printed)"),
                (None, Some(e)) => format!("{} (currency not printed)", scale_word(e)),
                (None, None) => "as printed (no unit in caption)".to_string(),
            };
            (unit, scale)
        }
    }
}

// ───────────────────────────── rows ─────────────────────────────

#[derive(Clone, Debug)]
pub(crate) struct ReleaseRow {
    /// Section id of the table ("ex99.1:balance_sheet").
    pub section: String,
    pub statement: Option<&'static str>,
    /// Matches the same table across releases: the statement kind, else the
    /// table's title key.
    pub table_key: String,
    /// The label as printed (footnote markers dropped), with its group heading
    /// in brackets when the bare label repeats in the table or is generic.
    pub label: String,
    pub label_key: String,
    /// The heading row above the label ("Current assets"), as printed.
    pub group: String,
    pub group_key: String,
    pub unit: String,
    pub unit_kind: UnitKind,
    pub scale: Option<i32>,
    pub values: Vec<(Period, f64)>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ReleaseData {
    /// "EX-99.1"
    pub exhibit: String,
    pub period_end: Option<NaiveDate>,
    pub rows: Vec<ReleaseRow>,
    /// Figures read (all columns).
    pub values: usize,
    pub notes: Vec<String>,
}

/// A label that carries on the line above ("provided by operating
/// activities:"): its first word is all lowercase ("iPhone" is not).
fn continues_line(label: &str) -> bool {
    label
        .split_whitespace()
        .next()
        .is_some_and(|w| w.chars().all(|c| !c.is_alphabetic() || c.is_lowercase()) && w.chars().next().is_some_and(|c| c.is_lowercase()))
}

fn is_generic(label_key: &str) -> bool {
    label_key.chars().count() < 3
        || matches!(
            label_key,
            "total" | "other" | "net" | "basic" | "diluted" | "other net" | "subtotal" | "all other" | "consolidated" | "corporate"
                | "eliminations" | "revenue" | "revenues" | "net sales" | "operating income" | "operating expenses" | "cost of revenue"
        )
}

/// Read every table's rows as figures by period. `fye` is the filer's fiscal
/// year-end (month, day) from SEC submissions, used to give year-to-date
/// columns their span.
pub(crate) fn extract_release(doc: &ParsedDoc, tables: &[ReleaseTable], exhibit: &str, fye: Option<(u32, u32)>) -> ReleaseData {
    let mut data = ReleaseData {
        exhibit: exhibit.to_string(),
        ..Default::default()
    };
    let mut unread: Vec<String> = Vec::new();
    for t in tables {
        let units = caption_units(&t.caption);
        let table_key = t.statement.map(str::to_string).unwrap_or_else(|| title_key(&t.title));
        let mut rows: Vec<ReleaseRow> = Vec::new();
        for (ti, trows) in t.tables.iter().enumerate() {
            // A statement's second table without headers takes the first
            // table's columns when both hold the same number of figure columns
            // and every figure column of the first is a dated one.
            let continued = || -> Option<Columns> {
                if ti == 0 || !headerless(doc, trows) {
                    return None;
                }
                let first = table_columns(doc, &t.tables[0], t.statement).ok()?;
                let mut keys: Vec<u16> = first.by_col.keys().copied().collect();
                keys.sort_unstable();
                if value_columns(doc, &t.tables[0]) != keys {
                    return None;
                }
                let mine = value_columns(doc, trows);
                if mine.len() != keys.len() {
                    return None;
                }
                Some(Columns {
                    by_col: mine.into_iter().zip(keys.iter().map(|k| first.by_col[k])).collect(),
                })
            };
            let cols = match table_columns(doc, trows, t.statement).or_else(|e| continued().ok_or(e)) {
                Ok(c) => c,
                Err(why) => {
                    if t.statement.is_some() {
                        let part = if t.tables.len() > 1 { format!(" (part {})", ti + 1) } else { String::new() };
                        data.notes.push(format!("Not read as data: {} '{}'{part}: {why}. Its text is at section={}.", t.id, t.title, t.id));
                    } else if t.listed {
                        unread.push(t.id.clone());
                    }
                    continue;
                }
            };
            let nhead = header_rows(doc, trows);
            let table_currency = trows.iter().find_map(|&i| doc.blocks[i].cells.iter().find_map(|c| parse_printed_number(c).and_then(|p| p.currency)));
            let mut group: Option<String> = None;
            let mut carry: Option<String> = None;
            let mut prev_percent = false;
            for (n, &i) in trows.iter().enumerate() {
                let b = &doc.blocks[i];
                // Column-header rows clear any heading above them (an in-table
                // title); a label-only row after them ("Net sales:") heads
                // the first figures.
                if n < nhead && b.cols.iter().any(|c| c.0 >= 1) {
                    group = None;
                    carry = None;
                    continue;
                }
                let label = row_label(doc, i).map(str::to_string);
                let mut vals: Vec<(Period, f64)> = Vec::new();
                let mut percent = false;
                let mut currency: Option<&'static str> = None;
                for (c, &(k0, k1)) in b.cells.iter().zip(b.cols.iter()) {
                    if k0 == 0 {
                        continue;
                    }
                    let Some(p) = parse_printed_number(c) else { continue };
                    let hits: Vec<Period> = (k0..=k1).filter_map(|k| cols.by_col.get(&k).copied()).collect();
                    if hits.len() != 1 || is_year(c) {
                        continue;
                    }
                    percent |= p.percent;
                    currency = currency.or(p.currency);
                    vals.push((hits[0], p.value));
                }
                let Some(label) = label else {
                    continue;
                };
                if vals.is_empty() {
                    if row_has_value(doc, i) {
                        continue; // figures only in non-value columns
                    }
                    // A heading row; a label that runs on into the next row
                    // ("Adjustments to reconcile net income to net cash /
                    // provided by operating activities:") joins it.
                    let next_lower = !label.trim_end().ends_with(':')
                        && trows.get(n + 1).and_then(|&j| row_label(doc, j)).is_some_and(continues_line);
                    let joined = match carry.take() {
                        Some(c) => format!("{c} {label}"),
                        None => label.clone(),
                    };
                    if next_lower {
                        carry = Some(joined);
                    } else {
                        group = Some(clean_label(&joined));
                        prev_percent = false;
                    }
                    continue;
                }
                let printed = match carry.take() {
                    Some(c) if continues_line(&label) => format!("{c} {label}"),
                    _ => label.clone(),
                };
                let display = clean_label(&printed);
                let label_key = normalize_label(&printed);
                let group_text = group.clone().unwrap_or_default();
                // Percent rows printed with the sign only on the first line,
                // under a heading that says so ("Y/Y net sales growth: 11% /
                // 14 / 20").
                let pct_group = {
                    let g = group_text.to_ascii_lowercase();
                    ["growth", "%", "mix", "margin", "percent", "rate"].iter().any(|w| g.contains(w))
                };
                if !percent && currency.is_none() && prev_percent && pct_group {
                    percent = true;
                }
                prev_percent = percent;
                let lower = display.to_ascii_lowercase();
                let unit_kind = if percent {
                    UnitKind::Percent
                } else if t.statement != Some("balance_sheet") && is_share_count(&display) {
                    UnitKind::Shares
                } else if is_per_share(&display) {
                    UnitKind::PerShare
                } else if t.statement != Some("balance_sheet") && is_share_count(&group_text) {
                    UnitKind::Shares
                } else if is_per_share(&group_text) {
                    UnitKind::PerShare
                } else if t.statement.is_none() && COUNT_WORDS.iter().any(|w| lower.contains(w)) {
                    UnitKind::Count
                } else {
                    UnitKind::Money
                };
                let (unit, scale) = row_unit(unit_kind, &units, &display, currency.or(table_currency));
                data.values += vals.len();
                // Year-to-date columns get their span from the fiscal year-end.
                if let Some(fy) = fye {
                    for (p, _) in vals.iter_mut() {
                        if let Period::Span(0, d) = *p {
                            if let Some(start) = fye_before(fy, d) {
                                let m = ((d - start).num_days() as f64 / 30.44).round() as u32;
                                if (1..=12).contains(&m) {
                                    *p = Period::Span(m, d);
                                }
                            }
                        }
                    }
                }
                rows.push(ReleaseRow {
                    section: t.id.clone(),
                    statement: t.statement,
                    table_key: table_key.clone(),
                    label: display,
                    label_key,
                    group_key: title_key(&group_text),
                    group: group_text.clone(),
                    unit,
                    unit_kind,
                    scale,
                    values: vals,
                });
                // "Total current assets" closes "Current assets:".
                if lower.starts_with("total") {
                    group = None;
                }
            }
        }
        // Disambiguate repeated or generic labels with their group heading.
        let mut counts: HashMap<String, usize> = HashMap::new();
        for r in &rows {
            *counts.entry(r.label_key.clone()).or_default() += 1;
        }
        // Outside the statements the heading always says what a row is
        // ("Technology and infrastructure" under "Stock-based Compensation").
        for r in rows.iter_mut() {
            let ambiguous = counts.get(&r.label_key).copied().unwrap_or(0) > 1 || is_generic(&r.label_key) || r.statement.is_none();
            if ambiguous && !r.group.is_empty() {
                r.label = format!("{} [{}]", r.label, r.group);
            }
        }
        data.rows.extend(rows);
    }
    if !unread.is_empty() {
        unread.dedup();
        data.notes.push(format!(
            "Other tables not read as data (columns without a printed span and date, or repeating one period, e.g. change and non-GAAP columns): {}. Their text is at section=<id>.",
            unread.join(", ")
        ));
    }
    // The release's period: the latest date the primary statements print.
    data.period_end = data
        .rows
        .iter()
        .filter(|r| r.statement.is_some())
        .flat_map(|r| r.values.iter().filter_map(|(p, _)| p.end()))
        .max();
    data
}

/// The fiscal year-end on or before `d` (more than 20 days before it), for a
/// month/day fiscal year-end; 52/53-week years land within days of it.
fn fye_before((m, day): (u32, u32), d: NaiveDate) -> Option<NaiveDate> {
    for y in [d.year(), d.year() - 1, d.year() - 2] {
        let f = NaiveDate::from_ymd_opt(y, m, day).or_else(|| NaiveDate::from_ymd_opt(y, m, 28))?;
        if (d - f).num_days() > 20 {
            return Some(f);
        }
    }
    None
}

// ───────────────────────────── as data ─────────────────────────────

/// The previous earnings release, for quarter-over-quarter values.
pub(crate) struct PriorRelease<'a> {
    pub data: &'a ReleaseData,
    /// "8-K 2026-07-31"
    pub label: String,
}

pub(crate) struct ReleaseOptions {
    pub concepts: Vec<String>,
    pub min_change_pct: f64,
    /// Fiscal year-end (month, day), from SEC submissions.
    pub fye: Option<(u32, u32)>,
    /// Why there is no prior release to compare with, when there isn't.
    pub prior_note: Option<String>,
}

fn round_to(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

fn near(a: NaiveDate, b: NaiveDate, tol: i64) -> bool {
    (a - b).num_days().abs() <= tol
}

fn year_before(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year() - 1, d.month(), d.day())
        .or_else(|| NaiveDate::from_ymd_opt(d.year() - 1, d.month(), 28))
        .unwrap_or(d)
}

/// The value printed for the same span, a year / a quarter earlier.
fn find_value(values: &[(Period, f64)], cur: Period, years: bool) -> Option<(Period, f64)> {
    values.iter().copied().find(|(p, _)| match (cur, *p) {
        (Period::At(d), Period::At(o)) => {
            if years {
                near(o, year_before(d), 10)
            } else {
                near(o, d - chrono::Duration::days(91), 12)
            }
        }
        (Period::Span(m, d), Period::Span(mo, o)) if m == mo => {
            if years {
                near(o, year_before(d), 10)
            } else {
                m == 3 && near(o, d - chrono::Duration::days(91), 12)
            }
        }
        (Period::Quarter(y, q), Period::Quarter(yo, qo)) => {
            if years {
                yo == y - 1 && qo == q
            } else if q == 1 {
                yo == y - 1 && qo == 4
            } else {
                yo == y && qo == q - 1
            }
        }
        _ => false,
    })
}

/// Build the numbers-as-data view of a release in the FilingXbrl shape.
pub(crate) fn build_release_xbrl(cur: &ReleaseData, prior: Option<&PriorRelease<'_>>, opts: &ReleaseOptions, form: &str) -> FilingXbrl {
    let mut notes: Vec<String> = Vec::new();
    let source = format!("press release {} tables", cur.exhibit);
    let p_end = cur.period_end;

    // Prior fiscal year-end: from submissions, else from a year-to-date span
    // printed for this period.
    let fye_date: Option<NaiveDate> = p_end.and_then(|end| {
        opts.fye.and_then(|f| fye_before(f, end)).or_else(|| {
            cur.rows
                .iter()
                .flat_map(|r| r.values.iter())
                .find_map(|(p, _)| match *p {
                    Period::Span(m @ (6 | 9 | 12), d) if near(d, end, 3) => Some(d - chrono::Duration::days((m as f64 * 30.44).round() as i64)),
                    _ => None,
                })
        })
    });

    // Which tables of the prior release use each label, for exact matching.
    let mut prior_index: HashMap<(&str, &str, &str), Vec<&ReleaseRow>> = HashMap::new();
    let mut prior_labels: HashMap<(&str, &str), usize> = HashMap::new();
    if let Some(p) = prior {
        for r in &p.data.rows {
            prior_index.entry((r.table_key.as_str(), r.group_key.as_str(), r.label_key.as_str())).or_default().push(r);
            *prior_labels.entry((r.table_key.as_str(), r.label_key.as_str())).or_default() += 1;
        }
    }
    let mut cur_labels: HashMap<(&str, &str), usize> = HashMap::new();
    for r in &cur.rows {
        *cur_labels.entry((r.table_key.as_str(), r.label_key.as_str())).or_default() += 1;
    }

    let make_row = |r: &ReleaseRow| -> Option<XbrlFactRow> {
        // The row's own period: balance, else the quarter, else the longest
        // span, all ending on the release's period end; quarter-label tables
        // their latest quarter.
        let rank = |p: &Period| -> (u32, u32) {
            match p {
                Period::At(_) => (4, 0),
                Period::Span(3, _) => (3, 0),
                Period::Span(0, _) => (1, 0),
                Period::Span(m, _) => (2, *m),
                Period::Quarter(..) => (0, 0),
            }
        };
        let (period, value) = match p_end {
            Some(end) if r.values.iter().any(|(p, _)| p.end().is_some()) => r
                .values
                .iter()
                .copied()
                .filter(|(p, _)| p.end().is_some_and(|d| near(d, end, 3)))
                .max_by_key(|(p, _)| rank(p))?,
            _ => r.values.iter().copied().filter(|(p, _)| matches!(p, Period::Quarter(..))).max_by_key(|(p, _)| *p)?,
        };
        let to_change = |(p, v): (Period, f64), from: Option<String>| -> XbrlChange {
            let change = value - v;
            XbrlChange {
                period: p.short(),
                value: round_to(v, 4),
                change: round_to(change, 4),
                pct: if v != 0.0 { Some(round_to(change / v.abs() * 100.0, 1)) } else { None },
                from,
            }
        };
        let mut missing: Vec<String> = Vec::new();
        let yoy = find_value(&r.values, period, true).map(|c| to_change(c, None));
        if yoy.is_none() {
            let target = match period {
                Period::At(d) | Period::Span(_, d) => year_before(d).to_string(),
                Period::Quarter(y, q) => format!("Q{q} {}", y - 1),
            };
            missing.push(format!("yoy: {target} not printed in this release"));
        }
        let wants_qoq = matches!(period, Period::At(_) | Period::Span(3, _) | Period::Quarter(..));
        let mut qoq = if wants_qoq { find_value(&r.values, period, false).map(|c| to_change(c, None)) } else { None };
        if wants_qoq && qoq.is_none() {
            let target = match period {
                Period::At(d) | Period::Span(_, d) => (d - chrono::Duration::days(91)).to_string(),
                Period::Quarter(..) => "the prior quarter".to_string(),
            };
            match (prior, period) {
                (_, Period::Quarter(..)) => missing.push(format!("qoq: {target} not printed in this release")),
                (None, _) => missing.push(format!(
                    "qoq: {target} not printed in this release; {}",
                    opts.prior_note.as_deref().unwrap_or("no previous earnings release found")
                )),
                (Some(p), _) => {
                    let exact = prior_index.get(&(r.table_key.as_str(), r.group_key.as_str(), r.label_key.as_str()));
                    let unique_both = prior_labels.get(&(r.table_key.as_str(), r.label_key.as_str())) == Some(&1)
                        && cur_labels.get(&(r.table_key.as_str(), r.label_key.as_str())) == Some(&1);
                    let found: Result<&ReleaseRow, String> = match exact {
                        Some(v) if v.len() == 1 => Ok(v[0]),
                        Some(_) => Err(format!("the label appears more than once in that table of {}", p.label)),
                        None if unique_both => p
                            .data
                            .rows
                            .iter()
                            .find(|o| o.table_key == r.table_key && o.label_key == r.label_key)
                            .ok_or_else(|| format!("no row with this label in the same table of {}", p.label)),
                        None => Err(format!("no row with this label in the same table of {}", p.label)),
                    };
                    match found {
                        Ok(o) if o.unit != r.unit => missing.push(format!("qoq: units differ in {} ({} vs {})", p.label, o.unit, r.unit)),
                        Ok(o) => match o.values.iter().copied().find(|(op, _)| match (period, *op) {
                            (Period::At(d), Period::At(od)) => near(od, d - chrono::Duration::days(91), 12),
                            (Period::Span(3, d), Period::Span(3, od)) => near(od, d - chrono::Duration::days(91), 12),
                            _ => false,
                        }) {
                            Some(c) => qoq = Some(to_change(c, Some(p.label.clone()))),
                            None => missing.push(format!("qoq: {target} not printed for this row in {}", p.label)),
                        },
                        Err(why) => missing.push(format!("qoq: {why}")),
                    }
                }
            }
        }
        // Balances: vs the prior fiscal year-end column, when that is not
        // already the year-ago or quarter-ago column.
        let mut vs_fye = None;
        if let (Period::At(d), Some(fy)) = (period, fye_date) {
            let is_other = !near(fy, year_before(d), 10) && !near(fy, d - chrono::Duration::days(91), 12);
            if is_other {
                vs_fye = r
                    .values
                    .iter()
                    .copied()
                    .find(|(p, _)| matches!(p, Period::At(o) if near(*o, fy, 10)))
                    .map(|c| to_change(c, None));
                if vs_fye.is_none() {
                    missing.push(format!("vs_fye: {fy} not printed in this release"));
                }
            }
        }
        Some(XbrlFactRow {
            concept: format!("release:{}", r.label),
            label: r.label.clone(),
            section: Some(r.section.clone()),
            dims: BTreeMap::new(),
            period: period.label(),
            unit: r.unit.clone(),
            value: round_to(value, 4),
            qoq,
            yoy,
            vs_fye,
            missing,
        })
    };

    let filters: Vec<String> = opts
        .concepts
        .iter()
        .map(|c| c.trim().to_ascii_lowercase().replace(' ', ""))
        .filter(|c| !c.is_empty())
        .collect();
    let mut rows: Vec<XbrlFactRow> = Vec::new();
    let selection: String;
    if !filters.is_empty() {
        let all = filters.iter().any(|f| f == "*" || f == "all");
        let matches = |r: &ReleaseRow| {
            let l = r.label.to_ascii_lowercase().replace(' ', "");
            all || filters.iter().any(|q| l.contains(q.as_str()))
        };
        rows.extend(cur.rows.iter().filter(|r| matches(r)).filter_map(make_row));
        selection = format!("label filter [{}] over every table of the release", opts.concepts.join(", "));
    } else {
        // 1. Every line of the primary statements.
        let statement_rows: Vec<XbrlFactRow> = cur.rows.iter().filter(|r| r.statement.is_some()).filter_map(make_row).collect();
        // A figure already returned (a segment total restating a statement
        // line, a quarter table repeating a segment table) is not repeated.
        let mut shown: HashSet<(String, i64)> = statement_rows.iter().map(|r| (r.unit.clone(), (r.value * 100.0).round() as i64)).collect();
        // Floor for currency movers: 0.05% of total assets, as with XBRL.
        let total_assets = cur
            .rows
            .iter()
            .find(|r| r.statement == Some("balance_sheet") && r.label_key == "total assets")
            .and_then(|r| {
                let e = r.scale?;
                let v = r.values.iter().filter(|(p, _)| matches!(p, Period::At(d) if Some(*d) == p_end)).map(|(_, v)| *v).next()?;
                Some(v.abs() * 10f64.powi(e))
            });
        let floor = total_assets.map(|a| a * 0.0005).unwrap_or(0.0);
        rows.extend(statement_rows);
        // 2. Rows of the other tables (segments, products) that moved a lot.
        let mut movers: Vec<(f64, XbrlFactRow)> = Vec::new();
        for r in cur.rows.iter().filter(|r| r.statement.is_none()) {
            if matches!(r.unit_kind, UnitKind::Percent | UnitKind::Count) {
                continue;
            }
            let Some(row) = make_row(r) else { continue };
            if shown.contains(&(row.unit.clone(), (row.value * 100.0).round() as i64)) {
                continue;
            }
            let mut best: Option<(f64, f64)> = None;
            for c in [&row.qoq, &row.yoy, &row.vs_fye].into_iter().flatten() {
                if let Some(p) = c.pct {
                    let raw = c.change.abs() * 10f64.powi(r.scale.unwrap_or(0));
                    if p.abs() >= opts.min_change_pct && best.is_none_or(|b| p.abs() > b.0) {
                        best = Some((p.abs(), raw));
                    }
                }
            }
            let Some((pct, raw)) = best else { continue };
            let money = r.unit_kind == UnitKind::Money && r.scale.is_some();
            if money && floor > 0.0 && raw < floor {
                continue;
            }
            let score = if money && floor > 0.0 { raw / floor } else { pct / 100.0 };
            shown.insert((row.unit.clone(), (row.value * 100.0).round() as i64));
            movers.push((score, row));
        }
        movers.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        const MOVERS_CAP: usize = 25;
        let total_movers = movers.len();
        rows.extend(movers.into_iter().take(MOVERS_CAP).map(|(_, r)| r));
        selection = format!(
            "each line of the release's primary statements (balance at period end; flows for the quarter where printed, else the longest span) + {} rows of its other tables (segments, products, reconciliations) that moved >= {}% (largest first{}). Every row matching a label: xbrl_concepts=<label words>",
            total_movers.min(MOVERS_CAP),
            opts.min_change_pct,
            if total_movers > MOVERS_CAP { format!(", {total_movers} qualified") } else { String::new() }
        );
        if floor > 0.0 {
            notes.push(format!(
                "Currency movers need a change of at least 0.05% of total assets ({} millions).",
                round_to(floor / 1e6, 1)
            ));
        }
    }
    notes.push(format!(
        "Source: the earnings press release ({}) filed with this {form}. Values are exactly as printed in its tables (parentheses = negative), in the units its captions state; the same quarter arrives as XBRL weeks later with the 10-Q / 10-K.",
        cur.exhibit
    ));
    notes.push("concept is a key made from the printed label (release:<label>), not a us-gaap tag; release rows are never mapped to the taxonomy.".to_string());
    notes.push(match prior {
        Some(p) => format!(
            "Comparisons are between values the company printed: other columns of this release (no `from`), or the same table and row label in its previous earnings release ({}).",
            p.label
        ),
        None => "Comparisons are between values the company printed in other columns of this release.".to_string(),
    });
    notes.extend(cur.notes.iter().cloned());

    FilingXbrl {
        source,
        document_type: Some(form.to_string()),
        period_end: p_end.map(|d| d.to_string()),
        fiscal_year: None,
        fiscal_period: None,
        facts_in_filing: cur.values,
        selection,
        returned: rows.len(),
        facts: rows,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::super::html::parse_filing_html;
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn parses_column_headers() {
        let h = header_period("Three Months Ended September 27, 2026").unwrap();
        assert_eq!((h.months, h.date), (Some(3), Some(d("2026-09-27"))));
        // Multi-row header joined top to bottom.
        let h = header_period("Twelve Months Ended September 26, 2026").unwrap();
        assert_eq!((h.months, h.date), (Some(12), Some(d("2026-09-26"))));
        let h = header_period("Nine Months Ended Sep 26, 2026").unwrap();
        assert_eq!((h.months, h.date), (Some(9), Some(d("2026-09-26"))));
        let h = header_period("Six Months Ended July 26, 2026").unwrap();
        assert_eq!(h.months, Some(6));
        let h = header_period("13 Weeks Ended Jan. 3, 2027").unwrap();
        assert_eq!((h.months, h.date), (Some(3), Some(d("2027-01-03"))));
        let h = header_period("Quarter Ended September 30, 2026").unwrap();
        assert_eq!(h.months, Some(3));
        let h = header_period("Year To Date September 30, 2026").unwrap();
        assert_eq!(h.months, Some(0));
        let h = header_period("As of December 31, 2025 (unaudited)").unwrap();
        assert_eq!((h.months, h.date), (None, Some(d("2025-12-31"))));
        let h = header_period("Q3 2026").unwrap();
        assert_eq!((h.date, h.quarter), (None, Some((2026, 3))));
        assert!(header_period("Three Months Ended").is_none());
        assert!(header_period("September 30, 2026 June 30, 2026").is_none());
        assert!(header_period("Three Months Ended Nine Months Ended September 30, 2026").is_none());
    }

    #[test]
    fn parses_printed_numbers() {
        let v = |s: &str| parse_printed_number(s).map(|p| p.value);
        assert_eq!(v("$82,904"), Some(82904.0));
        assert_eq!(v("(321)"), Some(-321.0));
        assert_eq!(v("$(290)"), Some(-290.0));
        assert_eq!(v("($290)"), Some(-290.0));
        assert_eq!(v("(1,234.5)"), Some(-1234.5));
        assert_eq!(v("—"), Some(0.0));
        assert_eq!(v("$2.11"), Some(2.11));
        assert_eq!(v("-17"), Some(-17.0));
        assert_eq!(v("–1,738"), Some(-1738.0));
        let p = parse_printed_number("(3)%").unwrap();
        assert_eq!((p.value, p.percent), (-3.0, true));
        assert_eq!(parse_printed_number("$1").unwrap().currency, Some("USD"));
        assert!(parse_printed_number("n/a").is_none());
        assert!(parse_printed_number("up 21%").is_none());
        assert!(parse_printed_number("$15.0 - $16.0").is_none());
        assert!(parse_printed_number("12,34").is_none());
    }

    #[test]
    fn reads_caption_scale() {
        let u = caption_units("(In millions, except number of shares, which are reflected in thousands, and per-share amounts)");
        assert_eq!((u.money, u.shares), (Some(6), ShareScale::Exp(3)));
        let u = caption_units("(In millions, except per share amounts) (Unaudited)");
        assert_eq!((u.money, u.shares), (Some(6), ShareScale::Same));
        let u = caption_units("(in thousands, except share and per share data)");
        assert_eq!((u.money, u.shares), (Some(3), ShareScale::Unstated));
        let u = caption_units("($ in billions)");
        assert_eq!(u.money, Some(9));
        assert_eq!(u.currency, Some("USD"));
        assert_eq!(caption_units("Segment results").money, None);
        let u = caption_units("€ millions, unless otherwise stated");
        assert_eq!((u.money, u.currency), (Some(6), Some("EUR")));
        assert_eq!(label_scale("Revenue ($B)"), Some(9));
    }

    #[test]
    fn normalizes_labels() {
        assert_eq!(clean_label("Total net sales (1)"), "Total net sales");
        assert_eq!(clean_label("(1) Net sales by reportable segment:"), "Net sales by reportable segment");
        assert_eq!(clean_label("Net income (loss)"), "Net income (loss)");
        assert_eq!(normalize_label("Shareholders’ equity:*"), normalize_label("SHAREHOLDERS' EQUITY"));
        assert_eq!(normalize_label("Accounts  receivable, net"), "accounts receivable net");
        assert_ne!(normalize_label("Inventories"), normalize_label("Inventory"));
    }

    const RELEASE: &str = r#"<html><body>
      <p>Example Corp reports results.</p>
      <p>EXAMPLE CORP.</p>
      <p>CONDENSED CONSOLIDATED STATEMENTS OF OPERATIONS (Unaudited)</p>
      <p>(In millions, except number of shares, which are reflected in thousands, and per-share amounts)</p>
      <table>
        <tr><td></td><td colspan="2">Three Months Ended</td><td colspan="2">Twelve Months Ended</td></tr>
        <tr><td></td><td>September 26,</td><td>September 27,</td><td>September 26,</td><td>September 27,</td></tr>
        <tr><td></td><td>2026</td><td>2025</td><td>2026</td><td>2025</td></tr>
        <tr><td>Net sales:</td><td></td><td></td><td></td><td></td></tr>
        <tr><td>Products</td><td>$82,904</td><td>$73,716</td><td>$355,533</td><td>$307,003</td></tr>
        <tr><td>Services</td><td>31,573</td><td>28,750</td><td>123,301</td><td>109,158</td></tr>
        <tr><td>Total net sales (1)</td><td>114,477</td><td>102,466</td><td>478,834</td><td>416,161</td></tr>
        <tr><td>Cost of sales:</td></tr>
        <tr><td>Products</td><td>52,064</td><td>47,019</td><td>215,874</td><td>194,116</td></tr>
        <tr><td>Total cost of sales</td><td>59,657</td><td>54,125</td><td>245,232</td><td>220,960</td></tr>
        <tr><td>Other income/(expense), net</td><td>504</td><td>377</td><td>1,174</td><td>(321)</td></tr>
        <tr><td>Earnings per share:</td></tr>
        <tr><td>Diluted</td><td>$2.10</td><td>$1.85</td><td>$8.98</td><td>$7.46</td></tr>
        <tr><td>Shares used in computing earnings per share:</td></tr>
        <tr><td>Diluted</td><td>14,630,318</td><td>14,863,609</td><td>14,720,306</td><td>15,004,697</td></tr>
      </table>
      <p>CONDENSED CONSOLIDATED BALANCE SHEETS (Unaudited)</p>
      <p>(In millions)</p>
      <table>
        <tr><td></td><td>September 26, 2026</td><td>June 27, 2026</td><td>September 27, 2025</td></tr>
        <tr><td>Inventories</td><td>$14,541</td><td>$7,000</td><td>$5,718</td></tr>
        <tr><td>Retained earnings/(Accumulated deficit)</td><td>13,700</td><td>9,000</td><td>(14,264)</td></tr>
        <tr><td>Total assets</td><td>$404,548</td><td>$380,000</td><td>$359,241</td></tr>
      </table>
      <p>Segment results (In millions)</p>
      <table>
        <tr><td></td><td>Q3 2026</td><td>Q3 2026 vs Q3 2025</td><td>Q3 2025</td></tr>
        <tr><td>Widgets</td><td>$900</td><td>80%</td><td>$500</td></tr>
        <tr><td>Gadgets</td><td>$100</td><td>0%</td><td>$100</td></tr>
        <tr><td>Total</td><td>$1,000</td><td>67%</td><td>$600</td></tr>
      </table>
      <p>CONDENSED CONSOLIDATED STATEMENTS OF CASH FLOWS</p>
      <table>
        <tr><td></td><td>September 26, 2026</td><td>September 27, 2025</td></tr>
        <tr><td>Inventories</td><td>(8,905)</td><td>1,400</td></tr>
        <tr><td>Net cash</td><td>(8,905)</td><td>1,400</td></tr>
      </table>
    </body></html>"#;

    #[test]
    fn finds_statements_and_reads_rows() {
        let doc = parse_filing_html(RELEASE);
        let tables = detect_release_tables(&doc, "ex99.1");
        let ids: Vec<&str> = tables.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["ex99.1:income_statement", "ex99.1:balance_sheet", "ex99.1:segment_results", "ex99.1:cash_flow"]);
        // Section text is the title, units line and table, nothing else.
        let is = &tables[0];
        let text = doc.text_range(is.start, is.end);
        assert!(text.starts_with("CONDENSED CONSOLIDATED STATEMENTS OF OPERATIONS"), "{text}");
        assert!(!text.contains("EXAMPLE CORP") && !text.contains("BALANCE SHEETS"));

        let data = extract_release(&doc, &tables, "EX-99.1", Some((9, 27)));
        assert_eq!(data.period_end, Some(d("2026-09-26")));
        let labels: Vec<&str> = data.rows.iter().map(|r| r.label.as_str()).collect();
        let row = |label: &str| data.rows.iter().find(|r| r.label == label).unwrap_or_else(|| panic!("{label} not in {labels:?}"));
        let prod = row("Products [Net sales]");
        assert_eq!(prod.unit, "USD millions");
        assert!(prod.values.contains(&(Period::Span(3, d("2026-09-26")), 82904.0)));
        assert!(prod.values.contains(&(Period::Span(12, d("2025-09-27")), 307003.0)));
        assert_eq!(row("Total net sales").values.len(), 4);
        assert!(row("Other income/(expense), net").values.contains(&(Period::Span(12, d("2025-09-27")), -321.0)));
        assert_eq!(row("Diluted [Earnings per share]").unit, "USD per share");
        assert_eq!(row("Diluted [Shares used in computing earnings per share]").unit, "shares thousands");
        // Cash flow columns have dates but no span: not read as data.
        assert!(data.rows.iter().all(|r| r.statement != Some("cash_flow")));
        assert!(data.notes.iter().any(|n| n.contains("ex99.1:cash_flow")));
        // The "vs" change column is not a value column.
        let w = row("Widgets");
        assert_eq!(w.values, vec![(Period::Quarter(2026, 3), 900.0), (Period::Quarter(2025, 3), 500.0)]);

        let x = build_release_xbrl(
            &data,
            None,
            &ReleaseOptions { concepts: Vec::new(), min_change_pct: 25.0, fye: Some((9, 27)), prior_note: None },
            "8-K",
        );
        let inv = x.facts.iter().find(|r| r.label == "Inventories").expect("inventory row");
        assert_eq!(inv.section.as_deref(), Some("ex99.1:balance_sheet"));
        assert_eq!(inv.period, "at 2026-09-26");
        assert_eq!(inv.value, 14541.0);
        assert_eq!(inv.concept, "release:Inventories");
        let q = inv.qoq.as_ref().expect("qoq from own column");
        assert_eq!((q.value, q.change, q.from.as_deref()), (7000.0, 7541.0, None));
        let y = inv.yoy.as_ref().expect("yoy");
        assert_eq!((y.value, y.pct), (5718.0, Some(154.3)));
        let re = x.facts.iter().find(|r| r.label.starts_with("Retained")).unwrap();
        assert_eq!(re.yoy.as_ref().unwrap().value, -14264.0);
        let sales = x.facts.iter().find(|r| r.label == "Total net sales").unwrap();
        assert_eq!(sales.period, "3M ended 2026-09-26");
        assert!(sales.qoq.is_none() && sales.missing.iter().any(|m| m.starts_with("qoq:")));
        // Movers: Widgets moved 80%; Gadgets didn't.
        assert!(x.facts.iter().any(|r| r.label == "Widgets" && r.period == "Q3 2026"));
        assert!(!x.facts.iter().any(|r| r.label == "Gadgets"));
    }

    #[test]
    fn quarter_over_quarter_from_previous_release() {
        let doc = parse_filing_html(RELEASE);
        let tables = detect_release_tables(&doc, "ex99.1");
        let cur = extract_release(&doc, &tables, "EX-99.1", Some((9, 27)));
        let prev_html = RELEASE
            .replace("September 26,</td><td>September 27,", "June 27,</td><td>June 28,")
            .replace("Twelve Months Ended", "Nine Months Ended")
            .replace("82,904", "70,000");
        let pdoc = parse_filing_html(&prev_html);
        let ptables = detect_release_tables(&pdoc, "ex99.1");
        let prev = extract_release(&pdoc, &ptables, "EX-99.1", Some((9, 27)));
        let prior = PriorRelease { data: &prev, label: "8-K 2026-07-30".to_string() };
        let x = build_release_xbrl(
            &cur,
            Some(&prior),
            &ReleaseOptions { concepts: vec!["products".to_string()], min_change_pct: 25.0, fye: Some((9, 27)), prior_note: None },
            "8-K",
        );
        let prod = x.facts.iter().find(|r| r.label == "Products [Net sales]").expect("net sales products");
        let q = prod.qoq.as_ref().expect("qoq from prior release");
        assert_eq!((q.value, q.from.as_deref(), q.period.as_str()), (70000.0, Some("8-K 2026-07-30"), "2026-06-27"));
        // "Products" under cost of sales matches its own group, not net sales.
        let cost = x.facts.iter().find(|r| r.label == "Products [Cost of sales]").expect("cost products");
        assert_eq!(cost.qoq.as_ref().map(|q| q.value), Some(52064.0));
    }
}
