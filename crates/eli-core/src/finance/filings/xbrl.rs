//! Numbers as data: the filing's inline-XBRL facts with prior-period
//! comparisons.
//!
//! Source of truth is the inline XBRL in the primary document itself (every
//! tagged number, including company-extension concepts and segment
//! dimensions, available the minute the filing is accepted). Each fact knows
//! where it sits (statement / note / item) and the row label the filer printed
//! next to it. Comparisons come from the same filing first (the comparative
//! columns), then from SEC companyfacts (prior 10-Q/10-K values, non-
//! dimensional us-gaap/dei facts) for quarter-over-quarter and year-ago
//! balances the filing itself doesn't carry. A Q4 quarter that was never
//! reported on its own is derived as FY minus the 9-month YTD.

use super::html::ParsedDoc;
use super::sections::{FormFamily, SectionKind, Structure};
use chrono::{Datelike, NaiveDate};
use eli_finance_types::{FilingXbrl, XbrlChange, XbrlFactRow};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug)]
pub(crate) struct Fact {
    pub concept: String,
    /// Value in reporting units (USD, shares), scale and sign applied.
    pub value: f64,
    /// Presentation scale from the tag (6 = shown in millions).
    pub scale: i32,
    pub unit: String,
    pub start: Option<NaiveDate>,
    pub end: NaiveDate,
    pub dims: Vec<(String, String)>,
    /// Block index in the rendered document (usize::MAX when hidden).
    pub block: usize,
    pub row_label: Option<String>,
    /// The filing prints this number with the opposite sign of its XBRL value
    /// (e.g. "(4,516)" for an IncreaseDecreaseInInventories of +4,516).
    pub flip: bool,
    pub in_label_cell: bool,
}

impl Fact {
    fn months(&self) -> u32 {
        span_months(self.start, self.end)
    }
}

fn span_months(start: Option<NaiveDate>, end: NaiveDate) -> u32 {
    match start {
        None => 0,
        Some(s) => {
            let days = (end - s).num_days() + 1;
            ((days as f64) / 30.44).round().max(1.0) as u32
        }
    }
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    let s = s.get(..10).unwrap_or(s);
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

const NUMBER_WORDS: &[(&str, f64)] = &[
    ("no", 0.0),
    ("none", 0.0),
    ("zero", 0.0),
    ("one", 1.0),
    ("two", 2.0),
    ("three", 3.0),
    ("four", 4.0),
    ("five", 5.0),
    ("six", 6.0),
    ("seven", 7.0),
    ("eight", 8.0),
    ("nine", 9.0),
    ("ten", 10.0),
    ("eleven", 11.0),
    ("twelve", 12.0),
    ("thirteen", 13.0),
    ("fourteen", 14.0),
    ("fifteen", 15.0),
    ("sixteen", 16.0),
    ("seventeen", 17.0),
    ("eighteen", 18.0),
    ("nineteen", 19.0),
    ("twenty", 20.0),
    ("thirty", 30.0),
    ("forty", 40.0),
    ("fifty", 50.0),
    ("sixty", 60.0),
    ("seventy", 70.0),
    ("eighty", 80.0),
    ("ninety", 90.0),
    ("hundred", 100.0),
];

/// Parse an ix:nonFraction display value per its `format` transform.
pub(crate) fn parse_ix_number(text: &str, format: Option<&str>) -> Option<f64> {
    let fmt = format
        .map(|f| f.rsplit(':').next().unwrap_or(f).to_ascii_lowercase())
        .unwrap_or_default();
    let t = text.trim();
    match fmt.as_str() {
        "fixed-zero" | "zerodash" | "fixedzero" | "fixed-empty" => return Some(0.0),
        "fixed-false" | "fixed-true" | "booleanfalse" | "booleantrue" => return None,
        "num-comma-decimal" | "numcommadecimal" | "num-unit-decimal" | "numunitdecimal" => {
            let cleaned: String = t
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == ',')
                .map(|c| if c == ',' { '.' } else { c })
                .collect();
            return cleaned.parse::<f64>().ok();
        }
        "numwordsen" | "num-word-en" | "numwordsen-1" => {
            let lower = t.to_ascii_lowercase();
            let mut total = 0.0;
            let mut any = false;
            for w in lower.split(|c: char| !c.is_ascii_alphabetic()) {
                if let Some((_, v)) = NUMBER_WORDS.iter().find(|(k, _)| *k == w) {
                    if *v == 100.0 {
                        total = if total == 0.0 { 100.0 } else { total * 100.0 };
                    } else {
                        total += v;
                    }
                    any = true;
                }
            }
            return if any { Some(total) } else { t.parse::<f64>().ok() };
        }
        _ => {}
    }
    if matches!(t, "—" | "–" | "-" | "") {
        return if fmt.is_empty() { None } else { Some(0.0) };
    }
    let cleaned: String = t
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    cleaned.parse::<f64>().ok()
}

/// Facts from the parsed document: numbers, periods, units, dimensions.
pub(crate) fn extract_facts(doc: &ParsedDoc) -> Vec<Fact> {
    let mut out: Vec<Fact> = Vec::new();
    let mut seen: HashMap<(String, String, String), usize> = HashMap::new();
    for rf in &doc.facts {
        if rf.nil || rf.concept.is_empty() {
            continue;
        }
        let Some(ctx) = doc.contexts.get(&rf.context) else {
            continue;
        };
        let Some(mut v) = parse_ix_number(&rf.text, rf.format.as_deref()) else {
            continue;
        };
        v *= 10f64.powi(rf.scale);
        if rf.negative {
            v = -v;
        }
        let (start, end) = match (&ctx.instant, &ctx.start, &ctx.end) {
            (Some(i), _, _) => (None, parse_date(i)),
            (None, Some(s), Some(e)) => (parse_date(s), parse_date(e)),
            (None, None, Some(e)) => (None, parse_date(e)),
            _ => (None, None),
        };
        let Some(end) = end else {
            continue;
        };
        let unit = rf
            .unit
            .as_deref()
            .and_then(|u| doc.units.get(u).cloned())
            .unwrap_or_default();
        let key = (rf.concept.clone(), rf.context.clone(), unit.clone());
        let block = if rf.hidden { usize::MAX } else { rf.block };
        if let Some(&idx) = seen.get(&key) {
            // Same fact tagged twice: keep the visible, earliest placement.
            if out[idx].block == usize::MAX && block != usize::MAX {
                out[idx].block = block;
                out[idx].row_label = rf.row_label.clone();
                out[idx].scale = rf.scale;
                out[idx].flip = out[idx].value != 0.0 && rf.shown_negative != (out[idx].value < 0.0)
                    && (rf.shown_negative || rf.row_label.is_some());
                out[idx].in_label_cell = rf.in_label_cell;
            }
            continue;
        }
        seen.insert(key, out.len());
        let flip = v != 0.0 && rf.shown_negative != (v < 0.0) && !rf.hidden && block != usize::MAX
            && (rf.shown_negative || rf.row_label.is_some());
        out.push(Fact {
            concept: rf.concept.clone(),
            value: v,
            scale: rf.scale,
            unit,
            start,
            end,
            dims: ctx.dims.clone(),
            block,
            row_label: rf.row_label.clone(),
            flip,
            in_label_cell: rf.in_label_cell,
        });
    }
    out
}

#[derive(Clone, Debug)]
pub(crate) struct PeriodInfo {
    pub period_end: NaiveDate,
    /// First day of the fiscal year (from the YTD dei context of a 10-Q).
    pub fy_start: Option<NaiveDate>,
    pub fiscal_period: Option<String>,
    pub fiscal_year: Option<String>,
    pub document_type: Option<String>,
}

pub(crate) fn period_info(doc: &ParsedDoc, facts: &[Fact]) -> Option<PeriodInfo> {
    let dei_ctx = ["dei:DocumentPeriodEndDate", "dei:DocumentType", "dei:DocumentFiscalPeriodFocus"]
        .iter()
        .find_map(|k| doc.dei.get(*k))
        .and_then(|(_, ctx)| doc.contexts.get(ctx));
    let mut period_end = dei_ctx.and_then(|c| {
        c.end
            .as_deref()
            .or(c.instant.as_deref())
            .and_then(parse_date)
    });
    let mut fy_start = dei_ctx
        .and_then(|c| c.start.as_deref().and_then(parse_date))
        .filter(|s| period_end.is_some_and(|e| (e - *s).num_days() > 40));
    if period_end.is_none() {
        // Most common end date among visible facts (all facts if none are placed).
        let mut counts: HashMap<NaiveDate, usize> = HashMap::new();
        let any_visible = facts.iter().any(|f| f.block != usize::MAX);
        for f in facts.iter().filter(|f| !any_visible || f.block != usize::MAX) {
            *counts.entry(f.end).or_insert(0) += 1;
        }
        period_end = counts.into_iter().max_by_key(|(d, c)| (*c, *d)).map(|(d, _)| d);
        fy_start = None;
    }
    let period_end = period_end?;
    let dei = |k: &str| doc.dei.get(k).map(|(t, _)| t.trim().to_string()).filter(|t| !t.is_empty());
    Some(PeriodInfo {
        period_end,
        fy_start,
        fiscal_period: dei("dei:DocumentFiscalPeriodFocus"),
        fiscal_year: dei("dei:DocumentFiscalYearFocus"),
        document_type: dei("dei:DocumentType"),
    })
}

// ───────────────────────────── companyfacts ─────────────────────────────

#[derive(Deserialize)]
struct CfRoot {
    #[serde(default)]
    facts: HashMap<String, HashMap<String, CfConceptRaw>>,
}

#[derive(Deserialize)]
struct CfConceptRaw {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    units: HashMap<String, Vec<CfEntryRaw>>,
}

#[derive(Deserialize)]
struct CfEntryRaw {
    #[serde(default)]
    start: Option<String>,
    end: String,
    val: serde_json::Number,
    #[serde(default)]
    accn: Option<String>,
    #[serde(default)]
    form: Option<String>,
    #[serde(default)]
    filed: Option<String>,
}

#[derive(Clone, Debug)]
struct CfEntry {
    start: Option<NaiveDate>,
    end: NaiveDate,
    val: f64,
    accn: String,
    form: String,
    filed: String,
}

#[derive(Default)]
pub(crate) struct CompanyFacts {
    /// "us-gaap:InventoryNet" -> (label, unit -> entries)
    concepts: HashMap<String, (Option<String>, HashMap<String, Vec<CfEntry>>)>,
    pub accessions: HashSet<String>,
}

impl CompanyFacts {
    pub(crate) fn parse(bytes: &[u8]) -> Option<CompanyFacts> {
        let root: CfRoot = serde_json::from_slice(bytes).ok()?;
        let mut cf = CompanyFacts::default();
        for (taxonomy, concepts) in root.facts {
            for (name, raw) in concepts {
                let mut units: HashMap<String, Vec<CfEntry>> = HashMap::new();
                for (unit, entries) in raw.units {
                    let list: Vec<CfEntry> = entries
                        .into_iter()
                        .filter_map(|e| {
                            if let Some(a) = e.accn.as_ref() {
                                cf.accessions.insert(a.clone());
                            }
                            Some(CfEntry {
                                start: e.start.as_deref().and_then(parse_date),
                                end: parse_date(&e.end)?,
                                val: e.val.as_f64()?,
                                accn: e.accn.unwrap_or_default(),
                                form: e.form.unwrap_or_default(),
                                filed: e.filed.unwrap_or_default(),
                            })
                        })
                        .collect();
                    units.insert(unit, list);
                }
                cf.concepts.insert(format!("{taxonomy}:{name}"), (raw.label, units));
            }
        }
        Some(cf)
    }

    fn label(&self, concept: &str) -> Option<&str> {
        self.concepts.get(concept).and_then(|(l, _)| l.as_deref())
    }

    /// Every fact SEC attributes to one accession (for documents without
    /// inline XBRL): non-dimensional, no position in the document.
    pub(crate) fn facts_for_accession(&self, accn: &str) -> Vec<Fact> {
        let mut out = Vec::new();
        for (concept, (_, units)) in &self.concepts {
            for (unit, entries) in units {
                for e in entries.iter().filter(|e| e.accn == accn) {
                    let currency = is_currency_unit(unit);
                    let scale = if (currency || unit == "shares") && e.val.abs() >= 1e7 { 6 } else { 0 };
                    out.push(Fact {
                        concept: concept.clone(),
                        value: e.val,
                        scale,
                        unit: unit.clone(),
                        start: e.start,
                        end: e.end,
                        dims: Vec::new(),
                        block: usize::MAX,
                        row_label: None,
                        flip: false,
                        in_label_cell: false,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.concept.cmp(&b.concept).then(b.end.cmp(&a.end)));
        out
    }

    /// Latest-filed value (as of `filed_max`) for this concept/unit with the
    /// given span whose end is within `tol` days of `target_end`.
    fn find(
        &self,
        concept: &str,
        unit: &str,
        months: u32,
        target_end: NaiveDate,
        tol: i64,
        filed_max: &str,
    ) -> Option<&CfEntry> {
        let (_, units) = self.concepts.get(concept)?;
        let entries = units.get(unit)?;
        entries
            .iter()
            .filter(|e| filed_max.is_empty() || e.filed.as_str() <= filed_max)
            .filter(|e| span_months(e.start, e.end) == months)
            .filter(|e| (e.end - target_end).num_days().abs() <= tol)
            .max_by(|a, b| {
                a.filed
                    .cmp(&b.filed)
                    .then_with(|| (b.end - target_end).num_days().abs().cmp(&(a.end - target_end).num_days().abs()))
            })
    }
}

// ───────────────────────────── comparisons ─────────────────────────────

fn shift_years(d: NaiveDate, years: i32) -> NaiveDate {
    let y = d.year() + years;
    NaiveDate::from_ymd_opt(y, d.month(), d.day())
        .or_else(|| NaiveDate::from_ymd_opt(y, d.month(), 28))
        .unwrap_or(d)
}

fn period_label(start: Option<NaiveDate>, end: NaiveDate) -> String {
    match start {
        None => format!("at {end}"),
        Some(_) => format!("{}M ended {end}", span_months(start, end)),
    }
}

struct Cmp {
    period: String,
    raw: f64,
    from: Option<String>,
}

fn same_dims(a: &[(String, String)], b: &[(String, String)]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut x: Vec<&(String, String)> = a.iter().collect();
    let mut y: Vec<&(String, String)> = b.iter().collect();
    x.sort();
    y.sort();
    x == y
}

/// Facts from the previous 10-Q / 10-K, for quarter-over-quarter values the
/// current filing doesn't carry (extension concepts and segment dimensions
/// included, which companyfacts lacks).
pub(crate) struct PriorFiling<'a> {
    pub facts: &'a [Fact],
    /// "10-Q 2026-07-31"
    pub label: String,
}

struct Ctx<'a> {
    facts: &'a [Fact],
    by_concept: HashMap<&'a str, Vec<usize>>,
    prior: Option<(&'a PriorFiling<'a>, HashMap<&'a str, Vec<usize>>)>,
    cf: Option<&'a CompanyFacts>,
    filed_max: String,
}

fn find_in<'a>(
    facts: &'a [Fact],
    index: &HashMap<&str, Vec<usize>>,
    f: &Fact,
    months: u32,
    target: NaiveDate,
    tol: i64,
) -> Option<&'a Fact> {
    index.get(f.concept.as_str())?.iter().map(|&i| &facts[i]).find(|o| {
        o.unit == f.unit
            && same_dims(&o.dims, &f.dims)
            && o.months() == months
            && (o.end - target).num_days().abs() <= tol
            && !(o.end == f.end && o.start == f.start)
    })
}

impl<'a> Ctx<'a> {
    fn lookup(&self, f: &Fact, months: u32, target: NaiveDate, tol: i64) -> Option<Cmp> {
        if let Some(o) = find_in(self.facts, &self.by_concept, f, months, target, tol) {
            return Some(Cmp {
                period: period_label(o.start, o.end),
                raw: o.value,
                from: None,
            });
        }
        if let Some((prior, index)) = self.prior.as_ref() {
            if let Some(o) = find_in(prior.facts, index, f, months, target, tol) {
                return Some(Cmp {
                    period: period_label(o.start, o.end),
                    raw: o.value,
                    from: Some(prior.label.clone()),
                });
            }
        }
        if !f.dims.is_empty() {
            return None;
        }
        let cf = self.cf?;
        let e = cf.find(&f.concept, &f.unit, months, target, tol, &self.filed_max)?;
        Some(Cmp {
            period: period_label(e.start, e.end),
            raw: e.val,
            from: Some(format!("{} {}", e.form, e.filed)),
        })
    }

    /// Q4 never reported alone: FY minus the 9-month YTD.
    fn derived_q4(&self, f: &Fact, q4_end: NaiveDate) -> Option<Cmp> {
        if !f.dims.is_empty() {
            return None;
        }
        let cf = self.cf?;
        let fy = cf.find(&f.concept, &f.unit, 12, q4_end, 10, &self.filed_max)?;
        let nine_end = q4_end - chrono::Duration::days(91);
        let nine = cf.find(&f.concept, &f.unit, 9, nine_end, 12, &self.filed_max)?;
        Some(Cmp {
            period: format!("3M ended {}", fy.end),
            raw: fy.val - nine.val,
            from: Some(format!("derived: FY ({} {}) minus 9M YTD ({} {})", fy.form, fy.filed, nine.form, nine.filed)),
        })
    }
}

// ───────────────────────────── selection ─────────────────────────────

const STATEMENT_IDS: &[&str] = &["income_statement", "comprehensive_income", "balance_sheet", "cash_flow", "equity"];

/// Headline concepts used when a filing's statements can't be located.
const CORE_CONCEPTS: &[&str] = &[
    "Revenues",
    "RevenueFromContractWithCustomerExcludingAssessedTax",
    "CostOfRevenue",
    "CostOfGoodsAndServicesSold",
    "GrossProfit",
    "ResearchAndDevelopmentExpense",
    "SellingGeneralAndAdministrativeExpense",
    "OperatingExpenses",
    "OperatingIncomeLoss",
    "IncomeTaxExpenseBenefit",
    "NetIncomeLoss",
    "EarningsPerShareBasic",
    "EarningsPerShareDiluted",
    "CashAndCashEquivalentsAtCarryingValue",
    "AccountsReceivableNetCurrent",
    "InventoryNet",
    "AssetsCurrent",
    "PropertyPlantAndEquipmentNet",
    "Goodwill",
    "Assets",
    "AccountsPayableCurrent",
    "LiabilitiesCurrent",
    "LongTermDebtNoncurrent",
    "LongTermDebt",
    "Liabilities",
    "StockholdersEquity",
    "NetCashProvidedByUsedInOperatingActivities",
    "NetCashProvidedByUsedInInvestingActivities",
    "NetCashProvidedByUsedInFinancingActivities",
    "PaymentsToAcquirePropertyPlantAndEquipment",
    "PaymentsForRepurchaseOfCommonStock",
    "PaymentsOfDividends",
];

pub(crate) struct XbrlOptions {
    pub concepts: Vec<String>,
    pub min_change_pct: f64,
}

fn humanize(concept: &str) -> String {
    let local = concept.rsplit(':').next().unwrap_or(concept);
    let mut out = String::new();
    let chars: Vec<char> = local.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev_lower = chars[i - 1].is_lowercase();
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev_lower || (chars[i - 1].is_uppercase() && next_lower) {
                out.push(' ');
            }
        }
        if i > 0 && c.is_uppercase() && out.ends_with(' ') {
            out.extend(c.to_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

fn is_generic_label(l: &str) -> bool {
    let k = l.trim().trim_end_matches(':').to_ascii_lowercase();
    k.chars().count() < 3
        || matches!(
            k.as_str(),
            "total" | "other" | "net" | "basic" | "diluted" | "balance" | "amount" | "thereafter"
                | "total, net" | "consolidated" | "total consolidated" | "corporate" | "eliminations"
                | "subtotal" | "all other" | "net of tax" | "ending balance" | "beginning balance"
                | "total segments" | "segment total"
        )
}

fn scale_word(scale: i32) -> &'static str {
    match scale {
        3 => " thousands",
        6 => " millions",
        9 => " billions",
        _ => "",
    }
}

/// Display scale and unit label for a fact.
fn display_unit(unit: &str, scale: i32) -> (i32, String) {
    match unit {
        "pure" => {
            if scale == -2 {
                (-2, "%".to_string())
            } else {
                (0, "ratio".to_string())
            }
        }
        u if u.contains('/') => {
            let (num, den) = u.split_once('/').unwrap_or((u, ""));
            let den = if den == "shares" { "share" } else { den };
            (0, format!("{num} per {den}"))
        }
        u => {
            let s = if matches!(scale, 3 | 6 | 9) { scale } else { 0 };
            (s, format!("{}{}", if u.is_empty() { "units" } else { u }, scale_word(s)))
        }
    }
}

fn round_to(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

fn strip_ns(s: &str) -> String {
    s.rsplit(':').next().unwrap_or(s).to_string()
}

fn is_currency_unit(u: &str) -> bool {
    u.len() == 3 && u.chars().all(|c| c.is_ascii_uppercase())
}

/// Build the XBRL view for one filing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_xbrl(
    doc: &ParsedDoc,
    structure: &Structure,
    facts: &[Fact],
    period: Option<&PeriodInfo>,
    prior: Option<&PriorFiling<'_>>,
    cf: Option<&CompanyFacts>,
    filing_date: &str,
    opts: &XbrlOptions,
    cf_note: Option<String>,
) -> FilingXbrl {
    let mut notes: Vec<String> = Vec::new();
    if let Some(n) = cf_note {
        notes.push(n);
    }
    let period = match period {
        Some(p) if !facts.is_empty() => p,
        _ => {
            let hint = if structure.family == FormFamily::EightK {
                "This document tags no numbers in XBRL (an 8-K cover tags only text), and none of its EX-99 exhibits carries financial statement tables to read instead. Read the exhibits with section=ex99.1; the quarter's numbers arrive as XBRL with the 10-Q / 10-K."
            } else {
                "No numeric inline-XBRL facts in this document; use section= for the statements' text."
            };
            return FilingXbrl {
                source: "inline XBRL".to_string(),
                document_type: period.and_then(|p| p.document_type.clone()),
                period_end: period.map(|p| p.period_end.to_string()),
                fiscal_year: None,
                fiscal_period: None,
                facts_in_filing: facts.len(),
                selection: "none".to_string(),
                returned: 0,
                facts: Vec::new(),
                notes: vec![hint.to_string()],
            };
        }
    };
    let p_end = period.period_end;

    // Section id per block (deepest section wins).
    let nblocks = doc.blocks.len();
    let mut block_section: Vec<Option<&str>> = vec![None; nblocks];
    let mut secs: Vec<&super::sections::Section> = structure.sections.iter().collect();
    secs.sort_by_key(|s| s.kind.level());
    for s in secs {
        if matches!(s.kind, SectionKind::Cover | SectionKind::Document) {
            continue;
        }
        for slot in block_section.iter_mut().take(s.end.min(nblocks)).skip(s.start) {
            *slot = Some(s.id.as_str());
        }
    }
    let section_of = |f: &Fact| -> Option<String> {
        if f.block == usize::MAX {
            return None;
        }
        block_section.get(f.block).copied().flatten().map(str::to_string)
    };

    let mut by_concept: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, f) in facts.iter().enumerate() {
        by_concept.entry(f.concept.as_str()).or_default().push(i);
    }
    let prior_index = prior.map(|p| {
        let mut idx: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, f) in p.facts.iter().enumerate() {
            idx.entry(f.concept.as_str()).or_default().push(i);
        }
        (p, idx)
    });
    let ctx = Ctx {
        facts,
        by_concept,
        prior: prior_index,
        cf,
        filed_max: filing_date.to_string(),
    };
    // Rows outside the statements whose table row carries several concepts
    // (a commitments table by year): the row label alone is ambiguous.
    let mut concepts_per_block: HashMap<usize, HashSet<&str>> = HashMap::new();
    for f in facts.iter().filter(|f| f.block != usize::MAX) {
        concepts_per_block.entry(f.block).or_default().insert(f.concept.as_str());
    }
    let is_10q = structure.family == FormFamily::TenQ;
    let prior_fye = period.fy_start.map(|s| s - chrono::Duration::days(1));
    let total_assets = facts
        .iter()
        .find(|f| f.concept == "us-gaap:Assets" && f.dims.is_empty() && f.start.is_none() && f.end == p_end)
        .map(|f| f.value.abs());

    let make_row = |f: &Fact| -> XbrlFactRow {
        let months = f.months();
        let (dscale, unit_label) = display_unit(&f.unit, f.scale);
        // Values carry the sign as printed in the filing.
        let sign = if f.flip { -1.0 } else { 1.0 };
        let shown = |raw: f64| round_to(sign * raw / 10f64.powi(dscale), 4);
        let to_change = |c: Cmp| -> XbrlChange {
            let change = f.value - c.raw;
            let pct = if c.raw != 0.0 {
                Some(round_to(sign * change / c.raw.abs() * 100.0, 1))
            } else {
                None
            };
            XbrlChange {
                // Same span as the row's own period; only the end date differs.
                period: c.period.rsplit(' ').next().unwrap_or(&c.period).to_string(),
                value: shown(c.raw),
                change: shown(change),
                pct,
                from: c.from,
            }
        };
        let yoy = ctx
            .lookup(f, months, shift_years(f.end, -1), 10)
            .map(to_change);
        let qoq = if months == 0 || months == 3 {
            let target = f.end - chrono::Duration::days(91);
            ctx.lookup(f, months, target, 12)
                .or_else(|| {
                    if months == 3 && prior_fye.is_some_and(|fye| (fye - target).num_days().abs() <= 12) {
                        ctx.derived_q4(f, prior_fye.unwrap_or(target))
                    } else {
                        None
                    }
                })
                .map(to_change)
        } else {
            None
        };
        let vs_fye = if is_10q && months == 0 {
            prior_fye.and_then(|fye| {
                let q_target = f.end - chrono::Duration::days(91);
                if (fye - q_target).num_days().abs() <= 12 {
                    None // Q1: the prior quarter-end *is* the fiscal year-end
                } else {
                    ctx.lookup(f, 0, fye, 7).map(to_change)
                }
            })
        } else {
            None
        };
        // Name the sources searched, so a blank comparison reads as "not
        // disclosed" rather than as a gap in the tool.
        let mut missing: Vec<String> = Vec::new();
        let mut sources: Vec<String> = vec!["this filing".to_string()];
        if let Some((p, _)) = ctx.prior.as_ref() {
            sources.push(p.label.clone());
        }
        if ctx.cf.is_some() && f.dims.is_empty() {
            sources.push("SEC companyfacts".to_string());
        }
        let searched = sources.join(", ");
        if qoq.is_none() && (months == 0 || months == 3) {
            let target = f.end - chrono::Duration::days(91);
            missing.push(format!("qoq: {target} not reported in {searched}"));
        }
        if yoy.is_none() {
            missing.push(format!("yoy: {} not reported in {searched}", shift_years(f.end, -1)));
        }
        let taxonomy_label = || {
            cf.and_then(|c| c.label(&f.concept))
                .map(str::to_string)
                .unwrap_or_else(|| humanize(&f.concept))
        };
        let row_label = f
            .row_label
            .as_deref()
            .filter(|l| !is_generic_label(l))
            .map(|l| l.trim().trim_end_matches(':').to_string());
        let shared_row = concepts_per_block.get(&f.block).is_some_and(|c| c.len() > 1);
        let in_statement = section_of(f).is_some_and(|s| STATEMENT_IDS.contains(&s.as_str()));
        let generic_row = f.row_label.as_deref().is_some_and(is_generic_label);
        let label = match row_label {
            Some(r) if shared_row && !in_statement => format!("{} [{}]", taxonomy_label(), r),
            Some(r) => r,
            None if generic_row => format!(
                "{} [{}]",
                taxonomy_label(),
                f.row_label.as_deref().unwrap_or("").trim().trim_end_matches(':')
            ),
            None => taxonomy_label(),
        };
        let label = if label.chars().count() > 110 {
            format!("{}…", label.chars().take(108).collect::<String>())
        } else {
            label
        };
        XbrlFactRow {
            concept: f.concept.clone(),
            label,
            section: section_of(f),
            dims: f.dims.iter().map(|(a, m)| (strip_ns(a), strip_ns(m))).collect::<BTreeMap<_, _>>(),
            period: period_label(f.start, f.end),
            unit: unit_label,
            value: shown(f.value),
            qoq,
            yoy,
            vs_fye,
            missing,
        }
    };

    // Current facts end on the period end (balances at it, flows ending on it).
    let current: Vec<&Fact> = facts.iter().filter(|f| (f.end - p_end).num_days().abs() <= 3).collect();

    let mut rows: Vec<XbrlFactRow> = Vec::new();
    let selection: String;
    let filters: Vec<String> = opts
        .concepts
        .iter()
        .map(|c| c.trim().to_ascii_lowercase().replace(' ', ""))
        .filter(|c| !c.is_empty())
        .collect();
    if !filters.is_empty() {
        let all = filters.iter().any(|f| f == "*" || f == "all");
        let matches = |f: &Fact| -> bool {
            if all {
                return true;
            }
            let name = f.concept.to_ascii_lowercase();
            let label = f
                .row_label
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase()
                .replace(' ', "");
            filters.iter().any(|q| name.contains(q.as_str()) || (!label.is_empty() && label.contains(q.as_str())))
        };
        let mut picked: Vec<&Fact> = current.iter().copied().filter(|f| matches(f)).collect();
        let mut note = String::new();
        if picked.is_empty() && !all {
            picked = facts.iter().filter(|f| matches(f)).collect();
            if !picked.is_empty() {
                note = " (no current-period match; showing every period)".to_string();
            }
        }
        picked.sort_by_key(|f| (f.block, f.concept.clone()));
        const CAP: usize = 600;
        if picked.len() > CAP {
            notes.push(format!("{} facts matched; first {CAP} in document order returned. Narrow xbrl_concepts.", picked.len()));
            picked.truncate(CAP);
        }
        rows.extend(picked.into_iter().map(make_row));
        selection = format!("concept filter [{}]{note}", opts.concepts.join(", "));
    } else {
        // 1. Every non-dimensional current number printed in the primary statements.
        let mut in_statements: Vec<&Fact> = current
            .iter()
            .copied()
            .filter(|f| f.dims.is_empty() && !f.in_label_cell)
            .filter(|f| section_of(f).is_some_and(|s| STATEMENT_IDS.contains(&s.as_str())))
            .collect();
        let used_statements = !in_statements.is_empty();
        if !used_statements {
            in_statements = current
                .iter()
                .copied()
                .filter(|f| f.dims.is_empty())
                .filter(|f| {
                    let local = f.concept.rsplit(':').next().unwrap_or("");
                    CORE_CONCEPTS.contains(&local)
                })
                .collect();
        }
        // One row per line item: the balance, else the quarter (3M) where the
        // filing reports one, else the longest year-to-date span.
        let mut best_period: HashMap<(&str, &str), u32> = HashMap::new();
        for f in &in_statements {
            let m = f.months();
            let key = (f.concept.as_str(), f.unit.as_str());
            let better = |cur: u32| -> bool {
                let rank = |x: u32| -> (u32, u32) {
                    match x {
                        0 => (3, 0),
                        3 => (2, 0),
                        x => (1, x),
                    }
                };
                rank(m) > rank(cur)
            };
            match best_period.get(&key) {
                Some(&cur) if !better(cur) => {}
                _ => {
                    best_period.insert(key, m);
                }
            }
        }
        in_statements.retain(|f| best_period.get(&(f.concept.as_str(), f.unit.as_str())) == Some(&f.months()));
        in_statements.sort_by_key(|f| (f.block, f.months()));
        let mut seen: HashSet<(String, u32, String)> = HashSet::new();
        for f in in_statements {
            if seen.insert((f.concept.clone(), f.months(), f.unit.clone())) {
                rows.push(make_row(f));
            }
        }
        let statement_values: HashSet<(String, i64)> = rows
            .iter()
            .map(|r| (r.concept.clone(), (r.value * 100.0).round() as i64))
            .collect();
        // 2. Anything else (notes, MD&A, segments) that moved a lot.
        let floor = total_assets.map(|a| a * 0.0005).unwrap_or(0.0);
        let mut movers: Vec<(f64, XbrlFactRow)> = Vec::new();
        let mut mover_keys: HashSet<(String, String, i64)> = HashSet::new();
        for f in current.iter().copied() {
            if f.dims.is_empty() && seen.iter().any(|(c, _, u)| *c == f.concept && *u == f.unit) {
                continue;
            }
            if f.block == usize::MAX {
                continue;
            }
            // Equity roll-forward components restate the statement lines.
            if f.dims.iter().any(|(axis, _)| axis.ends_with("StatementEquityComponentsAxis")) {
                continue;
            }
            if section_of(f).is_some_and(|s| STATEMENT_IDS.contains(&s.as_str())) {
                continue;
            }
            let currency = is_currency_unit(&f.unit);
            if !currency && !f.dims.is_empty() {
                continue;
            }
            let row = make_row(f);
            // Same number restated under another axis or in another table: once.
            let vkey = (row.concept.clone(), row.period.clone(), (row.value * 100.0).round() as i64);
            if !mover_keys.insert(vkey) {
                continue;
            }
            if !f.dims.is_empty() && statement_values.contains(&(row.concept.clone(), (row.value * 100.0).round() as i64)) {
                continue;
            }
            let mut best: Option<(f64, f64)> = None; // (|pct|, |raw change|)
            for c in [&row.qoq, &row.yoy, &row.vs_fye].into_iter().flatten() {
                if let Some(p) = c.pct {
                    let raw_change = (c.change * 10f64.powi(display_unit(&f.unit, f.scale).0)).abs();
                    if p.abs() >= opts.min_change_pct && best.is_none_or(|b| p.abs() > b.0) {
                        best = Some((p.abs(), raw_change));
                    }
                }
            }
            let Some((pct, raw_change)) = best else {
                continue;
            };
            if currency && raw_change < floor {
                continue;
            }
            let score = if currency && floor > 0.0 { raw_change / floor } else { pct / 100.0 };
            movers.push((score, row));
        }
        movers.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        const MOVERS_CAP: usize = 25;
        let total_movers = movers.len();
        rows.extend(movers.into_iter().take(MOVERS_CAP).map(|(_, r)| r));
        selection = format!(
            "{} + {} facts outside them that moved >= {}% (largest first{}). Every period of a concept: xbrl_concepts=<name>",
            if used_statements {
                "each line item of the primary statements (balance at period end; flows for the quarter where reported, else year-to-date)"
            } else {
                "headline concepts (statements not located)"
            },
            total_movers.min(MOVERS_CAP),
            opts.min_change_pct,
            if total_movers > MOVERS_CAP { format!(", {total_movers} qualified") } else { String::new() }
        );
        if floor > 0.0 {
            notes.push(format!(
                "Movers in currency need a change of at least 0.05% of total assets ({} {}).",
                round_to(floor / 1e6, 1),
                "USD millions"
            ));
        }
    }
    notes.push("Comparisons come from this filing's own columns unless `from` names an earlier filing (prior 10-Q/10-K inline XBRL, or SEC companyfacts).".to_string());
    notes.push("Values carry the sign printed in the filing (parentheses = negative); for flow items like IncreaseDecreaseInInventories that can be the opposite of the XBRL concept's own sign.".to_string());

    FilingXbrl {
        source: if !doc.is_inline_xbrl {
            "SEC companyfacts entries for this accession (the document has no inline XBRL)".to_string()
        } else if cf.is_some() {
            "inline XBRL in the primary document + SEC companyfacts for prior periods".to_string()
        } else {
            "inline XBRL in the primary document".to_string()
        },
        document_type: period.document_type.clone(),
        period_end: Some(p_end.to_string()),
        fiscal_year: period.fiscal_year.clone(),
        fiscal_period: period.fiscal_period.clone(),
        facts_in_filing: facts.len(),
        selection,
        returned: rows.len(),
        facts: rows,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ix_number_formats() {
        assert_eq!(parse_ix_number("14,541", Some("ixt:num-dot-decimal")), Some(14541.0));
        assert_eq!(parse_ix_number("—", Some("ixt:fixed-zero")), Some(0.0));
        assert_eq!(parse_ix_number("1.234,5", Some("ixt:num-comma-decimal")), Some(1234.5));
        assert_eq!(parse_ix_number("three", Some("ixt-sec:numwordsen")), Some(3.0));
        assert_eq!(parse_ix_number("0.63", None), Some(0.63));
        assert_eq!(parse_ix_number("—", None), None);
    }

    #[test]
    fn spans_and_labels() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(span_months(Some(d("2026-07-01")), d("2026-09-30")), 3);
        assert_eq!(span_months(Some(d("2026-06-28")), d("2026-09-26")), 3);
        assert_eq!(span_months(Some(d("2025-12-28")), d("2026-09-26")), 9);
        assert_eq!(span_months(Some(d("2024-09-29")), d("2025-09-27")), 12);
        assert_eq!(span_months(None, d("2026-09-30")), 0);
        assert_eq!(period_label(None, d("2026-09-30")), "at 2026-09-30");
        assert_eq!(period_label(Some(d("2026-07-01")), d("2026-09-30")), "3M ended 2026-09-30");
        assert_eq!(shift_years(d("2026-09-30"), -1), d("2025-09-30"));
        assert_eq!(shift_years(d("2024-02-29"), -1), d("2023-02-28"));
        assert_eq!(humanize("us-gaap:InventoryNet"), "Inventory net");
        assert_eq!(display_unit("USD", 6), (6, "USD millions".to_string()));
        assert_eq!(display_unit("USD/shares", 0), (0, "USD per share".to_string()));
        assert_eq!(display_unit("pure", -2), (-2, "%".to_string()));
    }

    #[test]
    fn builds_rows_with_prior_period_changes() {
        use super::super::html::parse_filing_html;
        use super::super::sections::detect_structure;
        // A 10-Q balance sheet tagging inventory at quarter-end and prior FYE,
        // plus a quarterly revenue line with its year-ago comparative.
        let html = r#"<html><body><div style="display:none"><ix:header><ix:hidden>
          <ix:nonNumeric name="dei:DocumentType" contextRef="c-1">10-Q</ix:nonNumeric>
          <ix:nonNumeric name="dei:DocumentPeriodEndDate" contextRef="c-1">September 30, 2026</ix:nonNumeric>
          </ix:hidden><ix:resources>
          <xbrli:context id="c-1"><xbrli:period><xbrli:startDate>2026-01-01</xbrli:startDate><xbrli:endDate>2026-09-30</xbrli:endDate></xbrli:period></xbrli:context>
          <xbrli:context id="q"><xbrli:period><xbrli:startDate>2026-07-01</xbrli:startDate><xbrli:endDate>2026-09-30</xbrli:endDate></xbrli:period></xbrli:context>
          <xbrli:context id="qp"><xbrli:period><xbrli:startDate>2025-07-01</xbrli:startDate><xbrli:endDate>2025-09-30</xbrli:endDate></xbrli:period></xbrli:context>
          <xbrli:context id="i"><xbrli:period><xbrli:instant>2026-09-30</xbrli:instant></xbrli:period></xbrli:context>
          <xbrli:context id="fye"><xbrli:period><xbrli:instant>2025-12-31</xbrli:instant></xbrli:period></xbrli:context>
          <xbrli:unit id="usd"><xbrli:measure>iso4217:USD</xbrli:measure></xbrli:unit>
          </ix:resources></ix:header></div>
          <p>PART I</p><table><tr><td>Item 1.</td><td>Financial Statements</td></tr></table>
          <p>CONSOLIDATED STATEMENTS OF OPERATIONS</p>
          <table><tr><td></td><td>2026</td><td>2025</td></tr>
          <tr><td>Total net sales</td><td><ix:nonFraction name="us-gaap:Revenues" contextRef="q" unitRef="usd" scale="6" decimals="-6">201,944</ix:nonFraction></td><td><ix:nonFraction name="us-gaap:Revenues" contextRef="qp" unitRef="usd" scale="6" decimals="-6">180,169</ix:nonFraction></td></tr>
          <tr><td>Cost of sales and other operating expenses</td><td>94,573</td><td>88,670</td></tr><tr><td>Operating income</td><td>28,076</td><td>17,422</td></tr></table>
          <p>CONSOLIDATED BALANCE SHEETS</p>
          <table><tr><td></td><td>December 31, 2025</td><td>September 30, 2026</td></tr>
          <tr><td>Inventories</td><td><ix:nonFraction name="us-gaap:InventoryNet" contextRef="fye" unitRef="usd" scale="6" decimals="-6">5,718</ix:nonFraction></td><td><ix:nonFraction name="us-gaap:InventoryNet" contextRef="i" unitRef="usd" scale="6" decimals="-6">14,541</ix:nonFraction></td></tr>
          <tr><td>Total current assets</td><td>147,957</td><td>173,857</td></tr><tr><td>Total assets</td><td>359,241</td><td>404,548</td></tr></table>
          <p>Notes to Consolidated Financial Statements</p><p>Note 1 — Basis</p><p>Text.</p>
          <table><tr><td>Item 2.</td><td>Management's Discussion and Analysis</td></tr></table><p>MD&amp;A text.</p>
        </body></html>"#;
        let doc = parse_filing_html(html);
        let st = detect_structure(&doc, "10-Q");
        let facts = extract_facts(&doc);
        let period = period_info(&doc, &facts);
        assert_eq!(period.as_ref().map(|p| p.period_end.to_string()).as_deref(), Some("2026-09-30"));
        let x = build_xbrl(
            &doc,
            &st,
            &facts,
            period.as_ref(),
            None,
            None,
            "2026-10-30",
            &XbrlOptions { concepts: Vec::new(), min_change_pct: 25.0 },
            None,
        );
        let inv = x.facts.iter().find(|r| r.concept == "us-gaap:InventoryNet").expect("inventory row");
        assert_eq!(inv.section.as_deref(), Some("balance_sheet"));
        assert_eq!(inv.label, "Inventories");
        assert_eq!(inv.unit, "USD millions");
        assert_eq!(inv.value, 14541.0);
        let fye = inv.vs_fye.as_ref().expect("vs fye");
        assert_eq!(fye.value, 5718.0);
        assert_eq!(fye.change, 8823.0);
        assert_eq!(fye.pct, Some(154.3));
        let rev = x.facts.iter().find(|r| r.concept == "us-gaap:Revenues").expect("revenue row");
        assert_eq!(rev.period, "3M ended 2026-09-30");
        let yoy = rev.yoy.as_ref().expect("yoy");
        assert_eq!(yoy.value, 180169.0);
        assert_eq!(yoy.pct, Some(12.1));
        // Concept filter.
        let only = build_xbrl(
            &doc,
            &st,
            &facts,
            period.as_ref(),
            None,
            None,
            "2026-10-30",
            &XbrlOptions { concepts: vec!["inventory".to_string()], min_change_pct: 25.0 },
            None,
        );
        assert_eq!(only.returned, 1);
        assert_eq!(only.facts[0].concept, "us-gaap:InventoryNet");
    }
}
