//! Filing HTML -> structured plain text.
//!
//! SEC primary documents (10-K / 10-Q / 8-K, almost all inline-XBRL XHTML
//! since 2019) are rendered into a flat list of blocks: paragraphs, table rows
//! (each table's columns aligned as plain text with ` | ` separators), and
//! page-break markers. Page furniture (page numbers, running headers/footers,
//! "Table of Contents" back-links) is flagged so section text leaves it out.
//!
//! The same single pass collects the inline-XBRL facts, contexts and units,
//! with each fact's position (block index) and the label of the table row it
//! sits in, so `xbrl.rs` can say which statement or note a number came from.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Para,
    Row,
    PageBreak,
}

#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub kind: BlockKind,
    /// Rendered text. For table rows: the aligned row (columns padded, ` | `
    /// between columns) or, for layout tables, the cells joined by spaces.
    pub text: String,
    /// Table rows only: the non-empty cells after `$`/`)`/`%` fragments are merged.
    pub cells: Vec<String>,
    /// Table ordinal (1-based) for rows, 0 otherwise.
    pub table: u32,
    /// Row of a layout table (single text column / single row): reads as prose.
    pub layout: bool,
    /// Page number, running header/footer, or "Table of Contents" back-link.
    pub furniture: bool,
}

impl Block {
    fn para(text: String) -> Self {
        Block {
            kind: BlockKind::Para,
            text,
            cells: Vec::new(),
            table: 0,
            layout: false,
            furniture: false,
        }
    }

    /// Text used for heading detection: rows read as their cells joined by spaces.
    pub(crate) fn heading_text(&self) -> Cow<'_, str> {
        match self.kind {
            BlockKind::Row => Cow::Owned(self.cells.join(" ")),
            _ => Cow::Borrowed(self.text.as_str()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct XbrlContext {
    pub start: Option<String>,
    pub end: Option<String>,
    pub instant: Option<String>,
    pub dims: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub(crate) struct RawFact {
    pub concept: String,
    pub context: String,
    pub unit: Option<String>,
    pub scale: i32,
    pub negative: bool,
    pub format: Option<String>,
    pub nil: bool,
    pub text: String,
    pub block: usize,
    pub row_label: Option<String>,
    pub hidden: bool,
    /// Printed in parentheses / with a minus sign in its table cell.
    pub shown_negative: bool,
    /// Sits inside a row's label cell ("Preferred stock ($0.01 par value; 500
    /// shares authorized)"): a parenthetical detail, not a line item.
    pub in_label_cell: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct TextBlockSpan {
    pub concept: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedDoc {
    pub blocks: Vec<Block>,
    pub facts: Vec<RawFact>,
    pub contexts: HashMap<String, XbrlContext>,
    pub units: HashMap<String, String>,
    /// dei facts (concept -> (text, contextRef)), first occurrence.
    pub dei: HashMap<String, (String, String)>,
    /// ix:nonNumeric text blocks (notes / policies) by block span.
    pub text_blocks: Vec<TextBlockSpan>,
    pub is_inline_xbrl: bool,
}

impl ParsedDoc {
    /// Clean text of blocks `[start, end)`: furniture and page breaks dropped,
    /// a blank line around each data table.
    pub(crate) fn text_range(&self, start: usize, end: usize) -> String {
        let end = end.min(self.blocks.len());
        let mut out = String::new();
        let mut prev_table: Option<u32> = None;
        for b in self.blocks.iter().take(end).skip(start) {
            if b.furniture || b.kind == BlockKind::PageBreak {
                continue;
            }
            let data_row = b.kind == BlockKind::Row && !b.layout;
            if data_row {
                if prev_table != Some(b.table) && !out.is_empty() && !out.ends_with("\n\n") {
                    out.push('\n');
                }
                prev_table = Some(b.table);
            } else {
                if prev_table.is_some() && !out.ends_with("\n\n") {
                    out.push('\n');
                }
                prev_table = None;
            }
            out.push_str(&b.text);
            out.push('\n');
        }
        let trimmed_len = out.trim_end().len();
        out.truncate(trimmed_len);
        out
    }

    /// Character count of `text_range` without building it twice in callers.
    pub(crate) fn chars_range(&self, start: usize, end: usize) -> usize {
        self.text_range(start, end).chars().count()
    }
}

/// Parse filing HTML (XHTML / inline XBRL or legacy HTML) into blocks + facts.
pub(crate) fn parse_filing_html(html: &str) -> ParsedDoc {
    let mut p = Parser::default();
    p.run(strip_sgml_wrapper(html));
    p.finish()
}

/// Some EDGAR documents keep their SGML envelope (`<DOCUMENT><TYPE>EX-99.1
/// <SEQUENCE>2<FILENAME>...<TEXT>`); the readable document starts after <TEXT>.
fn strip_sgml_wrapper(raw: &str) -> &str {
    let head_len = raw.len().min(4096);
    let head = match raw.get(..head_len) {
        Some(h) => h,
        None => return raw,
    };
    let trimmed = head.trim_start();
    if !(trimmed.starts_with("<DOCUMENT>") || trimmed.starts_with("<TYPE>") || trimmed.starts_with("<SEC-DOCUMENT>")) {
        return raw;
    }
    match head.find("<TEXT>") {
        Some(p) => {
            let body = &raw[p + "<TEXT>".len()..];
            match body.rfind("</TEXT>") {
                Some(e) => &body[..e],
                None => body,
            }
        }
        None => raw,
    }
}

/// Render plain text (legacy .txt filings) into paragraph blocks.
pub(crate) fn parse_plain_text(text: &str) -> ParsedDoc {
    let mut doc = ParsedDoc::default();
    for line in text.lines() {
        let t = collapse_ws(line);
        if t.is_empty() {
            continue;
        }
        doc.blocks.push(Block::para(t));
    }
    mark_furniture(&mut doc.blocks);
    doc
}

/// Heuristic: does this payload look like HTML/XHTML rather than plain text?
pub(crate) fn looks_like_html(raw: &str) -> bool {
    let head: String = raw.chars().take(4096).collect();
    let lower = head.to_ascii_lowercase();
    lower.contains("<html")
        || lower.contains("<!doctype")
        || lower.contains("xmlns:ix=")
        || lower.contains("<ix:")
        || lower.contains("<body")
        || lower.contains("<div")
        || lower.contains("<p ")
        || lower.contains("<p>")
        || lower.contains("<table")
}

// ───────────────────────────── parser ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    None,
    Table,
    Row,
    Cell,
    Fact(usize),
    NonNumeric(usize),
    Exclude,
    Resources,
    Context,
    Unit,
    UnitDenominator,
    Capture,
}

#[derive(Debug)]
struct OpenEl {
    name: String,
    block: bool,
    hidden: bool,
    page_break_after: bool,
    role: Role,
}

#[derive(Debug, Default)]
struct RawCell {
    text: String,
    colspan: usize,
}

#[derive(Debug, Default)]
struct TableBuilder {
    ordinal: u32,
    rows: Vec<Vec<RawCell>>,
    in_cell: bool,
    cell_text: String,
    cell_colspan: usize,
}

#[derive(Debug)]
enum Pos {
    Block(usize),
    TableRow(u32, usize),
    Hidden,
}

#[derive(Debug)]
struct FactBuilder {
    fact: RawFact,
    pos: Pos,
}

#[derive(Debug)]
enum NonNumericKind {
    Dei,
    TextBlock,
    Other,
}

#[derive(Debug)]
struct NonNumericBuilder {
    concept: String,
    context: String,
    kind: NonNumericKind,
    text: String,
    start: Pos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureKind {
    StartDate,
    EndDate,
    Instant,
    Explicit,
    Typed,
    Measure,
}

#[derive(Debug, Default)]
struct UnitBuilder {
    id: String,
    num: Vec<String>,
    den: Vec<String>,
    in_den: bool,
}

#[derive(Default)]
struct Parser {
    blocks: Vec<Block>,
    para: String,
    stack: Vec<OpenEl>,
    suppress: usize,
    tables: Vec<TableBuilder>,
    table_counter: u32,
    table_row_blocks: HashMap<u32, Vec<usize>>,
    fact_builders: Vec<FactBuilder>,
    open_facts: Vec<usize>,
    exclude_depth: usize,
    nn_builders: Vec<NonNumericBuilder>,
    open_nn: Vec<usize>,
    contexts: HashMap<String, XbrlContext>,
    units: HashMap<String, String>,
    cur_context: Option<(String, XbrlContext)>,
    cur_unit: Option<UnitBuilder>,
    capture: Option<(CaptureKind, String, String)>,
    resources_depth: usize,
    is_ixbrl: bool,
}

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

const BLOCK_TAGS: &[&str] = &[
    "address", "article", "aside", "blockquote", "body", "caption", "center", "dd", "div", "dl",
    "dt", "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5",
    "h6", "header", "hr", "html", "li", "main", "nav", "ol", "p", "pre", "section", "ul",
];

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

impl Parser {
    fn run(&mut self, html: &str) {
        let bytes = html.as_bytes();
        let len = bytes.len();
        let mut i = 0usize;
        while i < len {
            if bytes[i] != b'<' {
                let next = memchr(b'<', &bytes[i..]).map(|p| i + p).unwrap_or(len);
                self.text(&html[i..next]);
                i = next;
                continue;
            }
            // Comments, doctype, processing instructions, CDATA.
            if bytes[i..].starts_with(b"<!--") {
                i = find_bytes(bytes, i + 4, b"-->").map(|p| p + 3).unwrap_or(len);
                continue;
            }
            if bytes[i..].starts_with(b"<![CDATA[") {
                let end = find_bytes(bytes, i + 9, b"]]>").unwrap_or(len);
                self.text(&html[i + 9..end.min(len)]);
                i = (end + 3).min(len);
                continue;
            }
            if i + 1 < len && (bytes[i + 1] == b'!' || bytes[i + 1] == b'?') {
                i = memchr(b'>', &bytes[i..]).map(|p| i + p + 1).unwrap_or(len);
                continue;
            }
            let closing = i + 1 < len && bytes[i + 1] == b'/';
            let name_start = if closing { i + 2 } else { i + 1 };
            if name_start >= len || !bytes[name_start].is_ascii_alphabetic() {
                // A stray '<' in text.
                self.text("<");
                i += 1;
                continue;
            }
            let mut name_end = name_start;
            while name_end < len
                && (bytes[name_end].is_ascii_alphanumeric()
                    || matches!(bytes[name_end], b':' | b'-' | b'_' | b'.'))
            {
                name_end += 1;
            }
            // Find the end of the tag, respecting quoted attribute values.
            let mut j = name_end;
            let mut quote: Option<u8> = None;
            while j < len {
                let b = bytes[j];
                match quote {
                    Some(q) if b == q => quote = None,
                    Some(_) => {}
                    None if b == b'"' || b == b'\'' => quote = Some(b),
                    None if b == b'>' => break,
                    None => {}
                }
                j += 1;
            }
            let tag_end = j.min(len);
            let name = html[name_start..name_end].to_ascii_lowercase();
            let attr_src = &html[name_end..tag_end];
            let self_closing = attr_src.trim_end().ends_with('/');
            i = (tag_end + 1).min(len);
            if closing {
                self.close_tag(&name);
                continue;
            }
            if name == "script" || name == "style" {
                // Raw text element: skip to its close tag.
                let close = format!("</{name}");
                let lower_rest = &html[i..];
                let skip_to = find_ascii_ci(lower_rest, &close)
                    .map(|p| i + p)
                    .unwrap_or(len);
                i = memchr(b'>', &bytes[skip_to..])
                    .map(|p| skip_to + p + 1)
                    .unwrap_or(len);
                continue;
            }
            let attrs = parse_attrs(attr_src);
            self.open_tag(&name, &attrs, self_closing);
        }
    }

    fn finish(mut self) -> ParsedDoc {
        // Close anything left open (sloppy HTML).
        while let Some(el) = self.stack.pop() {
            self.on_close(el);
        }
        while !self.tables.is_empty() {
            self.close_table();
        }
        self.flush_para();
        mark_furniture(&mut self.blocks);

        let mut facts = Vec::with_capacity(self.fact_builders.len());
        for fb in self.fact_builders {
            let mut fact = fb.fact;
            fact.block = resolve_pos(&fb.pos, &self.table_row_blocks, self.blocks.len());
            facts.push(fact);
        }
        let mut dei = HashMap::new();
        let mut text_blocks = Vec::new();
        for nb in self.nn_builders {
            match nb.kind {
                NonNumericKind::Dei => {
                    dei.entry(nb.concept)
                        .or_insert((collapse_ws(&nb.text), nb.context));
                }
                NonNumericKind::TextBlock => {
                    let start = resolve_pos(&nb.start, &self.table_row_blocks, self.blocks.len());
                    // `text` holds the end block index (set on close).
                    let end = nb.text.parse::<usize>().unwrap_or(start);
                    text_blocks.push(TextBlockSpan {
                        concept: nb.concept,
                        start,
                        end: end.max(start),
                    });
                }
                NonNumericKind::Other => {}
            }
        }
        ParsedDoc {
            blocks: self.blocks,
            facts,
            contexts: self.contexts,
            units: self.units,
            dei,
            text_blocks,
            is_inline_xbrl: self.is_ixbrl,
        }
    }

    // ── text ──

    fn text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let t = decode_entities(raw);
        let t: &str = &t;
        if self.exclude_depth == 0 {
            for &fi in &self.open_facts {
                self.fact_builders[fi].fact.text.push_str(t);
            }
        }
        for &ni in &self.open_nn {
            let nb = &mut self.nn_builders[ni];
            if matches!(nb.kind, NonNumericKind::Dei) && nb.text.len() < 600 {
                nb.text.push_str(t);
            }
        }
        if let Some((_, _, buf)) = self.capture.as_mut() {
            buf.push_str(t);
        }
        if self.suppress > 0 {
            return;
        }
        if let Some(tb) = self.tables.last_mut() {
            if tb.in_cell {
                tb.cell_text.push_str(t);
            } else if !t.trim().is_empty() {
                // Text between cells (invalid markup): give it its own cell.
                if tb.rows.is_empty() {
                    tb.rows.push(Vec::new());
                }
                tb.in_cell = true;
                tb.cell_colspan = 1;
                tb.cell_text.push_str(t);
            }
            return;
        }
        self.para.push_str(t);
    }

    fn boundary(&mut self) {
        if let Some(tb) = self.tables.last_mut() {
            if tb.in_cell {
                // Line break inside a cell: kept for header cells
                // ("Three Months Ended<br/>September 30,"), joined otherwise.
                tb.cell_text.push('\n');
            }
            return;
        }
        self.flush_para();
    }

    fn flush_para(&mut self) {
        if self.para.is_empty() {
            return;
        }
        let t = collapse_ws(&self.para);
        self.para.clear();
        if !t.is_empty() {
            self.blocks.push(Block::para(t));
        }
    }

    fn page_break(&mut self) {
        if !self.tables.is_empty() {
            return;
        }
        self.flush_para();
        if self
            .blocks
            .last()
            .is_some_and(|b| b.kind == BlockKind::PageBreak)
        {
            return;
        }
        self.blocks.push(Block {
            kind: BlockKind::PageBreak,
            text: String::new(),
            cells: Vec::new(),
            table: 0,
            layout: false,
            furniture: false,
        });
    }

    fn current_pos(&self) -> Pos {
        if self.suppress > 0 {
            return Pos::Hidden;
        }
        if let Some(outer) = self.tables.first() {
            return Pos::TableRow(outer.ordinal, outer.rows.len().saturating_sub(1));
        }
        Pos::Block(self.blocks.len())
    }

    // ── tags ──

    fn open_tag(&mut self, name: &str, attrs: &[(String, String)], self_closing: bool) {
        if name.starts_with("ix:") {
            self.is_ixbrl = true;
        }
        let style = attr(attrs, "style")
            .map(|s| s.to_ascii_lowercase().replace([' ', '\t', '\n', '\r'], ""))
            .unwrap_or_default();
        let hidden = style.contains("display:none");
        let pb_before =
            style.contains("page-break-before:always") || style.contains("break-before:page");
        let pb_after =
            style.contains("page-break-after:always") || style.contains("break-after:page");
        let is_void = VOID_TAGS.contains(&name);
        let block = BLOCK_TAGS.contains(&name);

        // Implicit closes for sloppy HTML.
        match name {
            "td" | "th" => self.implicit_close_cell(),
            "tr" => {
                self.implicit_close_cell();
                self.implicit_close_until_role(Role::Row, Role::Table);
            }
            "p" => {
                if self.stack.last().is_some_and(|e| e.name == "p") {
                    if let Some(el) = self.stack.pop() {
                        self.on_close(el);
                    }
                }
            }
            _ => {}
        }

        if name == "br" {
            self.boundary();
            return;
        }
        if block {
            self.boundary();
        }
        if pb_before {
            self.page_break();
        }

        let local = local_name(name);
        let in_resources = self.resources_depth > 0;
        let role = match name {
            "table" => {
                self.open_table();
                Role::Table
            }
            "tr" => {
                self.open_row();
                Role::Row
            }
            "td" | "th" => {
                let colspan = attr(attrs, "colspan")
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, 200);
                self.open_cell(colspan);
                Role::Cell
            }
            "ix:nonfraction" => {
                let fact = RawFact {
                    concept: attr(attrs, "name").unwrap_or_default().to_string(),
                    context: attr(attrs, "contextref").unwrap_or_default().to_string(),
                    unit: attr(attrs, "unitref").map(str::to_string),
                    scale: attr(attrs, "scale")
                        .and_then(|v| v.trim().parse::<i32>().ok())
                        .unwrap_or(0),
                    negative: attr(attrs, "sign").is_some_and(|v| v.trim() == "-"),
                    format: attr(attrs, "format").map(str::to_string),
                    nil: attr(attrs, "xsi:nil").is_some_and(|v| v.trim() == "true"),
                    text: String::new(),
                    block: 0,
                    row_label: None,
                    hidden: self.suppress > 0,
                    shown_negative: false,
                    in_label_cell: false,
                };
                let mut fact = fact;
                if let Some(tb) = self.tables.last().filter(|t| t.in_cell) {
                    let before = tb.cell_text.trim_end().trim_end_matches('$').trim_end();
                    fact.shown_negative = before.ends_with('(') || before.ends_with('-') || before.ends_with('−');
                    fact.in_label_cell = tb.rows.last().is_none_or(|r| r.iter().all(|c| c.text.trim().is_empty()))
                        && tb.cell_text.chars().any(|c| c.is_alphabetic());
                }
                let pos = self.current_pos();
                self.fact_builders.push(FactBuilder { fact, pos });
                let idx = self.fact_builders.len() - 1;
                self.open_facts.push(idx);
                Role::Fact(idx)
            }
            "ix:nonnumeric" => {
                let concept = attr(attrs, "name").unwrap_or_default().to_string();
                let kind = if concept.starts_with("dei:") {
                    NonNumericKind::Dei
                } else if concept.ends_with("TextBlock") {
                    NonNumericKind::TextBlock
                } else {
                    NonNumericKind::Other
                };
                if matches!(kind, NonNumericKind::TextBlock) && self.tables.is_empty() {
                    self.flush_para();
                }
                let start = self.current_pos();
                self.nn_builders.push(NonNumericBuilder {
                    concept,
                    context: attr(attrs, "contextref").unwrap_or_default().to_string(),
                    kind,
                    text: String::new(),
                    start,
                });
                let idx = self.nn_builders.len() - 1;
                self.open_nn.push(idx);
                Role::NonNumeric(idx)
            }
            "ix:exclude" => {
                self.exclude_depth += 1;
                Role::Exclude
            }
            "ix:resources" => {
                self.resources_depth += 1;
                Role::Resources
            }
            _ if in_resources && local == "context" => {
                let id = attr(attrs, "id").unwrap_or_default().to_string();
                self.cur_context = Some((id, XbrlContext::default()));
                Role::Context
            }
            _ if in_resources && local == "unit" => {
                self.cur_unit = Some(UnitBuilder {
                    id: attr(attrs, "id").unwrap_or_default().to_string(),
                    ..Default::default()
                });
                Role::Unit
            }
            _ if in_resources && local == "unitdenominator" => {
                if let Some(u) = self.cur_unit.as_mut() {
                    u.in_den = true;
                }
                Role::UnitDenominator
            }
            _ if in_resources
                && matches!(
                    local,
                    "startdate" | "enddate" | "instant" | "explicitmember" | "typedmember" | "measure"
                ) =>
            {
                let kind = match local {
                    "startdate" => CaptureKind::StartDate,
                    "enddate" => CaptureKind::EndDate,
                    "instant" => CaptureKind::Instant,
                    "explicitmember" => CaptureKind::Explicit,
                    "typedmember" => CaptureKind::Typed,
                    _ => CaptureKind::Measure,
                };
                let dim = attr(attrs, "dimension").unwrap_or_default().to_string();
                self.capture = Some((kind, dim, String::new()));
                Role::Capture
            }
            _ => Role::None,
        };

        let hidden_el = hidden || name == "ix:header" || name == "head" || name == "title";
        if hidden_el {
            self.suppress += 1;
        }
        let el = OpenEl {
            name: name.to_string(),
            block,
            hidden: hidden_el,
            page_break_after: pb_after,
            role,
        };
        if is_void || self_closing {
            self.on_close(el);
        } else {
            self.stack.push(el);
        }
    }

    fn close_tag(&mut self, name: &str) {
        if VOID_TAGS.contains(&name) {
            if name == "br" {
                self.boundary();
            }
            return;
        }
        let Some(pos) = self.stack.iter().rposition(|e| e.name == name) else {
            return;
        };
        while self.stack.len() > pos {
            if let Some(el) = self.stack.pop() {
                self.on_close(el);
            }
        }
    }

    fn on_close(&mut self, el: OpenEl) {
        if el.block {
            self.boundary();
        }
        match el.role {
            Role::Table => self.close_table(),
            Role::Row => self.close_cell(),
            Role::Cell => self.close_cell(),
            Role::Fact(idx) => {
                self.open_facts.retain(|&i| i != idx);
                let label = self.current_row_label();
                if let Some(fb) = self.fact_builders.get_mut(idx) {
                    fb.fact.row_label = label;
                }
            }
            Role::NonNumeric(idx) => {
                self.open_nn.retain(|&i| i != idx);
                if matches!(self.nn_builders[idx].kind, NonNumericKind::TextBlock) {
                    if self.tables.is_empty() {
                        self.flush_para();
                    }
                    let end = self.blocks.len();
                    self.nn_builders[idx].text = end.to_string();
                }
            }
            Role::Exclude => self.exclude_depth = self.exclude_depth.saturating_sub(1),
            Role::Resources => self.resources_depth = self.resources_depth.saturating_sub(1),
            Role::Context => {
                if let Some((id, ctx)) = self.cur_context.take() {
                    if !id.is_empty() {
                        self.contexts.insert(id, ctx);
                    }
                }
            }
            Role::Unit => {
                if let Some(u) = self.cur_unit.take() {
                    let num = u.num.join("*");
                    let value = if u.den.is_empty() {
                        num
                    } else {
                        format!("{}/{}", num, u.den.join("*"))
                    };
                    if !u.id.is_empty() {
                        self.units.insert(u.id, value);
                    }
                }
            }
            Role::UnitDenominator => {
                if let Some(u) = self.cur_unit.as_mut() {
                    u.in_den = false;
                }
            }
            Role::Capture => {
                if let Some((kind, dim, buf)) = self.capture.take() {
                    let v = collapse_ws(&buf);
                    match kind {
                        CaptureKind::Measure => {
                            let m = strip_prefix_ns(&v).to_string();
                            if let Some(u) = self.cur_unit.as_mut() {
                                if u.in_den {
                                    u.den.push(m);
                                } else {
                                    u.num.push(m);
                                }
                            }
                        }
                        _ => {
                            if let Some((_, ctx)) = self.cur_context.as_mut() {
                                match kind {
                                    CaptureKind::StartDate => ctx.start = Some(v),
                                    CaptureKind::EndDate => ctx.end = Some(v),
                                    CaptureKind::Instant => ctx.instant = Some(v),
                                    CaptureKind::Explicit | CaptureKind::Typed => {
                                        ctx.dims.push((dim, v))
                                    }
                                    CaptureKind::Measure => {}
                                }
                            }
                        }
                    }
                }
            }
            Role::None => {}
        }
        if el.hidden {
            self.suppress = self.suppress.saturating_sub(1);
        }
        if el.page_break_after {
            self.page_break();
        }
    }

    fn implicit_close_cell(&mut self) {
        // Pop up to an open cell of the innermost table (not crossing a table).
        let Some(cell_pos) = self
            .stack
            .iter()
            .rposition(|e| matches!(e.role, Role::Cell | Role::Table))
        else {
            return;
        };
        if self.stack[cell_pos].role != Role::Cell {
            return;
        }
        while self.stack.len() > cell_pos {
            if let Some(el) = self.stack.pop() {
                self.on_close(el);
            }
        }
    }

    fn implicit_close_until_role(&mut self, role: Role, barrier: Role) {
        let Some(p) = self
            .stack
            .iter()
            .rposition(|e| e.role == role || e.role == barrier)
        else {
            return;
        };
        if self.stack[p].role != role {
            return;
        }
        while self.stack.len() > p {
            if let Some(el) = self.stack.pop() {
                self.on_close(el);
            }
        }
    }

    // ── tables ──

    fn open_table(&mut self) {
        let nested = self.tables.last().is_some_and(|t| t.in_cell);
        if !nested && self.tables.is_empty() {
            self.flush_para();
        }
        self.table_counter += 1;
        self.tables.push(TableBuilder {
            ordinal: self.table_counter,
            ..Default::default()
        });
    }

    fn open_row(&mut self) {
        self.close_cell();
        if let Some(tb) = self.tables.last_mut() {
            tb.rows.push(Vec::new());
        }
    }

    fn open_cell(&mut self, colspan: usize) {
        self.close_cell();
        if let Some(tb) = self.tables.last_mut() {
            if tb.rows.is_empty() {
                tb.rows.push(Vec::new());
            }
            tb.in_cell = true;
            tb.cell_text.clear();
            tb.cell_colspan = colspan;
        }
    }

    fn close_cell(&mut self) {
        if let Some(tb) = self.tables.last_mut() {
            if tb.in_cell {
                let text = std::mem::take(&mut tb.cell_text);
                let colspan = tb.cell_colspan.max(1);
                if tb.rows.is_empty() {
                    tb.rows.push(Vec::new());
                }
                if let Some(row) = tb.rows.last_mut() {
                    row.push(RawCell { text, colspan });
                }
                tb.in_cell = false;
            }
        }
    }

    fn close_table(&mut self) {
        self.close_cell();
        let Some(tb) = self.tables.pop() else {
            return;
        };
        let rendered = render_table(&tb.rows);
        if let Some(parent) = self.tables.last_mut() {
            // Nested table: flatten into the parent's current cell.
            let flat = rendered
                .iter()
                .map(|r| r.cells.join(" "))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ; ");
            if !parent.in_cell {
                if parent.rows.is_empty() {
                    parent.rows.push(Vec::new());
                }
                parent.in_cell = true;
                parent.cell_colspan = 1;
            }
            parent.cell_text.push(' ');
            parent.cell_text.push_str(&flat);
            parent.cell_text.push(' ');
            return;
        }
        let mut row_blocks = vec![usize::MAX; tb.rows.len()];
        for r in rendered {
            let idx = self.blocks.len();
            if r.source_row < row_blocks.len() {
                row_blocks[r.source_row] = idx;
            }
            self.blocks.push(Block {
                kind: BlockKind::Row,
                text: r.text,
                cells: r.cells,
                table: tb.ordinal,
                layout: r.layout,
                furniture: false,
            });
        }
        // Dropped (empty) rows map to the next rendered row, else the table's last block.
        let mut next = self.blocks.len().saturating_sub(1);
        for slot in row_blocks.iter_mut().rev() {
            if *slot == usize::MAX {
                *slot = next;
            } else {
                next = *slot;
            }
        }
        self.table_row_blocks.insert(tb.ordinal, row_blocks);
    }

    fn current_row_label(&self) -> Option<String> {
        let tb = self.tables.last()?;
        let row = tb.rows.last()?;
        row.iter()
            .map(|c| collapse_ws(&c.text))
            .find(|t| !t.is_empty() && !is_numeric_cell(t) && !is_fragment(t))
    }
}

fn resolve_pos(pos: &Pos, table_rows: &HashMap<u32, Vec<usize>>, nblocks: usize) -> usize {
    match pos {
        Pos::Block(b) => (*b).min(nblocks.saturating_sub(1)),
        Pos::TableRow(t, r) => table_rows
            .get(t)
            .and_then(|rows| rows.get(*r).or_else(|| rows.last()).copied())
            .unwrap_or(0),
        Pos::Hidden => usize::MAX,
    }
}

// ───────────────────────────── table rendering ─────────────────────────────

pub(crate) struct RenderedRow {
    pub source_row: usize,
    pub text: String,
    pub cells: Vec<String>,
    pub layout: bool,
}

struct PCell {
    start: usize,
    span: usize,
    text: String,
    lines: Vec<String>,
}

const LABEL_WIDTH_CAP: usize = 56;
const TEXT_WIDTH_CAP: usize = 60;
const SEP: &str = " | ";

fn render_table(rows: &[Vec<RawCell>]) -> Vec<RenderedRow> {
    // 1. Position cells on the grid. Each cell keeps its visual lines (split at
    //    <br>/<div> inside the cell); `text` is the lines joined by spaces.
    let mut grid: Vec<Vec<PCell>> = rows
        .iter()
        .map(|r| {
            let mut col = 0usize;
            r.iter()
                .map(|c| {
                    let span = c.colspan.max(1);
                    let lines: Vec<String> = c
                        .text
                        .split('\n')
                        .map(collapse_ws)
                        .filter(|l| !l.is_empty())
                        .collect();
                    let cell = PCell {
                        start: col,
                        span,
                        text: lines.join(" "),
                        lines,
                    };
                    col += span;
                    cell
                })
                .collect()
        })
        .collect();
    // 2. Merge `$` / `(` prefixes and `)` / `%` suffixes into their numbers.
    for row in grid.iter_mut() {
        merge_fragments(row);
    }
    let nonempty_rows: Vec<usize> = grid
        .iter()
        .enumerate()
        .filter(|(_, r)| r.iter().any(|c| !c.text.is_empty()))
        .map(|(i, _)| i)
        .collect();
    if nonempty_rows.is_empty() {
        return Vec::new();
    }

    // 3. Kept columns: starts of "leaf" cells (cells that don't span another cell's start).
    let starts: BTreeSet<usize> = grid
        .iter()
        .flatten()
        .filter(|c| !c.text.is_empty())
        .map(|c| c.start)
        .collect();
    let spans_other =
        |c: &PCell| starts.range(c.start + 1..c.start + c.span).next().is_some();
    let mut kset: BTreeSet<usize> = grid
        .iter()
        .flatten()
        .filter(|c| !c.text.is_empty() && !spans_other(c))
        .map(|c| c.start)
        .collect();
    let spanning_without_leaf: Vec<usize> = grid
        .iter()
        .flatten()
        .filter(|c| !c.text.is_empty() && spans_other(c))
        .filter(|c| kset.range(c.start..c.start + c.span).next().is_none())
        .map(|c| c.start)
        .collect();
    kset.extend(spanning_without_leaf);
    let kcols: Vec<usize> = kset.into_iter().collect();

    let row_cells = |r: &Vec<PCell>| -> Vec<String> {
        r.iter()
            .filter(|c| !c.text.is_empty())
            .map(|c| c.text.clone())
            .collect()
    };

    // Layout tables (one text column, or a single row of words): read as prose.
    let single_row = nonempty_rows.len() == 1;
    let any_numeric = grid
        .iter()
        .flatten()
        .any(|c| !c.text.is_empty() && is_numeric_cell(&c.text));
    if kcols.len() <= 1 || (single_row && !any_numeric) {
        return nonempty_rows
            .iter()
            .map(|&i| {
                let cells = row_cells(&grid[i]);
                RenderedRow {
                    source_row: i,
                    text: cells.join(" "),
                    cells,
                    layout: true,
                }
            })
            .collect();
    }

    // 4. Map cells onto kept columns.
    struct Placed {
        k0: usize,
        k1: usize,
        text: String,
        lines: Vec<String>,
    }
    let mut placed_rows: Vec<(usize, bool, Vec<Placed>)> = Vec::new();
    for &ri in &nonempty_rows {
        // Header-like rows (no figures other than years) may wrap onto
        // several lines; data rows always stay on one line.
        let header_like = grid[ri]
            .iter()
            .filter(|c| !c.text.is_empty())
            .all(|c| !is_numeric_cell(&c.text) || is_year_like(&c.text));
        let mut placed = Vec::new();
        for c in grid[ri].iter().filter(|c| !c.text.is_empty()) {
            let lo = kcols.partition_point(|&k| k < c.start);
            let hi = kcols.partition_point(|&k| k < c.start + c.span);
            let (k0, k1) = if lo >= hi {
                // No kept column inside this cell's range: attach to the nearest one.
                let k = lo.min(kcols.len() - 1);
                (k, k)
            } else {
                (lo, hi - 1)
            };
            let lines = if header_like {
                c.lines.clone()
            } else {
                vec![c.text.clone()]
            };
            placed.push(Placed {
                k0,
                k1,
                text: c.text.clone(),
                lines,
            });
        }
        placed.sort_by_key(|p| p.k0);
        // Two cells landing on the same column: join them.
        let mut merged: Vec<Placed> = Vec::new();
        for p in placed {
            if let Some(last) = merged.last_mut() {
                if p.k0 <= last.k1 {
                    last.text.push(' ');
                    last.text.push_str(&p.text);
                    last.lines = vec![last.text.clone()];
                    last.k1 = last.k1.max(p.k1);
                    continue;
                }
            }
            merged.push(p);
        }
        placed_rows.push((ri, header_like, merged));
    }

    // 5. Column widths and alignment.
    let ncols = kcols.len();
    let line_width = |p: &Placed| p.lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let mut numeric_votes = vec![(0usize, 0usize); ncols];
    let mut widths = vec![0usize; ncols];
    for (_, _, cells) in &placed_rows {
        for p in cells.iter().filter(|p| p.k0 == p.k1) {
            let w = line_width(p);
            let cap = if p.k0 == 0 { LABEL_WIDTH_CAP } else { TEXT_WIDTH_CAP };
            widths[p.k0] = widths[p.k0].max(w.min(cap));
            let v = &mut numeric_votes[p.k0];
            v.1 += 1;
            if is_numeric_cell(&p.text) || is_year_like(&p.text) {
                v.0 += 1;
            }
        }
    }
    for (_, _, cells) in &placed_rows {
        for p in cells.iter().filter(|p| p.k1 > p.k0) {
            let need = line_width(p);
            let have: usize =
                widths[p.k0..=p.k1].iter().sum::<usize>() + SEP.len() * (p.k1 - p.k0);
            if need > have && need <= 80 {
                // Spread the shortfall across the spanned columns so a wide
                // header doesn't leave one lopsided column under it.
                let n = p.k1 - p.k0 + 1;
                let deficit = need - have;
                for (j, w) in widths[p.k0..=p.k1].iter_mut().enumerate() {
                    *w += deficit / n + usize::from(j < deficit % n);
                }
            }
        }
    }
    let right_align: Vec<bool> = numeric_votes
        .iter()
        .enumerate()
        .map(|(k, (num, total))| k > 0 && *total > 0 && num * 2 >= *total)
        .collect();

    // 6. Render (multi-line header rows bottom-aligned, like the printed page).
    let mut out = Vec::with_capacity(placed_rows.len());
    for (ri, _header_like, cells) in placed_rows {
        let height = cells.iter().map(|p| p.lines.len().max(1)).max().unwrap_or(1);
        let mut lines_out: Vec<String> = Vec::with_capacity(height);
        for line in 0..height {
            let mut segs: Vec<String> = Vec::new();
            let mut k = 0usize;
            let mut ci = 0usize;
            while k < ncols {
                if ci < cells.len() && cells[ci].k0 == k {
                    let p = &cells[ci];
                    let offset = height - p.lines.len().max(1);
                    let txt = if line >= offset {
                        p.lines.get(line - offset).map(String::as_str).unwrap_or("")
                    } else {
                        ""
                    };
                    let width: usize =
                        widths[p.k0..=p.k1].iter().sum::<usize>() + SEP.len() * (p.k1 - p.k0);
                    let seg = if p.k1 > p.k0 {
                        pad_center(txt, width)
                    } else if right_align[k] {
                        pad_left(txt, width)
                    } else {
                        pad_right(txt, width)
                    };
                    segs.push(seg);
                    k = p.k1 + 1;
                    ci += 1;
                } else {
                    segs.push(" ".repeat(widths[k]));
                    k += 1;
                }
            }
            while segs.last().is_some_and(|s| s.trim().is_empty()) {
                segs.pop();
            }
            let text = segs.join(SEP).trim_end().to_string();
            if !text.trim().is_empty() {
                lines_out.push(text);
            }
        }
        out.push(RenderedRow {
            source_row: ri,
            text: lines_out.join("\n"),
            cells: cells.into_iter().map(|p| p.text).collect(),
            layout: false,
        });
    }
    out
}

fn pad_right(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let mut out = String::with_capacity(width);
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', width - n));
    out
}

fn pad_left(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let mut out = String::with_capacity(width);
    out.extend(std::iter::repeat_n(' ', width - n));
    out.push_str(s);
    out
}

fn pad_center(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let left = (width - n) / 2;
    let mut out = String::with_capacity(width);
    out.extend(std::iter::repeat_n(' ', left));
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', width - n - left));
    out
}

fn is_prefix_fragment(t: &str) -> bool {
    matches!(
        t,
        "$" | "€" | "£" | "¥" | "(" | "$(" | "($" | "US$" | "C$" | "A$" | "HK$" | "R$" | "₹" | "CHF"
            | "RMB" | "Ps."
    )
}

fn is_suffix_fragment(t: &str) -> bool {
    matches!(t, ")" | "%" | ")%" | "%)" | "%)%" | ")*" | "x" | "bps" | "pts" | "pp")
}

fn is_fragment(t: &str) -> bool {
    is_prefix_fragment(t) || is_suffix_fragment(t)
}

fn merge_fragments(row: &mut [PCell]) {
    let n = row.len();
    for i in 0..n {
        let t = row[i].text.trim().to_string();
        if t.is_empty() || !is_prefix_fragment(&t) {
            continue;
        }
        let Some(j) = (i + 1..n.min(i + 4)).find(|&j| !row[j].text.is_empty()) else {
            continue;
        };
        let next = row[j].text.trim_start();
        let starts_num = next
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '(' | '.' | '—' | '–' | '-' | '−'));
        if starts_num {
            row[j].text = format!("{t}{next}");
            row[j].lines = vec![row[j].text.clone()];
            row[i].text.clear();
            row[i].lines.clear();
        }
    }
    for i in 0..n {
        let t = row[i].text.trim().to_string();
        if t.is_empty() || !is_suffix_fragment(&t) {
            continue;
        }
        let Some(j) = (i.saturating_sub(3)..i).rev().find(|&j| !row[j].text.is_empty()) else {
            continue;
        };
        let prev = row[j].text.trim_end();
        let ends_num = prev
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, ')' | '%' | '—' | '–' | '-'));
        if ends_num {
            let joined = if t.chars().all(|c| c.is_ascii_alphabetic()) {
                format!("{prev} {t}")
            } else {
                format!("{prev}{t}")
            };
            row[j].text = joined;
            row[j].lines = vec![row[j].text.clone()];
            row[i].text.clear();
            row[i].lines.clear();
        }
    }
}

/// A table cell that holds a number (with optional currency, parens, % or dash).
pub(crate) fn is_numeric_cell(t: &str) -> bool {
    let s = t.trim();
    if s.is_empty() {
        return false;
    }
    if matches!(s, "—" | "–" | "-" | "−" | "— %" | "—%" | "n/a" | "N/A" | "NM" | "nm" | "*" | "—)") {
        return true;
    }
    let s = s
        .trim_start_matches(|c: char| {
            matches!(c, '$' | '€' | '£' | '¥' | '(' | '-' | '−' | '+' | ' ' | '₹') || c.is_ascii_uppercase()
        })
        .trim_end_matches(|c: char| {
            matches!(c, ')' | '%' | ' ' | 'x' | '*' | '†') || c.is_ascii_alphabetic()
        });
    !s.is_empty()
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | ' '))
}

fn is_year_like(t: &str) -> bool {
    let s = t.trim();
    s.len() == 4 && s.starts_with(['1', '2']) && s.chars().all(|c| c.is_ascii_digit())
}

// ───────────────────────────── furniture ─────────────────────────────

/// Whole text is a page marker: "12", "- 12 -", "Page 12", "F-7", "iii".
pub(crate) fn is_page_marker(t: &str) -> bool {
    let s = t
        .trim()
        .trim_matches(|c: char| matches!(c, '-' | '–' | '—' | '|' | ' ' | '.'));
    if s.is_empty() {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    let lower = lower.strip_prefix("page ").unwrap_or(&lower).trim();
    let lower = match lower.find(" of ") {
        Some(p) if lower[p + 4..].trim().chars().all(|c| c.is_ascii_digit()) => &lower[..p],
        _ => lower,
    };
    // Pages run 1-999; four digits would be a year ("October 31, 2025").
    if !lower.is_empty() && lower.len() <= 3 && lower.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if !lower.is_empty() && lower.len() <= 6 && lower.chars().all(|c| matches!(c, 'i' | 'v' | 'x' | 'l')) {
        return true;
    }
    // F-12, A-3, S-1 style financial-section page numbers.
    let b = lower.as_bytes();
    b.len() >= 3
        && b.len() <= 5
        && b[0].is_ascii_alphabetic()
        && b[1] == b'-'
        && b[2..].iter().all(|c| c.is_ascii_digit())
}

fn is_backlink(t: &str) -> bool {
    let s = t
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_ascii_lowercase();
    matches!(
        s.as_str(),
        "table of contents"
            | "back to table of contents"
            | "return to table of contents"
            | "back to contents"
            | "back to top"
            | "return to top"
            | "toc"
            | "index"
            | "table of contents (continued)"
            | "contents"
    )
}

fn furniture_key(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut prev_space = false;
    for c in t.chars() {
        let c = if c.is_ascii_digit() {
            '#'
        } else if c == '|' || c.is_whitespace() {
            ' '
        } else {
            c.to_ascii_lowercase()
        };
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn ends_with_page_number(b: &Block, small_tables: &std::collections::HashSet<u32>) -> bool {
    if b.text.chars().count() > 160 {
        return false;
    }
    match b.kind {
        // Footers that are tables are tiny tables of their own; the last row of
        // a real table that happens to end in a small number is not a footer.
        BlockKind::Row if !small_tables.contains(&b.table) => false,
        BlockKind::Row => {
            let first = b.cells.first().map(|s| s.as_str()).unwrap_or("");
            let last = b.cells.last().map(|s| s.as_str()).unwrap_or("");
            b.cells.len() <= 6 && (is_page_marker(last) || (b.cells.len() > 1 && is_page_marker(first)))
        }
        BlockKind::Para => {
            let toks: Vec<&str> = b.text.split_whitespace().collect();
            if toks.len() < 2 || !b.text.chars().any(|c| c.is_alphabetic()) {
                return false;
            }
            let last = toks[toks.len() - 1];
            let first = toks[0];
            (is_page_marker(last) && !last.contains(','))
                || (is_page_marker(first) && first.chars().all(|c| c.is_ascii_digit()) && toks.len() <= 12)
        }
        BlockKind::PageBreak => false,
    }
}

/// Flag page numbers, "Table of Contents" back-links, and running page
/// headers/footers next to page breaks. The first occurrence of a repeated
/// running header is kept: it usually doubles as the section's real heading.
pub(crate) fn mark_furniture(blocks: &mut [Block]) {
    for b in blocks.iter_mut() {
        if b.kind == BlockKind::PageBreak {
            continue;
        }
        let t = if b.kind == BlockKind::Row {
            b.cells.join(" ")
        } else {
            b.text.clone()
        };
        if is_page_marker(&t) || is_backlink(&t) {
            b.furniture = true;
        }
    }
    let breaks: Vec<usize> = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.kind == BlockKind::PageBreak)
        .map(|(i, _)| i)
        .collect();
    if breaks.is_empty() {
        return;
    }
    let mut rows_per_table: HashMap<u32, usize> = HashMap::new();
    for b in blocks.iter().filter(|b| b.kind == BlockKind::Row) {
        *rows_per_table.entry(b.table).or_insert(0) += 1;
    }
    let small_tables: std::collections::HashSet<u32> = rows_per_table
        .into_iter()
        .filter(|(_, n)| *n <= 2)
        .map(|(t, _)| t)
        .collect();
    // Up to two non-furniture blocks on one side of a page break.
    let near = |blocks: &[Block], at: usize, forward: bool| -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = at;
        while out.len() < 2 {
            let next = if forward {
                i += 1;
                if i >= blocks.len() {
                    break;
                }
                i
            } else {
                if i == 0 {
                    break;
                }
                i -= 1;
                i
            };
            let b = &blocks[next];
            if b.kind == BlockKind::PageBreak {
                break;
            }
            if b.furniture {
                continue;
            }
            out.push(next);
        }
        out
    };
    // A running header/footer is a short line that sits next to a page break on
    // a good share of pages (>= 15% of breaks, at least 3). A table caption
    // that happens to open two pages does not qualify.
    const RUNNING_MAX_CHARS: usize = 100;
    let threshold = 3usize.max(breaks.len() * 15 / 100);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for &pb in &breaks {
        let mut keys_here: std::collections::HashSet<String> = std::collections::HashSet::new();
        for i in near(blocks, pb, false)
            .into_iter()
            .chain(near(blocks, pb, true))
        {
            let b = &blocks[i];
            if b.text.chars().count() <= RUNNING_MAX_CHARS {
                keys_here.insert(furniture_key(&b.heading_text()));
            }
        }
        for k in keys_here {
            *counts.entry(k).or_insert(0) += 1;
        }
    }
    let is_running = |b: &Block, key: &str| {
        b.text.chars().count() <= RUNNING_MAX_CHARS
            && counts.get(key).copied().unwrap_or(0) >= threshold
    };
    // First occurrence of each running line anywhere in the document: kept.
    let mut first_idx: HashMap<String, usize> = HashMap::new();
    for (i, b) in blocks.iter().enumerate() {
        if b.kind == BlockKind::PageBreak || b.furniture {
            continue;
        }
        if b.text.chars().count() > RUNNING_MAX_CHARS {
            continue;
        }
        let key = furniture_key(&b.heading_text());
        if counts.get(&key).copied().unwrap_or(0) >= threshold {
            first_idx.entry(key).or_insert(i);
        }
    }
    let mut to_mark: Vec<usize> = Vec::new();
    for &pb in &breaks {
        for i in near(blocks, pb, false) {
            let b = &blocks[i];
            let key = furniture_key(&b.heading_text());
            if ends_with_page_number(b, &small_tables) {
                to_mark.push(i);
            } else if is_running(b, &key) {
                if first_idx.get(&key) != Some(&i) {
                    to_mark.push(i);
                }
            } else {
                break;
            }
        }
        for i in near(blocks, pb, true) {
            let b = &blocks[i];
            let key = furniture_key(&b.heading_text());
            if is_running(b, &key) {
                if first_idx.get(&key) != Some(&i) {
                    to_mark.push(i);
                }
            } else {
                break;
            }
        }
    }
    for i in to_mark {
        blocks[i].furniture = true;
    }
}

// ───────────────────────────── text utils ─────────────────────────────

pub(crate) fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        match c {
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{ad}' => continue,
            c if c.is_whitespace() || c == '\u{a0}' => {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            }
            c => {
                out.push(c);
                prev_space = false;
            }
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn strip_prefix_ns(s: &str) -> &str {
    s.rsplit(':').next().unwrap_or(s)
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

fn find_bytes(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

fn find_ascii_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn parse_attrs(src: &str) -> Vec<(String, String)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let ks = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let key = src[ks..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                let ve = b[vs..].iter().position(|&c| c == q).map(|p| vs + p).unwrap_or(b.len());
                out.push((key, src[vs..ve].to_string()));
                i = (ve + 1).min(b.len());
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                out.push((key, src[vs..i].to_string()));
            }
        } else if !key.is_empty() {
            out.push((key, String::new()));
        }
    }
    out
}

pub(crate) fn decode_entities(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let semi = tail.as_bytes().iter().take(12).position(|&b| b == b';');
        if let Some(semi) = semi {
            if let Some(c) = decode_entity(&tail[..semi]) {
                out.push(c);
                rest = &tail[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = tail;
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Windows-1252 code points that old filings write as numeric entities
/// (`&#146;` for a right quote); as Unicode they would be C1 control chars.
fn cp1252(code: u32) -> Option<char> {
    Some(match code {
        128 => '€',
        130 => '‚',
        132 => '„',
        133 => '…',
        134 => '†',
        135 => '‡',
        139 => '‹',
        145 => '‘',
        146 => '’',
        147 => '“',
        148 => '”',
        149 => '•',
        150 => '–',
        151 => '—',
        152 => '˜',
        153 => '™',
        155 => '›',
        _ => return None,
    })
}

fn decode_entity(ent: &str) -> Option<char> {
    if let Some(num) = ent.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        if (0x80..=0x9f).contains(&code) {
            return Some(cp1252(code).unwrap_or(' '));
        }
        return char::from_u32(code);
    }
    Some(match ent {
        "nbsp" => '\u{a0}',
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "rsquo" => '’',
        "lsquo" => '‘',
        "rdquo" => '”',
        "ldquo" => '“',
        "ndash" => '–',
        "mdash" => '—',
        "minus" => '−',
        "bull" => '•',
        "middot" => '·',
        "hellip" => '…',
        "trade" => '™',
        "reg" => '®',
        "copy" => '©',
        "sect" => '§',
        "para" => '¶',
        "deg" => '°',
        "cent" => '¢',
        "pound" => '£',
        "euro" => '€',
        "yen" => '¥',
        "times" => '×',
        "divide" => '÷',
        "plusmn" => '±',
        "frac12" => '½',
        "frac14" => '¼',
        "frac34" => '¾',
        "sup1" => '¹',
        "sup2" => '²',
        "sup3" => '³',
        "eacute" => 'é',
        "egrave" => 'è',
        "aacute" => 'á',
        "agrave" => 'à',
        "oacute" => 'ó',
        "uacute" => 'ú',
        "iacute" => 'í',
        "ntilde" => 'ñ',
        "ccedil" => 'ç',
        "uuml" => 'ü',
        "ouml" => 'ö',
        "auml" => 'ä',
        "Eacute" => 'É',
        "thinsp" => '\u{2009}',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "shy" => '\u{ad}',
        "zwsp" => '\u{200b}',
        "dagger" => '†',
        "Dagger" => '‡',
        "laquo" => '«',
        "raquo" => '»',
        "le" => '≤',
        "ge" => '≥',
        "ne" => '≠',
        "rarr" => '→',
        "larr" => '←',
        "micro" => 'µ',
        "ordm" => 'º',
        "acute" => '´',
        "check" => '✓',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dev harness: `ELI_FILING_HTML=/path/doc.htm cargo test -p eli-core --lib
    /// dump_rendered_filing -- --ignored --nocapture` prints the rendered blocks.
    #[test]
    #[ignore]
    fn dump_rendered_filing() {
        let Ok(path) = std::env::var("ELI_FILING_HTML") else {
            return;
        };
        let raw = std::fs::read_to_string(&path).expect("read filing");
        let t0 = std::time::Instant::now();
        let doc = parse_filing_html(&raw);
        eprintln!("parsed {} blocks, {} facts in {:?}", doc.blocks.len(), doc.facts.len(), t0.elapsed());
        for (i, b) in doc.blocks.iter().enumerate() {
            let tag = match (b.kind, b.furniture, b.layout) {
                (BlockKind::PageBreak, _, _) => "PB".to_string(),
                (_, true, _) => "FURN".to_string(),
                (BlockKind::Row, _, true) => format!("L{}", b.table),
                (BlockKind::Row, _, false) => format!("T{}", b.table),
                _ => "P".to_string(),
            };
            println!("{i}\t{tag}\t{}", b.text);
        }
    }

    #[test]
    fn decodes_entities_and_collapses_whitespace() {
        assert_eq!(decode_entities("A&amp;B&#160;C&#x2014;D&rsquo;"), "A&B\u{a0}C—D’");
        assert_eq!(collapse_ws("  a\u{a0}\u{a0} b\n\tc  "), "a b c");
        assert_eq!(decode_entities("AT&T and &unknown; stay"), "AT&T and &unknown; stay");
        assert_eq!(decode_entities("Registrant&#146;s &#150; 10-K"), "Registrant’s – 10-K");
    }

    #[test]
    fn renders_financial_table_with_merged_fragments_and_aligned_columns() {
        let html = r#"<table>
          <tr><td/><td/><td/><td/><td/><td/><td/></tr>
          <tr><td>&#160;</td><td colspan="3">Three Months Ended<br/>September 30,</td><td colspan="3">Nine Months Ended</td></tr>
          <tr><td/><td colspan="2">2025</td><td/><td colspan="2">2026</td><td/></tr>
          <tr><td>Net income</td><td>$</td><td>21,187</td><td/><td>$</td><td>20,685</td><td/></tr>
          <tr><td>Non-operating expense (income), net</td><td/><td>(10,112</td><td>)</td><td/><td>694</td><td/></tr>
          <tr><td>Margin</td><td/><td>15</td><td>%</td><td/><td>12</td><td>%</td></tr>
        </table>"#;
        let doc = parse_filing_html(html);
        let rows: Vec<&Block> = doc.blocks.iter().filter(|b| b.kind == BlockKind::Row).collect();
        assert_eq!(rows.len(), 5, "{:#?}", doc.blocks);
        let ni = rows.iter().find(|b| b.text.starts_with("Net income")).unwrap();
        assert_eq!(ni.cells, vec!["Net income", "$21,187", "$20,685"]);
        let neg = rows.iter().find(|b| b.text.starts_with("Non-operating")).unwrap();
        assert_eq!(neg.cells, vec!["Non-operating expense (income), net", "(10,112)", "694"]);
        let pct = rows.iter().find(|b| b.text.starts_with("Margin")).unwrap();
        assert_eq!(pct.cells, vec!["Margin", "15%", "12%"]);
        // Columns line up: every data row has its separators at the same offsets.
        let sep_positions = |s: &str| s.match_indices(" | ").map(|(i, _)| i).collect::<Vec<_>>();
        assert_eq!(sep_positions(&ni.text), sep_positions(&neg.text));
        assert_eq!(sep_positions(&ni.text), sep_positions(&pct.text));
        // Numbers are right-aligned under their year headers.
        let year_row = rows.iter().find(|b| b.cells == vec!["2025", "2026"]).unwrap();
        assert!(year_row.text.contains("2025"));
        assert!(ni.text.ends_with("$20,685"));
    }

    #[test]
    fn layout_tables_read_as_prose() {
        let html = r#"<table><tr><td colspan="3">Item&#160;1.</td><td colspan="3">Financial Statements</td></tr></table>"#;
        let doc = parse_filing_html(html);
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(doc.blocks[0].text, "Item 1. Financial Statements");
        assert!(doc.blocks[0].layout);
    }

    #[test]
    fn page_furniture_is_flagged() {
        let mut html = String::new();
        for (page, word) in ["alpha", "beta", "gamma", "delta"].iter().enumerate() {
            let page = page + 1;
            html.push_str(&format!(
                "<div><a href=\"#toc\">Table of Contents</a></div><div>ACME CORP</div><p>Body text about {word} that is real content.</p><div>Acme Corp | Q3 2026 Form 10-Q | {page}</div><hr style=\"page-break-after:always\"/>"
            ));
        }
        let doc = parse_filing_html(&html);
        let kept: Vec<&str> = doc
            .blocks
            .iter()
            .filter(|b| !b.furniture && b.kind != BlockKind::PageBreak)
            .map(|b| b.text.as_str())
            .collect();
        assert!(kept.iter().all(|t| !t.contains("Form 10-Q")), "{kept:?}");
        assert!(kept.iter().all(|t| *t != "Table of Contents"), "{kept:?}");
        // The running header's first occurrence survives; repeats are dropped.
        assert_eq!(kept.iter().filter(|t| **t == "ACME CORP").count(), 1, "{kept:?}");
        assert_eq!(kept.iter().filter(|t| t.starts_with("Body text")).count(), 4);
    }

    #[test]
    fn collects_inline_xbrl_facts_with_contexts_units_and_row_labels() {
        let html = r#"<html><body><div style="display:none"><ix:header><ix:hidden>
          <ix:nonNumeric name="dei:DocumentType" contextRef="c-1">10-Q</ix:nonNumeric>
          </ix:hidden><ix:resources>
          <xbrli:context id="c-1"><xbrli:entity><xbrli:identifier scheme="x">1</xbrli:identifier></xbrli:entity><xbrli:period><xbrli:startDate>2026-01-01</xbrli:startDate><xbrli:endDate>2026-09-30</xbrli:endDate></xbrli:period></xbrli:context>
          <xbrli:context id="c-2"><xbrli:entity><xbrli:identifier scheme="x">1</xbrli:identifier><xbrli:segment><xbrldi:explicitMember dimension="us-gaap:StatementBusinessSegmentsAxis">amzn:AWSSegmentMember</xbrldi:explicitMember></xbrli:segment></xbrli:entity><xbrli:period><xbrli:instant>2026-09-30</xbrli:instant></xbrli:period></xbrli:context>
          <xbrli:unit id="usd"><xbrli:measure>iso4217:USD</xbrli:measure></xbrli:unit>
          <xbrli:unit id="usdPerShare"><xbrli:divide><xbrli:unitNumerator><xbrli:measure>iso4217:USD</xbrli:measure></xbrli:unitNumerator><xbrli:unitDenominator><xbrli:measure>xbrli:shares</xbrli:measure></xbrli:unitDenominator></xbrli:divide></xbrli:unit>
          </ix:resources></ix:header></div>
          <p>Intro</p>
          <table><tr><td>Inventories</td><td>$</td><td><ix:nonFraction name="us-gaap:InventoryNet" contextRef="c-2" unitRef="usd" decimals="-6" scale="6" format="ixt:num-dot-decimal">14,541</ix:nonFraction></td></tr>
          <tr><td>Loss</td><td><ix:nonFraction name="us-gaap:NetIncomeLoss" contextRef="c-1" unitRef="usd" scale="6" sign="-">(1,234</ix:nonFraction></td><td>)</td></tr></table>
        </body></html>"#;
        let doc = parse_filing_html(html);
        assert!(doc.is_inline_xbrl);
        assert_eq!(doc.dei.get("dei:DocumentType").map(|v| v.0.as_str()), Some("10-Q"));
        assert_eq!(doc.units.get("usd").map(String::as_str), Some("USD"));
        assert_eq!(doc.units.get("usdPerShare").map(String::as_str), Some("USD/shares"));
        let c2 = doc.contexts.get("c-2").unwrap();
        assert_eq!(c2.instant.as_deref(), Some("2026-09-30"));
        assert_eq!(
            c2.dims,
            vec![("us-gaap:StatementBusinessSegmentsAxis".to_string(), "amzn:AWSSegmentMember".to_string())]
        );
        let inv = doc.facts.iter().find(|f| f.concept == "us-gaap:InventoryNet").unwrap();
        assert_eq!(inv.text.trim(), "14,541");
        assert_eq!(inv.scale, 6);
        assert_eq!(inv.row_label.as_deref(), Some("Inventories"));
        let row = &doc.blocks[inv.block];
        assert!(row.text.starts_with("Inventories"), "{:?}", row);
        let loss = doc.facts.iter().find(|f| f.concept == "us-gaap:NetIncomeLoss").unwrap();
        assert!(loss.negative);
        // The hidden dei block never reaches the rendered text.
        assert!(doc.blocks.iter().all(|b| !b.text.contains("10-Q")));
    }
}
