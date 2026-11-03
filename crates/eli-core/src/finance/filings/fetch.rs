use super::super::*;
use super::support::{sec_client, sec_fetch_submissions, sec_get_bytes, sec_lookup_cik};

pub async fn fetch_filings(req: FilingsRequest, cache_dir: &Path) -> Result<FilingsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    let used_default_forms = req.forms.iter().all(|s| s.trim().is_empty());
    let mut forms = req
        .forms
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase())
        .collect::<Vec<_>>();
    if forms.is_empty() {
        forms = vec!["8-K".to_string(), "10-K".to_string(), "10-Q".to_string()];
    }
    let forms_set: std::collections::HashSet<String> = forms.into_iter().collect();

    // Structured modes read one filing closely; default to the latest one.
    let structured = req.toc || req.xbrl || req.sections.iter().any(|s| !s.trim().is_empty());
    let diff_against = match req.diff_against.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => Some(normalize_accession(raw)?),
        None => None,
    };
    if (req.diff || diff_against.is_some()) && !req.sections.iter().any(|s| !s.trim().is_empty()) {
        return Err(Error::InvalidInput(
            "diff compares sections: pass section= as well, e.g. section=risk_factors,mda".to_string(),
        ));
    }
    let accession = match req.accession.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => Some(normalize_accession(raw)?),
        None => None,
    };
    let limit = if req.single_file || accession.is_some() {
        1
    } else {
        req.limit.unwrap_or(if structured { 1 } else { 5 }).clamp(1, 25)
    };
    let should_download = req.download
        || req.download_all
        || req.include_text
        || req.single_file
        || req.raw_text
        || req.press_release_text;

    let sec_dir = cache_dir.join("finance").join("sec");
    std::fs::create_dir_all(&sec_dir)?;

    let (cik_str, company_name) =
        sec_lookup_cik(&ticker, &sec_dir, req.user_agent.as_deref()).await?;
    let mut submissions =
        sec_fetch_submissions(&cik_str, &company_name, &sec_dir, req.user_agent.as_deref()).await?;
    if let Some(acc) = accession.as_deref() {
        // The submissions cache can be up to a day old; a just-filed accession
        // may be missing from it. Refetch once before falling back.
        let listed = submissions
            .filings
            .as_ref()
            .and_then(|f| f.recent.as_ref())
            .is_some_and(|r| r.accession_number.iter().any(|a| a == acc));
        if !listed {
            let cached = sec_dir
                .join("submissions")
                .join(format!("CIK{cik_str}.json"));
            let _ = std::fs::remove_file(&cached);
            submissions = sec_fetch_submissions(
                &cik_str,
                &company_name,
                &sec_dir,
                req.user_agent.as_deref(),
            )
            .await?;
        }
    }

    let recent = submissions
        .filings
        .as_ref()
        .and_then(|f| f.recent.as_ref())
        .ok_or_else(|| {
            Error::Provider(format!(
                "sec submissions missing recent filings for '{ticker}'"
            ))
        })?;

    let n = recent.form.len();
    let cik_num = submissions
        .cik
        .trim_start_matches('0')
        .parse::<u64>()
        .unwrap_or_else(|_| submissions.cik.parse::<u64>().unwrap_or(0));

    let mut out: Vec<FilingDoc> = Vec::new();
    let client = sec_client(req.user_agent.as_deref())?;

    let matching_indexes = |allowed_forms: &std::collections::HashSet<String>| {
        (0..n)
            .filter(|&i| {
                recent
                    .form
                    .get(i)
                    .map(|form| allowed_forms.contains(&form.to_ascii_uppercase()))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>()
    };

    let selected_indexes = if let Some(acc) = accession.as_deref() {
        (0..n)
            .filter(|&i| recent.accession_number.get(i).is_some_and(|a| a == acc))
            .collect::<Vec<_>>()
    } else {
        let mut selected = matching_indexes(&forms_set);
        if selected.is_empty() && used_default_forms {
            let foreign_forms = ["6-K", "20-F", "40-F"]
                .into_iter()
                .map(str::to_string)
                .collect::<std::collections::HashSet<_>>();
            selected = matching_indexes(&foreign_forms);
        }
        selected
    };

    let mut metas: Vec<FilingDoc> = Vec::new();
    for i in selected_indexes {
        let form = recent.form.get(i).cloned().unwrap_or_default();
        let accession = recent.accession_number.get(i).cloned().unwrap_or_default();
        let filing_date = recent.filing_date.get(i).cloned().unwrap_or_default();
        if accession.trim().is_empty() || filing_date.trim().is_empty() {
            continue;
        }

        let report_date = recent.report_date.as_ref().and_then(|v| v.get(i).cloned());
        let acceptance_datetime = recent
            .acceptance_date_time
            .as_ref()
            .and_then(|v| v.get(i).cloned());
        let items = recent.items.as_ref().and_then(|v| v.get(i).cloned());
        let size = recent.size.as_ref().and_then(|v| v.get(i).cloned());
        let primary_document = recent
            .primary_document
            .as_ref()
            .and_then(|v| v.get(i).cloned());
        let primary_doc_description = recent
            .primary_doc_description
            .as_ref()
            .and_then(|v| v.get(i).cloned());

        metas.push(new_filing_doc(
            cik_num,
            form,
            filing_date,
            report_date,
            accession,
            acceptance_datetime,
            items,
            primary_document,
            primary_doc_description,
            size,
        ));
        if metas.len() >= limit {
            break;
        }
    }
    if metas.is_empty() {
        if let Some(acc) = accession.as_deref() {
            // Older than the submissions "recent" window: read the filing's own header.
            metas.push(filing_doc_from_index_headers(&client, cache_dir, &ticker, cik_num, acc).await?);
        }
    }

    let history: Vec<PeriodicRef> = (0..n)
        .filter_map(|i| {
            let form = recent.form.get(i)?.to_ascii_uppercase();
            if !matches!(form.as_str(), "10-Q" | "10-K" | "20-F" | "40-F") {
                return None;
            }
            Some(PeriodicRef {
                form,
                accession: recent.accession_number.get(i)?.clone(),
                primary_document: recent.primary_document.as_ref()?.get(i)?.clone(),
                report_date: recent.report_date.as_ref()?.get(i)?.clone(),
                filing_date: recent.filing_date.get(i)?.clone(),
            })
        })
        .filter(|h| !h.primary_document.trim().is_empty() && !h.report_date.trim().is_empty())
        .collect();
    // Every filing with a primary document (report date may be blank), for
    // the base filing of a diff.
    let all_filings: Vec<PeriodicRef> = (0..n)
        .filter_map(|i| {
            Some(PeriodicRef {
                form: recent.form.get(i)?.to_ascii_uppercase(),
                accession: recent.accession_number.get(i)?.clone(),
                primary_document: recent.primary_document.as_ref()?.get(i)?.clone(),
                report_date: recent
                    .report_date
                    .as_ref()
                    .and_then(|r| r.get(i).cloned())
                    .unwrap_or_default(),
                filing_date: recent.filing_date.get(i)?.clone(),
            })
        })
        .filter(|h| !h.primary_document.trim().is_empty())
        .collect();
    // Earnings releases (8-Ks under Item 2.02), newest first, for the
    // previous quarter's release tables.
    let releases: Vec<ReleaseRef> = (0..n)
        .filter_map(|i| {
            let form = recent.form.get(i)?.to_ascii_uppercase();
            let items = recent.items.as_ref()?.get(i)?;
            if !form.starts_with("8-K") || !items.split(',').any(|it| it.trim() == "2.02") {
                return None;
            }
            Some(ReleaseRef {
                form,
                accession: recent.accession_number.get(i)?.clone(),
                filing_date: recent.filing_date.get(i)?.clone(),
            })
        })
        .collect();
    let fiscal_year_end = submissions.fiscal_year_end.as_deref().and_then(|s| {
        let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
        let (m, day) = (d.get(..2)?.parse::<u32>().ok()?, d.get(2..4)?.parse::<u32>().ok()?);
        ((1..=12).contains(&m) && (1..=31).contains(&day)).then_some((m, day))
    });

    let mut company_facts: Option<(Option<super::xbrl::CompanyFacts>, Option<String>)> = None;
    for mut doc in metas {
        let accession_nodash = doc.accession_number.replace('-', "");
        let base = format!(
            "https://www.sec.gov/Archives/edgar/data/{}/{}/",
            cik_num, accession_nodash
        );

        if should_download {
            let download = download_filing_files(
                &client,
                cache_dir,
                &ticker,
                &accession_nodash,
                &base,
                &doc.filing_index_url.clone().unwrap_or_default(),
                doc.primary_document.as_deref(),
                req.download_all,
                req.important_exhibits,
                req.single_file,
            )
            .await?;
            doc.download_dir = Some(download.download_dir);
            doc.primary_doc_path = download.primary_doc_path;
            doc.index_json_path = download.index_json_path;
            doc.downloaded_files = download.files;
        }

        if req.raw_text {
            attach_primary_raw_text(&client, &mut doc, req.max_chars).await?;
        }

        if req.press_release_text {
            attach_press_release_text(&mut doc, req.max_chars);
        }

        if structured {
            let ctx = StructureCtx {
                client: &client,
                cache_dir,
                sec_dir: &sec_dir,
                ticker: &ticker,
                cik_str: &cik_str,
                cik_num,
                accession_nodash: &accession_nodash,
                base: &base,
                history: &history,
                all_filings: &all_filings,
                diff_against: diff_against.as_deref(),
                releases: &releases,
                fiscal_year_end,
            };
            attach_structure(&ctx, &mut doc, &req, &mut company_facts).await?;
        }

        out.push(doc);
        if out.len() >= limit {
            break;
        }
    }

    Ok(FilingsResponse {
        ticker,
        cik: cik_str,
        company_name,
        generated_at: Utc::now(),
        filings: out,
    })
}

#[allow(clippy::too_many_arguments)]
fn new_filing_doc(
    cik_num: u64,
    form: String,
    filing_date: String,
    report_date: Option<String>,
    accession: String,
    acceptance_datetime: Option<String>,
    items: Option<String>,
    primary_document: Option<String>,
    primary_doc_description: Option<String>,
    size: Option<u64>,
) -> FilingDoc {
    let accession_nodash = accession.replace('-', "");
    let base = format!(
        "https://www.sec.gov/Archives/edgar/data/{}/{}/",
        cik_num, accession_nodash
    );
    let filing_index_url = format!("{base}index.json");
    let url = primary_document
        .as_ref()
        .filter(|s| !s.trim().is_empty())
        .map(|doc| format!("{base}{}", doc.trim_start_matches('/')));
    FilingDoc {
        form,
        filing_date,
        report_date,
        accession_number: accession,
        acceptance_datetime,
        items,
        primary_document,
        primary_doc_description,
        size,
        url,
        filing_index_url: Some(filing_index_url),
        download_dir: None,
        primary_doc_path: None,
        index_json_path: None,
        downloaded_files: Vec::new(),
        text_path: None,
        raw_text: None,
        raw_text_bytes: None,
        raw_text_truncated: None,
        press_release_path: None,
        press_release_text: None,
        text_excerpt: None,
        toc: None,
        sections: None,
        xbrl: None,
        structure_note: None,
    }
}

/// "0000320193-26-000024", "000032019326000024" -> "0000320193-26-000024".
fn normalize_accession(raw: &str) -> Result<String> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 18 {
        return Err(Error::InvalidInput(format!(
            "accession '{raw}' must be 18 digits, e.g. 0000320193-26-000024"
        )));
    }
    Ok(format!("{}-{}-{}", &digits[..10], &digits[10..12], &digits[12..]))
}

// ───────────────────── structured reading: TOC / sections / XBRL ─────────────────────

struct StructureCtx<'a> {
    client: &'a reqwest::Client,
    cache_dir: &'a Path,
    sec_dir: &'a Path,
    ticker: &'a str,
    cik_str: &'a str,
    cik_num: u64,
    accession_nodash: &'a str,
    base: &'a str,
    /// The company's recent 10-Q / 10-K filings (for the prior period's XBRL).
    history: &'a [PeriodicRef],
    /// Every recent filing, any form (the base of a section diff).
    all_filings: &'a [PeriodicRef],
    /// diff: compare against this accession instead of the previous filing.
    diff_against: Option<&'a str>,
    /// Its 8-K earnings releases, newest first (for the prior release's tables).
    releases: &'a [ReleaseRef],
    /// Fiscal year-end (month, day) from SEC submissions.
    fiscal_year_end: Option<(u32, u32)>,
}

#[derive(Clone, Debug)]
struct ReleaseRef {
    form: String,
    accession: String,
    filing_date: String,
}

#[derive(Clone, Debug)]
struct PeriodicRef {
    form: String,
    accession: String,
    primary_document: String,
    report_date: String,
    filing_date: String,
}

impl StructureCtx<'_> {
    fn filing_dir(&self) -> PathBuf {
        self.cache_dir
            .join("finance")
            .join("filings")
            .join(self.ticker)
            .join(self.accession_nodash)
    }
}

/// A file from the filing's EDGAR folder, from the local cache when present
/// (filings are immutable once accepted), else downloaded and cached.
async fn cached_archive_file(
    client: &reqwest::Client,
    base_url: &str,
    filing_dir: &Path,
    filename: &str,
) -> Result<Vec<u8>> {
    let Some(name) = sec_archive_filename(filename) else {
        return Err(Error::InvalidInput(format!("bad filing file name '{filename}'")));
    };
    let path = filing_dir.join(&name);
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 0 {
            return Ok(std::fs::read(&path)?);
        }
    }
    let bytes = sec_get_bytes(client, &archive_file_url(base_url, &name)).await?;
    std::fs::create_dir_all(filing_dir)?;
    std::fs::write(&path, &bytes)?;
    Ok(bytes)
}

fn parse_document(bytes: &[u8]) -> super::html::ParsedDoc {
    let raw = String::from_utf8_lossy(bytes);
    if super::html::looks_like_html(&raw) {
        super::html::parse_filing_html(&raw)
    } else {
        super::html::parse_plain_text(&raw)
    }
}

/// Clean readable text of a whole document (tables aligned, page furniture out).
fn clean_document_text(bytes: &[u8]) -> String {
    let parsed = parse_document(bytes);
    let text = parsed.text_range(0, parsed.blocks.len());
    if text.trim().is_empty() {
        strip_html_to_text(&String::from_utf8_lossy(bytes))
    } else {
        text
    }
}

#[derive(Clone, Debug)]
struct ExhibitDoc {
    /// "EX-99.1"
    ex_type: String,
    filename: String,
    description: Option<String>,
    bytes: Option<u64>,
}

impl ExhibitDoc {
    /// "99.1"
    fn number(&self) -> String {
        self.ex_type
            .trim()
            .to_ascii_uppercase()
            .trim_start_matches("EX-")
            .trim_start_matches("EX")
            .to_string()
            .to_ascii_lowercase()
    }

    /// "ex99.1"
    fn id(&self) -> String {
        format!("ex{}", self.number())
    }
}

#[derive(Default)]
struct IndexHeader {
    documents: Vec<(String, String, Option<String>)>,
    form: Option<String>,
    filed: Option<String>,
    period: Option<String>,
    acceptance: Option<String>,
}

/// Parse `{accession}-index-headers.html`: the SGML header of every document
/// in the filing (TYPE / FILENAME / DESCRIPTION) plus filing-level dates.
fn parse_index_headers(raw: &str) -> IndexHeader {
    let decoded = super::html::decode_entities(raw);
    let mut h = IndexHeader::default();
    let mut cur_type: Option<String> = None;
    let mut cur_file: Option<String> = None;
    let mut cur_desc: Option<String> = None;
    let flush = |t: &mut Option<String>, f: &mut Option<String>, d: &mut Option<String>, h: &mut IndexHeader| {
        if let (Some(ty), Some(fi)) = (t.take(), f.take()) {
            h.documents.push((ty, fi, d.take()));
        }
        *d = None;
    };
    for line in decoded.lines() {
        let l = line.trim();
        let tag_value = |tag: &str| -> Option<String> {
            l.strip_prefix(tag)
                .map(super::html::collapse_ws)
                .filter(|v| !v.is_empty())
        };
        if l.starts_with("<DOCUMENT>") {
            flush(&mut cur_type, &mut cur_file, &mut cur_desc, &mut h);
        } else if let Some(v) = tag_value("<TYPE>") {
            cur_type = Some(v);
        } else if let Some(v) = tag_value("<FILENAME>") {
            cur_file = Some(v);
        } else if let Some(v) = tag_value("<DESCRIPTION>") {
            cur_desc = Some(v);
        } else if let Some(v) = tag_value("CONFORMED SUBMISSION TYPE:") {
            h.form.get_or_insert(v);
        } else if let Some(v) = tag_value("FILED AS OF DATE:") {
            h.filed.get_or_insert(v);
        } else if let Some(v) = tag_value("CONFORMED PERIOD OF REPORT:") {
            h.period.get_or_insert(v);
        } else if let Some(v) = tag_value("<ACCEPTANCE-DATETIME>") {
            h.acceptance.get_or_insert(v);
        }
    }
    flush(&mut cur_type, &mut cur_file, &mut cur_desc, &mut h);
    h
}

fn yyyymmdd_to_iso(s: &str) -> String {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).take(8).collect();
    if d.len() == 8 {
        format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8])
    } else {
        s.to_string()
    }
}

async fn fetch_index_header(
    client: &reqwest::Client,
    base: &str,
    filing_dir: &Path,
    accession: &str,
) -> Result<IndexHeader> {
    let name = format!("{accession}-index-headers.html");
    match cached_archive_file(client, base, filing_dir, &name).await {
        Ok(bytes) => {
            let h = parse_index_headers(&String::from_utf8_lossy(&bytes));
            if !h.documents.is_empty() {
                return Ok(h);
            }
        }
        Err(e) => {
            // Older filings don't serve the SGML header page; the human index
            // page lists the same documents.
            let page = format!("{accession}-index.htm");
            let bytes = cached_archive_file(client, base, filing_dir, &page)
                .await
                .map_err(|_| e)?;
            return Ok(parse_index_page(&String::from_utf8_lossy(&bytes)));
        }
    }
    let page = format!("{accession}-index.htm");
    let bytes = cached_archive_file(client, base, filing_dir, &page).await?;
    Ok(parse_index_page(&String::from_utf8_lossy(&bytes)))
}

/// EDGAR's `{accession}-index.htm`: the document table (Seq | Description |
/// Document | Type | Size) and the filing / period / acceptance dates.
fn parse_index_page(raw: &str) -> IndexHeader {
    let doc = super::html::parse_filing_html(raw);
    let mut h = IndexHeader::default();
    let looks_like_file = |c: &str| {
        let l = c.to_ascii_lowercase();
        !l.contains(' ')
            && [".htm", ".html", ".txt", ".xml", ".pdf", ".jpg", ".gif", ".png", ".xsd", ".zip", ".xls", ".xlsx", ".json"]
                .iter()
                .any(|ext| l.ends_with(ext))
    };
    for b in doc.blocks.iter().filter(|b| b.kind == super::html::BlockKind::Row) {
        // Cells: seq, [description], document (maybe "name.htm iXBRL"), type, [size].
        let cells: Vec<String> = b
            .cells
            .iter()
            .map(|c| c.trim_end_matches(" iXBRL").trim().to_string())
            .collect();
        let Some(di) = cells.iter().position(|c| looks_like_file(c)) else {
            continue;
        };
        let Some(ty) = cells.get(di + 1).filter(|t| {
            !t.is_empty() && t.len() <= 20 && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '/' | ' '))
        }) else {
            continue;
        };
        let desc = if di >= 2 { Some(cells[1..di].join(" ")) } else { None };
        h.documents.push((ty.to_ascii_uppercase(), cells[di].clone(), desc.filter(|d| !d.is_empty())));
    }
    let after = |label: &str| -> Option<String> {
        let i = doc.blocks.iter().position(|b| b.text.trim().eq_ignore_ascii_case(label))?;
        doc.blocks.get(i + 1).map(|b| b.text.trim().to_string())
    };
    h.form = h.documents.first().map(|d| d.0.clone());
    h.filed = after("Filing Date");
    h.period = after("Period of Report");
    h.acceptance = after("Accepted");
    h
}

/// Filing metadata for an accession outside the submissions "recent" window.
async fn filing_doc_from_index_headers(
    client: &reqwest::Client,
    cache_dir: &Path,
    ticker: &str,
    cik_num: u64,
    accession: &str,
) -> Result<FilingDoc> {
    let nodash = accession.replace('-', "");
    let base = format!("https://www.sec.gov/Archives/edgar/data/{cik_num}/{nodash}/");
    let dir = cache_dir.join("finance").join("filings").join(ticker).join(&nodash);
    let h = fetch_index_header(client, &base, &dir, accession)
        .await
        .map_err(|e| {
            Error::InvalidInput(format!(
                "accession {accession} not found for {ticker} (CIK {cik_num}): {e}"
            ))
        })?;
    let form = h.form.clone().unwrap_or_default();
    let primary = h
        .documents
        .iter()
        .find(|(t, _, _)| t.eq_ignore_ascii_case(&form))
        .or_else(|| h.documents.first())
        .map(|(_, f, _)| f.clone());
    Ok(new_filing_doc(
        cik_num,
        form,
        h.filed.as_deref().map(yyyymmdd_to_iso).unwrap_or_default(),
        h.period.as_deref().map(yyyymmdd_to_iso),
        accession.to_string(),
        h.acceptance.clone(),
        None,
        primary,
        None,
        None,
    ))
}

async fn filing_exhibits(ctx: &StructureCtx<'_>, doc: &FilingDoc) -> Vec<ExhibitDoc> {
    exhibits_at(ctx.client, ctx.base, &ctx.filing_dir(), &doc.accession_number).await
}

/// The exhibits of any filing (by its EDGAR folder and cache dir).
async fn exhibits_at(client: &reqwest::Client, base: &str, dir: &Path, accession: &str) -> Vec<ExhibitDoc> {
    let Ok(h) = fetch_index_header(client, base, dir, accession).await else {
        return Vec::new();
    };
    // Raw sizes from index.json (cached by the regular download path too).
    let mut sizes: HashMap<String, u64> = HashMap::new();
    if let Ok(bytes) = cached_archive_file(client, base, dir, "index.json").await {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(items) = v.pointer("/directory/item").and_then(|i| i.as_array()) {
                for it in items {
                    let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let size = it
                        .get("size")
                        .and_then(|s| s.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| s.as_u64()));
                    if let Some(sz) = size {
                        sizes.insert(name.to_string(), sz);
                    }
                }
            }
        }
    }
    h.documents
        .into_iter()
        .filter(|(t, f, _)| {
            let t = t.to_ascii_uppercase();
            let f = f.to_ascii_lowercase();
            t.starts_with("EX-")
                && !t.starts_with("EX-101")
                && !t.starts_with("EX-104")
                && (f.ends_with(".htm") || f.ends_with(".html") || f.ends_with(".txt"))
        })
        .map(|(t, f, d)| ExhibitDoc {
            bytes: sizes.get(&f).copied(),
            ex_type: t.to_ascii_uppercase(),
            filename: f,
            description: d,
        })
        .collect()
}

/// "EX-99.1 Press release issued by Apple Inc. on November 2, 2026." — the
/// description from the filing's own exhibit index when it lists one.
fn exhibit_title(ex: &ExhibitDoc, primary: &super::html::ParsedDoc) -> String {
    let num = ex.number();
    let from_index = primary.blocks.iter().find_map(|b| {
        if b.kind != super::html::BlockKind::Row || b.cells.len() < 2 {
            return None;
        }
        let first = b.cells[0].trim().trim_end_matches('*').trim().to_ascii_lowercase();
        if first == num || first == format!("ex-{num}") || first == format!("exhibit {num}") {
            let d = b.cells[1..].join(" ");
            let d = d.trim();
            if d.chars().any(|c| c.is_alphabetic()) && d.len() <= 300 {
                return Some(d.to_string());
            }
        }
        None
    });
    let desc = from_index.or_else(|| {
        ex.description
            .clone()
            .filter(|d| !d.eq_ignore_ascii_case(&ex.ex_type))
    });
    match desc {
        Some(d) => format!("{} {}", ex.ex_type, d),
        None => ex.ex_type.clone(),
    }
}

/// Requested exhibit, for queries like "ex99.1", "EX-99.1", "exhibit:99.1",
/// "exhibit 99", "press_release". Bare numbers ("99.1") only when `bare_ok`.
fn match_exhibit<'a>(exhibits: &'a [ExhibitDoc], q: &str, bare_ok: bool) -> Option<&'a ExhibitDoc> {
    let ql = q.trim().to_ascii_lowercase();
    if matches!(
        ql.as_str(),
        "press_release" | "press release" | "pressrelease" | "earnings_release" | "release"
    ) {
        return exhibits.iter().find(|e| e.number().starts_with("99"));
    }
    let (prefixed, rest) = if let Some(r) = ql.strip_prefix("exhibit") {
        (true, r)
    } else if let Some(r) = ql.strip_prefix("ex") {
        (true, r)
    } else {
        (false, ql.as_str())
    };
    if !prefixed && !bare_ok {
        return None;
    }
    let num = rest.trim_start_matches([':', '-', '_', ' ', '.']).trim();
    if num.is_empty() || !num.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    exhibits
        .iter()
        .find(|e| e.number() == num)
        .or_else(|| {
            if num.contains('.') {
                None
            } else {
                exhibits.iter().find(|e| e.number().starts_with(&format!("{num}.")))
            }
        })
}

fn file_mtime_fresh(path: &Path, max_age_secs: u64) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() <= max_age_secs)
}

const COMPANYFACTS_TTL_SECS: u64 = 60 * 60 * 12;

/// SEC companyfacts for prior-period comparisons; cached, refreshed when the
/// cache predates the filing being read.
async fn load_company_facts(
    client: &reqwest::Client,
    sec_dir: &Path,
    cik_str: &str,
    accession: &str,
) -> (Option<super::xbrl::CompanyFacts>, Option<String>) {
    let dir = sec_dir.join("companyfacts");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("CIK{cik_str}.json"));
    let cached = if file_mtime_fresh(&path, COMPANYFACTS_TTL_SECS) {
        std::fs::read(&path).ok()
    } else {
        None
    };
    let has_accession = |b: &[u8]| {
        let needle = accession.as_bytes();
        b.windows(needle.len()).any(|w| w == needle)
    };
    let mut note = None;
    let bytes = match cached {
        Some(b) if has_accession(&b) => Some(b),
        stale => {
            let url = format!("https://data.sec.gov/api/xbrl/companyfacts/CIK{cik_str}.json");
            match sec_get_bytes(client, &url).await {
                Ok(b) => {
                    let _ = std::fs::write(&path, &b);
                    Some(b)
                }
                Err(e) => {
                    note = Some(format!(
                        "SEC companyfacts unavailable ({e}); comparisons limited to this filing's own columns."
                    ));
                    stale.or_else(|| std::fs::read(&path).ok())
                }
            }
        }
    };
    let parsed = bytes.as_deref().and_then(super::xbrl::CompanyFacts::parse);
    if parsed.is_none() && note.is_none() {
        note = Some("SEC companyfacts unreadable; comparisons limited to this filing's own columns.".to_string());
    }
    (parsed, note)
}

fn split_section_queries(sections: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in sections {
        for part in s.split(',') {
            let p = part.trim();
            if !p.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(p)) {
                out.push(p.to_string());
            }
        }
    }
    out
}

fn cap_text(text: String, max_chars: Option<usize>) -> (String, Option<bool>) {
    match max_chars {
        Some(limit) if text.chars().count() > limit => (text.chars().take(limit).collect(), Some(true)),
        _ => (text, None),
    }
}

async fn attach_structure(
    ctx: &StructureCtx<'_>,
    doc: &mut FilingDoc,
    req: &FilingsRequest,
    company_facts: &mut Option<(Option<super::xbrl::CompanyFacts>, Option<String>)>,
) -> Result<()> {
    use super::release::detect_release_tables;
    use super::sections::{detect_structure, form_family, resolve_by_subheading, resolve_section, FormFamily};

    let primary_name = doc
        .primary_document
        .clone()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| {
            Error::Provider(format!(
                "filing {} has no primary document to read",
                doc.accession_number
            ))
        })?;
    let dir = ctx.filing_dir();
    let bytes = cached_archive_file(ctx.client, ctx.base, &dir, &primary_name).await?;
    let parsed = parse_document(&bytes);
    let structure = detect_structure(&parsed, &doc.form);
    let family = form_family(&doc.form);
    let queries = split_section_queries(&req.sections);

    let is_8k = family == FormFamily::EightK;
    // Exhibits: listed for every form; read (for sizes) on 8-Ks / 6-Ks.
    let want_exhibits = req.toc
        || (is_8k && (req.xbrl || !queries.is_empty()))
        || queries.iter().any(|q| {
            let l = q.to_ascii_lowercase();
            l.starts_with("ex") || l.contains("press") || l.contains("release") || l.chars().next().is_some_and(|c| c.is_ascii_digit())
        });
    let exhibits = if want_exhibits {
        filing_exhibits(ctx, doc).await
    } else {
        Vec::new()
    };
    // An 8-K / 6-K's EX-99 exhibits (the earnings release) read once: their
    // tables are listed in the TOC, reachable by section, and read by xbrl.
    let mut release_docs: Vec<ReleaseDoc> = Vec::new();
    if is_8k && (req.toc || req.xbrl || !queries.is_empty()) {
        for ex in exhibits.iter().take(12).filter(|e| e.number().starts_with("99")) {
            if let Ok(b) = cached_archive_file(ctx.client, ctx.base, &dir, &ex.filename).await {
                let parsed = parse_document(&b);
                let tables = detect_release_tables(&parsed, &ex.id());
                release_docs.push(ReleaseDoc { ex: ex.clone(), parsed, tables });
            }
        }
    }

    if req.toc {
        let mut toc: Vec<FilingTocEntry> = structure
            .sections
            .iter()
            .map(|s| FilingTocEntry {
                id: s.id.clone(),
                title: s.title.clone(),
                kind: s.kind.as_str().to_string(),
                level: s.kind.level(),
                parent: s.parent.clone(),
                chars: Some(
                    structure
                        .chars
                        .get(&s.id)
                        .copied()
                        .unwrap_or_else(|| parsed.chars_range(s.start, s.end)),
                ),
                bytes: None,
            })
            .collect();
        // Every exhibit is read so its size is in the same unit as the sections:
        // characters of clean text that section=<id> returns. Raw bytes only
        // when the fetch fails.
        for ex in exhibits.iter().take(12) {
            let chars = cached_archive_file(ctx.client, ctx.base, &dir, &ex.filename)
                .await
                .ok()
                .map(|b| clean_document_text(&b).chars().count());
            toc.push(FilingTocEntry {
                id: ex.id(),
                title: exhibit_title(ex, &parsed),
                kind: "exhibit".to_string(),
                level: 1,
                parent: None,
                bytes: if chars.is_none() { ex.bytes } else { None },
                chars,
            });
            // Its statements and titled tables, each its own section.
            if let Some(rd) = release_docs.iter().find(|r| r.ex.filename == ex.filename) {
                for t in rd.tables.iter().filter(|t| t.listed) {
                    toc.push(FilingTocEntry {
                        id: t.id.clone(),
                        title: t.title.clone(),
                        kind: if t.statement.is_some() { "statement" } else { "subsection" }.to_string(),
                        level: 2,
                        parent: Some(ex.id()),
                        chars: Some(rd.parsed.chars_range(t.start, t.end)),
                        bytes: None,
                    });
                }
            }
        }
        let how = match structure.method {
            "item_headings" | "item_headings+toc" => "Sections come from the filing's Item headings.",
            "toc_titles" => "This filer drops 'Item N.' headings; sections were matched from its own table of contents and mapped to the standard items.",
            "title_match" => "No Item headings or usable table of contents; sections were matched by standard item titles.",
            _ => "No section headings recognized; section=document returns the full clean text.",
        };
        let release_note = release_docs
            .iter()
            .find(|r| r.tables.iter().any(|t| t.statement.is_some()))
            .map(|r| {
                let ex = r.ex.id();
                format!(
                    " The financial statements inside the earnings release are listed under {ex} ({ex}:income_statement, {ex}:balance_sheet, {ex}:cash_flow, ...); section=cash_flow reaches them, and xbrl=true returns the release's tables as data, as printed, before the 10-Q / 10-K XBRL exists."
                )
            })
            .unwrap_or_default();
        doc.structure_note = Some(format!(
            "{how} Get exact text with section=<id> (comma-separate several; chars is the size you will get back), passing accession={} to stay on this filing. Aliases: mda, risk_factors, business, income_statement, balance_sheet, cash_flow, equity, notes (all notes), note:<number or name>, auditor_report, exhibits, ex99.1, document (everything). xbrl=true returns the numbers with qoq/yoy changes.{release_note}",
            doc.accession_number
        ));
        doc.toc = Some(toc);
    }

    if !queries.is_empty() {
        let mut outs: Vec<FilingSectionText> = Vec::new();
        let diff_mode = req.diff || ctx.diff_against.is_some();
        let mut diff_bases: Vec<DiffBaseDoc> = Vec::new();
        for q in &queries {
            let ql = q.to_ascii_lowercase();
            // "ex99.1:cash_flow": a table inside an exhibit.
            if let Some((pre, rest)) = ql.split_once(':') {
                if let Some(ex) = match_exhibit(&exhibits, pre, false) {
                    let rd = release_docs.iter().find(|r| r.ex.filename == ex.filename);
                    match rd.and_then(|r| find_release_table(r, rest).map(|t| (r, t))) {
                        Some((r, t)) => {
                            let text = r.parsed.text_range(t.start, t.end);
                            let full_chars = text.chars().count();
                            let (text, truncated) = cap_text(text, req.max_chars);
                            outs.push(FilingSectionText {
                                requested: q.clone(),
                                diff: None,
                                id: Some(t.id.clone()),
                                title: Some(t.title.clone()),
                                chars: full_chars,
                                text,
                                truncated,
                                error: None,
                            });
                        }
                        None => {
                            let ids: Vec<String> = rd
                                .map(|r| r.tables.iter().filter(|t| t.listed).map(|t| t.id.clone()).collect())
                                .unwrap_or_default();
                            outs.push(FilingSectionText {
                                requested: q.clone(),
                                diff: None,
                                id: None,
                                title: None,
                                chars: 0,
                                text: String::new(),
                                truncated: None,
                                error: Some(if ids.is_empty() {
                                    format!("no tables recognized inside {}; section={} returns its full text", ex.id(), ex.id())
                                } else {
                                    format!("no table '{rest}' inside {}. Available ids: {}", ex.id(), ids.join(", "))
                                }),
                            });
                        }
                    }
                    continue;
                }
            }
            let whole = matches!(ql.as_str(), "document" | "full" | "all" | "full_text" | "everything");
            let exhibit = if ql.starts_with("ex") || ql.contains("press") || ql.contains("release") {
                match_exhibit(&exhibits, q, false)
            } else {
                None
            };
            let section = if whole || exhibit.is_some() {
                None
            } else {
                resolve_section(&structure, q).or_else(|| resolve_by_subheading(&structure, &parsed, q))
            };
            let exhibit = exhibit.or_else(|| {
                if !whole && section.is_none() {
                    match_exhibit(&exhibits, q, true)
                } else {
                    None
                }
            });
            // A statement the 8-K itself doesn't carry: the one in its
            // earnings release.
            let release_hit = if !whole && section.is_none() && exhibit.is_none() {
                release_fallback(&release_docs, q)
            } else {
                None
            };
            if diff_mode {
                if whole || section.is_some() {
                    let (id, title, chars) = match section {
                        Some(s) => (s.id.clone(), s.title.clone(), parsed.chars_range(s.start, s.end)),
                        None => (
                            "document".to_string(),
                            "Full document".to_string(),
                            parsed.chars_range(0, parsed.blocks.len()),
                        ),
                    };
                    let result = section_diff(ctx, doc, &parsed, q, section, &mut diff_bases).await;
                    outs.push(FilingSectionText {
                        requested: q.clone(),
                        id: Some(id),
                        title: Some(title),
                        chars,
                        text: String::new(),
                        truncated: None,
                        error: result.as_ref().err().cloned(),
                        diff: result.ok(),
                    });
                    continue;
                }
                if exhibit.is_some() || release_hit.is_some() {
                    outs.push(FilingSectionText {
                        requested: q.clone(),
                        diff: None,
                        id: None,
                        title: None,
                        chars: 0,
                        text: String::new(),
                        truncated: None,
                        error: Some(format!(
                            "diff compares sections of the filing's main document (items, notes, subsections); '{q}' is an exhibit. Call without diff for its text."
                        )),
                    });
                    continue;
                }
            }
            let (id, title, text) = if whole {
                (
                    Some("document".to_string()),
                    Some("Full document".to_string()),
                    parsed.text_range(0, parsed.blocks.len()),
                )
            } else if let Some(s) = section {
                (Some(s.id.clone()), Some(s.title.clone()), parsed.text_range(s.start, s.end))
            } else if let Some(ex) = exhibit {
                match cached_archive_file(ctx.client, ctx.base, &dir, &ex.filename).await {
                    Ok(b) => (Some(ex.id()), Some(exhibit_title(ex, &parsed)), clean_document_text(&b)),
                    Err(e) => {
                        outs.push(FilingSectionText {
                            requested: q.clone(),
                            diff: None,
                            id: Some(ex.id()),
                            title: None,
                            chars: 0,
                            text: String::new(),
                            truncated: None,
                            error: Some(format!("could not fetch exhibit {}: {e}", ex.filename)),
                        });
                        continue;
                    }
                }
            } else if let Some((r, t)) = release_hit {
                (Some(t.id.clone()), Some(t.title.clone()), r.parsed.text_range(t.start, t.end))
            } else {
                let mut available: Vec<String> = structure.sections.iter().map(|s| s.id.clone()).collect();
                available.extend(exhibits.iter().map(|e| e.id()));
                available.extend(release_docs.iter().flat_map(|r| r.tables.iter().filter(|t| t.listed).map(|t| t.id.clone())));
                outs.push(FilingSectionText {
                    requested: q.clone(),
                    diff: None,
                    id: None,
                    title: None,
                    chars: 0,
                    text: String::new(),
                    truncated: None,
                    error: Some(format!(
                        "no section matches '{q}'. Available ids: {}",
                        available.join(", ")
                    )),
                });
                continue;
            };
            let full_chars = text.chars().count();
            let (text, truncated) = cap_text(text, req.max_chars);
            outs.push(FilingSectionText {
                requested: q.clone(),
                diff: None,
                id,
                title,
                chars: full_chars,
                text,
                truncated,
                error: None,
            });
        }
        doc.sections = Some(outs);
    }

    if req.xbrl {
        let mut facts = super::xbrl::extract_facts(&parsed);
        if is_8k && facts.is_empty() {
            // An earnings 8-K: the press release's tables as data.
            if let Some(x) = release_xbrl(ctx, doc, &release_docs, req).await {
                doc.xbrl = Some(x);
                return Ok(());
            }
        }
        let periodic = matches!(family, FormFamily::TenK | FormFamily::TenQ | FormFamily::TwentyF);
        let mut extra_note: Option<String> = None;
        if (!facts.is_empty() || periodic) && company_facts.is_none() {
            *company_facts = Some(load_company_facts(ctx.client, ctx.sec_dir, ctx.cik_str, &doc.accession_number).await);
        }
        if facts.is_empty() && periodic && !parsed.is_inline_xbrl {
            // Pre-inline-XBRL filing: fall back to what SEC extracted for this accession.
            if let Some((Some(cf), _)) = company_facts.as_ref() {
                facts = cf.facts_for_accession(&doc.accession_number);
                if !facts.is_empty() {
                    extra_note = Some("This document carries no inline XBRL; facts are SEC companyfacts entries for this accession (non-dimensional, no statement placement).".to_string());
                }
            }
        }
        let mut period = super::xbrl::period_info(&parsed, &facts);
        if let Some(p) = period.as_mut() {
            if p.document_type.is_none() {
                p.document_type = Some(doc.form.clone());
            }
        }
        let (cf, cf_note) = match company_facts.as_ref() {
            Some((cf, note)) => (cf.as_ref(), note.clone().or(extra_note)),
            None => (None, extra_note),
        };
        // The previous 10-Q / 10-K: quarter-ago values for every concept,
        // including company extensions and segment dimensions.
        let mut prior_facts: Vec<super::xbrl::Fact> = Vec::new();
        let mut prior_label = String::new();
        if periodic && parsed.is_inline_xbrl {
            let current_report = doc.report_date.clone().unwrap_or_default();
            let prior = ctx
                .history
                .iter()
                .filter(|h| h.accession != doc.accession_number)
                .filter(|h| h.filing_date.as_str() <= doc.filing_date.as_str())
                .filter(|h| current_report.is_empty() || h.report_date.as_str() < current_report.as_str())
                .max_by(|a, b| a.report_date.cmp(&b.report_date).then(a.filing_date.cmp(&b.filing_date)));
            if let Some(h) = prior {
                let nodash = h.accession.replace('-', "");
                let pbase = format!("https://www.sec.gov/Archives/edgar/data/{}/{}/", ctx.cik_num, nodash);
                let pdir = ctx
                    .cache_dir
                    .join("finance")
                    .join("filings")
                    .join(ctx.ticker)
                    .join(&nodash);
                if let Ok(b) = cached_archive_file(ctx.client, &pbase, &pdir, &h.primary_document).await {
                    let pdoc = parse_document(&b);
                    prior_facts = super::xbrl::extract_facts(&pdoc);
                    prior_label = format!("{} {}", h.form, h.filing_date);
                }
            }
        }
        let prior = if prior_facts.is_empty() {
            None
        } else {
            Some(super::xbrl::PriorFiling {
                facts: &prior_facts,
                label: prior_label,
            })
        };
        let opts = super::xbrl::XbrlOptions {
            concepts: req
                .xbrl_concepts
                .iter()
                .flat_map(|c| c.split(','))
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
            min_change_pct: req.xbrl_min_change_pct.unwrap_or(25.0).max(0.0),
        };
        doc.xbrl = Some(super::xbrl::build_xbrl(
            &parsed,
            &structure,
            &facts,
            period.as_ref(),
            prior.as_ref(),
            cf,
            &doc.filing_date,
            &opts,
            cf_note,
        ));
    }
    Ok(())
}

/// An earlier filing loaded as the base of a section diff.
struct DiffBaseDoc {
    filing: PeriodicRef,
    parsed: super::html::ParsedDoc,
    structure: super::sections::Structure,
}

/// The base filing's index in `cache`, loading it on first use.
async fn load_diff_base(
    ctx: &StructureCtx<'_>,
    filing: &PeriodicRef,
    cache: &mut Vec<DiffBaseDoc>,
) -> std::result::Result<usize, String> {
    if let Some(i) = cache.iter().position(|b| b.filing.accession == filing.accession) {
        return Ok(i);
    }
    let nodash = filing.accession.replace('-', "");
    let pbase = format!("https://www.sec.gov/Archives/edgar/data/{}/{}/", ctx.cik_num, nodash);
    let pdir = ctx.cache_dir.join("finance").join("filings").join(ctx.ticker).join(&nodash);
    let bytes = cached_archive_file(ctx.client, &pbase, &pdir, &filing.primary_document)
        .await
        .map_err(|e| format!("could not fetch the base filing {} ({}): {e}", filing.accession, filing.form))?;
    let parsed = parse_document(&bytes);
    let structure = super::sections::detect_structure(&parsed, &filing.form);
    cache.push(DiffBaseDoc { filing: filing.clone(), parsed, structure });
    Ok(cache.len() - 1)
}

/// The filing a diff compares against by default: the previous original
/// (not amended) filing of the same form family, by report date.
fn previous_filing<'a>(ctx: &StructureCtx<'a>, doc: &FilingDoc, family_of: &str) -> Option<&'a PeriodicRef> {
    use super::sections::{form_family, FormFamily};
    let fam = form_family(family_of);
    let report = doc.report_date.clone().unwrap_or_default();
    ctx.all_filings
        .iter()
        .filter(|h| h.accession != doc.accession_number && !h.form.contains("/A"))
        .filter(|h| match fam {
            FormFamily::Other => h.form.eq_ignore_ascii_case(family_of),
            f => form_family(&h.form) == f,
        })
        .filter(|h| h.filing_date.as_str() <= doc.filing_date.as_str())
        .filter(|h| {
            if report.is_empty() || h.report_date.is_empty() {
                h.filing_date.as_str() < doc.filing_date.as_str()
            } else {
                h.report_date.as_str() < report.as_str()
            }
        })
        .max_by(|a, b| a.report_date.cmp(&b.report_date).then(a.filing_date.cmp(&b.filing_date)))
}

/// "Note 7 - Debt" -> "debt": a note's topic, since note numbers shift
/// between filings.
fn note_topic(title: &str) -> String {
    let k = super::sections::title_key(title);
    let mut words: Vec<&str> = k.split(' ').collect();
    if words.first() == Some(&"note") {
        words.remove(0);
    }
    if words.first().is_some_and(|w| w.chars().all(|c| c.is_ascii_digit())) {
        words.remove(0);
    }
    words.join(" ")
}

/// The same section in the base filing: items and statements by id (ids are
/// canonical per form), notes and subsections by title.
fn match_base_section<'a>(
    base: &'a super::sections::Structure,
    cur: &super::sections::Section,
    query: &str,
) -> Option<&'a super::sections::Section> {
    use super::sections::{resolve_section, SectionKind};
    match cur.kind {
        SectionKind::Note | SectionKind::Subsection => {
            let topic = note_topic(&cur.title);
            base.sections
                .iter()
                .find(|s| s.kind == cur.kind && !topic.is_empty() && note_topic(&s.title) == topic)
                .or_else(|| {
                    base.sections
                        .iter()
                        .find(|s| s.id == cur.id && super::sections::title_key(&s.title) == super::sections::title_key(&cur.title))
                })
        }
        _ => base
            .sections
            .iter()
            .find(|s| s.id == cur.id)
            .or_else(|| resolve_section(base, query).filter(|s| s.kind == cur.kind)),
    }
}

/// Compare one section (None: the whole document) against the same section
/// of the base filing.
async fn section_diff(
    ctx: &StructureCtx<'_>,
    doc: &FilingDoc,
    parsed: &super::html::ParsedDoc,
    query: &str,
    cur: Option<&super::sections::Section>,
    cache: &mut Vec<DiffBaseDoc>,
) -> std::result::Result<FilingSectionDiff, String> {
    use super::diff::{diff_paragraphs, section_paragraphs};
    use super::sections::{form_family, resolve_section, FormFamily};

    let (cstart, cend) = cur.map(|s| (s.start, s.end)).unwrap_or((0, parsed.blocks.len()));
    let cur_paras = section_paragraphs(parsed, cstart, cend);

    // The base filing: as asked, else the previous one of the same form.
    let explicit: Option<PeriodicRef> = match ctx.diff_against {
        Some(acc) => Some(match ctx.all_filings.iter().find(|h| h.accession == acc) {
            Some(h) => h.clone(),
            None => {
                let d = filing_doc_from_index_headers(ctx.client, ctx.cache_dir, ctx.ticker, ctx.cik_num, acc)
                    .await
                    .map_err(|e| e.to_string())?;
                PeriodicRef {
                    form: d.form.to_ascii_uppercase(),
                    accession: d.accession_number.clone(),
                    primary_document: d.primary_document.clone().unwrap_or_default(),
                    report_date: d.report_date.clone().unwrap_or_default(),
                    filing_date: d.filing_date.clone(),
                }
            }
        }),
        None => None,
    };
    if explicit.as_ref().is_some_and(|h| h.accession == doc.accession_number) {
        return Err("diff_against is this same filing; pass an earlier accession".to_string());
    }

    // 10-Q risk factors update the 10-K's. Compare against the 10-K when it
    // is newer than the previous 10-Q (the first quarter after year end), or
    // when this section only points back at the annual report and restates
    // what changed.
    let mut ten_k_fallback: Option<(usize, usize)> = None;
    let mut ten_k_newer = false;
    let tenq_risk = explicit.is_none() && form_family(&doc.form) == FormFamily::TenQ && cur.is_some_and(|s| s.id == "part2_item1a");
    if tenq_risk {
        let k_date = previous_filing(ctx, doc, "10-K").map(|k| k.filing_date.clone());
        let q_date = previous_filing(ctx, doc, &doc.form).map(|q| q.filing_date.clone());
        ten_k_newer = match (k_date, q_date) {
            (Some(k), Some(q)) => k > q,
            (Some(_), None) => true,
            _ => false,
        };
    }
    if tenq_risk {
        let cur_chars = parsed.chars_range(cstart, cend);
        let cur_text = parsed.text_range(cstart, cend).to_ascii_lowercase();
        let cur_text = cur_text.split_whitespace().collect::<Vec<_>>().join(" ");
        let refers_to_annual = cur_text.contains("form 10-k") || cur_text.contains("annual report");
        if refers_to_annual {
            if let Some(k) = previous_filing(ctx, doc, "10-K") {
                let k = k.clone();
                let bi = load_diff_base(ctx, &k, cache).await?;
                if let Some(ks) = resolve_section(&cache[bi].structure, "risk_factors") {
                    let k_chars = cache[bi].parsed.chars_range(ks.start, ks.end);
                    if cur_chars * 2 < k_chars {
                        ten_k_fallback = Some((cur_chars, k_chars));
                    }
                }
            }
        }
    }

    let base_ref: PeriodicRef = match (&explicit, ten_k_fallback) {
        (Some(h), _) => h.clone(),
        (None, Some(_)) => previous_filing(ctx, doc, "10-K").cloned().ok_or("no earlier 10-K found")?,
        (None, None) if ten_k_newer => previous_filing(ctx, doc, "10-K").cloned().ok_or("no earlier 10-K found")?,
        (None, None) => previous_filing(ctx, doc, &doc.form).cloned().ok_or_else(|| {
            format!(
                "no earlier {} filing found to compare against; pass diff_against=<accession>",
                doc.form
            )
        })?,
    };
    if base_ref.primary_document.trim().is_empty() {
        return Err(format!("base filing {} has no primary document", base_ref.accession));
    }
    let bi = load_diff_base(ctx, &base_ref, cache).await?;
    let base = &cache[bi];

    let base_section = match cur {
        None => None,
        Some(c) => {
            let found = if ten_k_fallback.is_some() || ten_k_newer {
                resolve_section(&base.structure, "risk_factors")
            } else {
                match_base_section(&base.structure, c, query)
                    // 10-Q vs a 10-K (or back), asked for explicitly: by alias.
                    .or_else(|| resolve_section(&base.structure, query).filter(|_| query.chars().any(|ch| ch.is_alphabetic())))
            };
            match found {
                Some(s) => Some(s),
                None => {
                    return Err(format!(
                        "the base filing ({} filed {}, {}) has no section matching {} ({}). Its sections: {}",
                        base.filing.form,
                        base.filing.filing_date,
                        base.filing.accession,
                        c.id,
                        c.title,
                        base.structure.sections.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")
                    ))
                }
            }
        }
    };
    let (bstart, bend) = base_section.map(|s| (s.start, s.end)).unwrap_or((0, base.parsed.blocks.len()));
    let base_paras = section_paragraphs(&base.parsed, bstart, bend);

    let mut diff = diff_paragraphs(&cur_paras, &base_paras, ten_k_fallback.is_none());
    diff.base = FilingDiffBase {
        form: base.filing.form.clone(),
        filing_date: base.filing.filing_date.clone(),
        report_date: Some(base.filing.report_date.clone()).filter(|r| !r.is_empty()),
        accession_number: base.filing.accession.clone(),
        section_id: base_section.map(|s| s.id.clone()).unwrap_or_else(|| "document".to_string()),
        section_title: base_section.map(|s| s.title.clone()).unwrap_or_else(|| "Full document".to_string()),
    };
    if let Some((cur_chars, k_chars)) = ten_k_fallback {
        diff.note = Some(format!(
            "This 10-Q's risk factors ({cur_chars} chars) refer to the annual report and restate only part of it; compared against the {} filed {} ({k_chars} chars) instead of the previous 10-Q. 10-K paragraphs it does not restate are counted in not_restated, not listed as removed. diff_against=<previous 10-Q accession> compares quarter to quarter.",
            base.filing.form, base.filing.filing_date
        ));
    } else if ten_k_newer {
        diff.note = Some(format!(
            "Compared against the {} filed {}: it came after the previous 10-Q, so its risk factors are the latest version this 10-Q updates.",
            base.filing.form, base.filing.filing_date
        ));
    } else if explicit.is_none() && form_family(&doc.form) == FormFamily::TenQ {
        // The first 10-Q after year end: the previous 10-Q came before the 10-K.
        if let Some(k) = previous_filing(ctx, doc, "10-K").filter(|k| k.filing_date > base.filing.filing_date) {
            diff.note = Some(format!(
                "The previous 10-Q predates the 10-K filed {} (accession {}); year-to-date discussion and items revised in the 10-K show up as changes. diff_against={} compares with the 10-K instead.",
                k.filing_date, k.accession, k.accession
            ));
        }
    }
    if diff.note.is_none() && (cur_paras.is_empty() || base_paras.is_empty()) {
        diff.note = Some("One side has no prose paragraphs (tables are not compared; xbrl=true compares the numbers).".to_string());
    }
    diff.chars = serde_json::to_string(&diff).map(|j| j.chars().count()).unwrap_or(0);
    Ok(diff)
}

/// An EX-99 exhibit of an 8-K / 6-K, parsed, with the tables found in it.
struct ReleaseDoc {
    ex: ExhibitDoc,
    parsed: super::html::ParsedDoc,
    tables: Vec<super::release::ReleaseTable>,
}

/// A table inside one exhibit, for the part after "ex99.1:" ("cash_flow",
/// "cf", "segment_information").
fn find_release_table<'a>(rd: &'a ReleaseDoc, rest: &str) -> Option<&'a super::release::ReleaseTable> {
    let rest = rest.trim().to_ascii_lowercase();
    let full = format!("{}:{rest}", rd.ex.id());
    let listed = || rd.tables.iter().filter(|t| t.listed);
    if let Some(t) = listed().find(|t| t.id == full) {
        return Some(t);
    }
    let norm = rest.replace([' ', '-'], "_");
    if let Some(kind) = super::sections::statement_alias(&norm) {
        return listed().find(|t| t.statement == Some(kind));
    }
    let qk = super::sections::title_key(&rest);
    if qk.len() < 3 {
        return None;
    }
    listed().find(|t| super::sections::title_key(&t.title).contains(&qk))
}

/// `section=cash_flow` (or another statement alias) on an 8-K whose own
/// document has no statements: the statement in its earnings release.
fn release_fallback<'a>(docs: &'a [ReleaseDoc], q: &str) -> Option<(&'a ReleaseDoc, &'a super::release::ReleaseTable)> {
    let norm = q.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    if let Some(kind) = super::sections::statement_alias(&norm) {
        return docs
            .iter()
            .find_map(|r| r.tables.iter().find(|t| t.listed && t.statement == Some(kind)).map(|t| (r, t)));
    }
    let qk = super::sections::title_key(q);
    if qk.len() < 4 {
        return None;
    }
    docs.iter().find_map(|r| {
        r.tables
            .iter()
            .find(|t| t.listed && super::sections::title_key(&t.title).contains(&qk))
            .map(|t| (r, t))
    })
}

/// xbrl=true on an earnings 8-K / 6-K: the release's tables in the XBRL
/// shape, with quarter-over-quarter values from the previous release. None
/// when no EX-99 exhibit carries financial statements.
async fn release_xbrl(
    ctx: &StructureCtx<'_>,
    doc: &FilingDoc,
    release_docs: &[ReleaseDoc],
    req: &FilingsRequest,
) -> Option<FilingXbrl> {
    use super::release::{build_release_xbrl, detect_release_tables, extract_release, PriorRelease, ReleaseData, ReleaseOptions};
    let rd = release_docs.iter().find(|r| r.tables.iter().any(|t| t.statement.is_some()))?;
    let cur = extract_release(&rd.parsed, &rd.tables, &rd.ex.ex_type, ctx.fiscal_year_end);
    let has_rows = cur.rows.iter().any(|r| r.statement.is_some());
    if !has_rows {
        let ids: Vec<String> = rd.tables.iter().filter(|t| t.statement.is_some()).map(|t| t.id.clone()).collect();
        let mut notes = vec![format!(
            "The release's statements ({}) were found, but their column headers could not be read as periods with confidence, so no numbers are returned as data. Read them as text with section=<id>.",
            ids.join(", ")
        )];
        notes.extend(cur.notes.iter().cloned());
        return Some(FilingXbrl {
            source: format!("press release {} tables", rd.ex.ex_type),
            document_type: Some(doc.form.clone()),
            period_end: None,
            fiscal_year: None,
            fiscal_period: None,
            facts_in_filing: cur.values,
            selection: "none".to_string(),
            returned: 0,
            facts: Vec::new(),
            notes,
        });
    }

    // The previous earnings release: the latest earlier 8-K under Item 2.02
    // whose statements end about a quarter before this one. (Statements
    // headed only by quarter labels compare within the release.)
    let target = cur.period_end.map(|end| end - chrono::Duration::days(91));
    let mut prior: Option<(ReleaseData, String)> = None;
    let mut checked: Vec<String> = Vec::new();
    let is_6k = doc.form.to_ascii_uppercase().starts_with("6-K");
    if let (false, Some(target)) = (is_6k, target) {
        for r in ctx
            .releases
            .iter()
            .filter(|r| r.accession != doc.accession_number && r.filing_date.as_str() < doc.filing_date.as_str())
            .take(3)
        {
            let nodash = r.accession.replace('-', "");
            let pbase = format!("https://www.sec.gov/Archives/edgar/data/{}/{}/", ctx.cik_num, nodash);
            let pdir = ctx.cache_dir.join("finance").join("filings").join(ctx.ticker).join(&nodash);
            let label = format!("{} {}", r.form, r.filing_date);
            let mut found: Option<ReleaseData> = None;
            for ex in exhibits_at(ctx.client, &pbase, &pdir, &r.accession)
                .await
                .iter()
                .filter(|e| e.number().starts_with("99"))
                .take(6)
            {
                let Ok(b) = cached_archive_file(ctx.client, &pbase, &pdir, &ex.filename).await else {
                    continue;
                };
                let pd = parse_document(&b);
                let tables = detect_release_tables(&pd, &ex.id());
                if tables.iter().any(|t| t.statement.is_some()) {
                    found = Some(extract_release(&pd, &tables, &ex.ex_type, ctx.fiscal_year_end));
                    break;
                }
            }
            match found {
                Some(d) if d.period_end.is_some_and(|pe| (pe - target).num_days().abs() <= 15) => {
                    prior = Some((d, label));
                    break;
                }
                Some(d) => checked.push(format!(
                    "{label} (period {})",
                    d.period_end.map(|p| p.to_string()).unwrap_or_else(|| "unreadable".to_string())
                )),
                None => checked.push(format!("{label} (no statement tables)")),
            }
        }
    }
    let prior_note = if prior.is_some() {
        None
    } else if is_6k {
        Some("a 6-K carries no item codes, so the previous release is not identified".to_string())
    } else if target.is_none() {
        Some("the statements print quarter labels, not dates, so the previous release is not matched".to_string())
    } else if checked.is_empty() {
        Some("no earlier 8-K under Item 2.02 in the company's recent filings".to_string())
    } else {
        Some(format!(
            "no previous release covering {} ({} checked)",
            target.map(|t| t.to_string()).unwrap_or_default(),
            checked.join(", ")
        ))
    };
    let prior_ref = prior.as_ref().map(|(d, l)| PriorRelease { data: d, label: l.clone() });
    let opts = ReleaseOptions {
        concepts: req
            .xbrl_concepts
            .iter()
            .flat_map(|c| c.split(','))
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect(),
        min_change_pct: req.xbrl_min_change_pct.unwrap_or(25.0).max(0.0),
        fye: ctx.fiscal_year_end,
        prior_note,
    };
    Some(build_release_xbrl(&cur, prior_ref.as_ref(), &opts, &doc.form))
}

struct FilingDownloadResult {
    download_dir: String,
    primary_doc_path: Option<String>,
    index_json_path: Option<String>,
    files: Vec<FilingDownload>,
}

async fn attach_primary_raw_text(
    client: &reqwest::Client,
    doc: &mut FilingDoc,
    max_chars: Option<usize>,
) -> Result<()> {
    let bytes = if let Some(path) = doc.primary_doc_path.as_deref() {
        std::fs::read(path)?
    } else if let Some(url) = doc.url.as_deref() {
        sec_get_bytes(client, url).await?
    } else {
        return Err(Error::Provider(format!(
            "raw_text requested but filing {} has no primary document URL",
            doc.accession_number
        )));
    };

    let raw = String::from_utf8_lossy(&bytes);
    // SEC "primary documents" for 10-Q/10-K/20-F are iXBRL/HTML — returning the raw
    // markup is useless for grepping footnotes (leases-not-commenced, RPO, purchase
    // obligations, VIE). Detect HTML/iXBRL and strip to clean text first, so max_chars
    // caps the *readable* text rather than the markup. (Same stripper the
    // press_release_text path already uses on these docs.)
    let head: String = raw.chars().take(2048).collect();
    let head_lower = head.to_ascii_lowercase();
    let looks_html = head.contains("xmlns:ix=")
        || head.contains("<ix:")
        || head_lower.contains("<html")
        || head_lower.contains("<!doctype")
        || (head.contains("<?xml") && head_lower.contains("<body"));
    // Readable text: paragraphs on their own lines, tables as aligned rows,
    // page numbers / running headers / TOC back-links dropped.
    let cleaned = if looks_html {
        clean_document_text(&bytes)
    } else {
        raw.into_owned()
    };
    let total_chars = cleaned.chars().count();
    let (text, truncated) = match max_chars {
        Some(limit) if total_chars > limit => {
            (cleaned.chars().take(limit).collect::<String>(), true)
        }
        _ => (cleaned, false),
    };

    doc.raw_text = Some(text);
    doc.raw_text_bytes = Some(bytes.len() as u64);
    doc.raw_text_truncated = Some(truncated);
    Ok(())
}

/// Substantive filing text — for 8-Ks the press-release exhibit (EX-99.1 etc.),
/// for 10-Q / 10-K / 20-F the body of the periodic filing itself. The XBRL cover
/// wrapper that SEC calls the "primary document" is bypassed for 8-Ks; for
/// periodic filings the primary document IS the body and is what we return.
///
/// Heuristic for 8-Ks: prefer filenames containing "press", "pr.htm", "ex99",
/// "ex-99", "991", "earnings", "results"; reject SOX certs / index / cover.
/// If nothing scores, falls back to the largest non-cert HTML in the dir.
fn attach_press_release_text(doc: &mut FilingDoc, max_chars: Option<usize>) {
    let dir = match doc.download_dir.as_deref() {
        Some(d) => d,
        None => return,
    };
    let primary_base = doc
        .primary_document
        .as_deref()
        .map(|p| p.to_lowercase())
        .unwrap_or_default();
    // What we extract depends on the form type:
    //   - 8-K with item 2.02: the press-release exhibit (EX-99.1 etc.)
    //   - 10-Q / 10-K / 20-F / 40-F / 6-K: the primary document body (XBRL-stripped)
    //   - Other 8-Ks (governance / debt / etc.): no clean "substantive text" → None.
    let form_upper = doc.form.to_uppercase();
    let items_str = doc.items.as_deref().unwrap_or("");
    let has_results_item = items_str.split(',').any(|s| s.trim() == "2.02");
    let is_periodic = matches!(
        form_upper.as_str(),
        "10-Q" | "10-K" | "10-K/A" | "10-Q/A" | "20-F" | "20-F/A" | "40-F" | "6-K"
    );
    if is_periodic {
        if let Some(primary_path) = doc.primary_doc_path.as_deref() {
            if let Ok(html) = std::fs::read(primary_path) {
                let parsed = clean_document_text(&html);
                let text = match max_chars {
                    Some(limit) if parsed.chars().count() > limit => {
                        parsed.chars().take(limit).collect()
                    }
                    _ => parsed,
                };
                doc.press_release_path = Some(primary_path.to_string());
                doc.press_release_text = Some(text);
                return;
            }
        }
        return;
    }
    if form_upper != "8-K" || !has_results_item {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut scored: Vec<(i64, std::path::PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let low = fname.to_lowercase();
        if !(low.ends_with(".htm") || low.ends_with(".html")) {
            continue;
        }
        // Exclude the primary doc (8-K XBRL cover) and known non-press files.
        if low == primary_base {
            continue;
        }
        // SOX cert exhibits use exhibit31x / exhibit32x / ex31, ex32, ex311, ex321, etc.
        // Match any filename containing "ex" followed by 31 or 32 or 23 with optional digits.
        let is_cert = {
            let chars: Vec<char> = low.chars().collect();
            let mut found = false;
            let mut i = 0;
            while i + 3 < chars.len() {
                if chars[i] == 'e' && chars[i + 1] == 'x' {
                    let rest_start = if chars[i + 2] == '-' { i + 3 } else { i + 2 };
                    if rest_start + 1 < chars.len()
                        && chars[rest_start] == '3'
                        && (chars[rest_start + 1] == '1' || chars[rest_start + 1] == '2')
                    {
                        found = true;
                        break;
                    }
                    if rest_start + 1 < chars.len()
                        && chars[rest_start] == '2'
                        && chars[rest_start + 1] == '3'
                    {
                        found = true;
                        break;
                    }
                }
                i += 1;
            }
            found
        };
        if is_cert {
            continue;
        }
        let exclude = ["index", "headers", "filingsummary", "exhibit10"];
        if exclude.iter().any(|k| low.contains(k)) {
            continue;
        }
        // r1.htm, r2.htm etc are XBRL viewer reports
        if low.starts_with('r')
            && low.len() <= 8
            && low[1..low.len() - 4].chars().all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let mut score: i64 = 0;
        for (kw, pts) in [
            ("pressrelease", 5_000_000_i64),
            ("press", 4_000_000),
            ("ex991pr", 4_500_000),
            ("ex991press", 4_500_000),
            ("ex99-1", 3_000_000),
            ("ex-99.1", 3_000_000),
            ("ex991", 2_500_000),
            ("ex99.1", 3_000_000),
            ("99-1", 1_500_000),
            ("pr.htm", 3_000_000),
            ("earnings", 1_500_000),
            ("results", 800_000),
        ] {
            if low.contains(kw) {
                score += pts;
            }
        }
        score += (bytes as i64).min(1_000_000);
        scored.push((score, path, bytes));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let (_score, path, _bytes) = match scored.into_iter().next() {
        Some(t) => t,
        None => return,
    };
    let html = match std::fs::read(&path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let parsed = clean_document_text(&html);
    let (text, _truncated) = if let Some(limit) = max_chars {
        if parsed.chars().count() > limit {
            (parsed.chars().take(limit).collect::<String>(), true)
        } else {
            (parsed, false)
        }
    } else {
        (parsed, false)
    };
    doc.press_release_path = Some(path.display().to_string());
    doc.press_release_text = Some(text);
}

/// Minimal HTML → text: strips <script>/<style>/XBRL-noise blocks, drops all other
/// tags, decodes common entities, collapses whitespace. Good enough for SEC earnings
/// press releases AND 10-Q / 10-K bodies (which embed inline XBRL).
///
/// XBRL stripping removes the metadata blocks (`<ix:hidden>`, `<ix:references>`,
/// `<ix:resources>`, `<ix:header>`) at the top of inline-XBRL filings while keeping
/// `<ix:nonNumeric>` and `<ix:nonFraction>` element content (those carry the
/// reported values via their text nodes and would be lost otherwise).
fn strip_html_to_text(html: &str) -> String {
    // Remove script/style sections.
    let no_script = strip_block(html, "<script", "</script>");
    let no_style = strip_block(&no_script, "<style", "</style>");
    // Remove inline XBRL metadata blocks.
    let no_ix_hidden = strip_block(&no_style, "<ix:hidden", "</ix:hidden>");
    let no_ix_refs = strip_block(&no_ix_hidden, "<ix:references", "</ix:references>");
    let no_ix_res = strip_block(&no_ix_refs, "<ix:resources", "</ix:resources>");
    let no_style = strip_block(&no_ix_res, "<ix:header", "</ix:header>");
    // Drop tags. Emit a space at every tag boundary so that adjacent text fragments
    // separated only by <td>X</td><td>Y</td>-style markup stay token-separated.
    let mut out = String::with_capacity(no_style.len());
    let mut in_tag = false;
    for c in no_style.chars() {
        match c {
            '<' => {
                if !in_tag {
                    out.push(' ');
                }
                in_tag = true;
            }
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // Decode common entities.
    let mut decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#58;", ":")
        .replace("&#8217;", "'")
        .replace("&#8216;", "'")
        .replace("&#8220;", "\"")
        .replace("&#8221;", "\"")
        .replace("&#8211;", "-")
        .replace("&#8212;", "-")
        .replace("&#8213;", "-")
        .replace("&#8226;", "•")
        .replace("&bull;", "•")
        .replace("&#176;", "°")
        .replace("&#8364;", "€")
        .replace("&#163;", "£")
        .replace("&#165;", "¥");
    // Numeric-entity fallback for the long tail (decimal codepoints). char-level walk so
    // we never split a multibyte UTF-8 grapheme.
    let mut result = String::with_capacity(decoded.len());
    let mut rest = decoded.as_str();
    loop {
        match rest.find("&#") {
            None => {
                result.push_str(rest);
                break;
            }
            Some(idx) => {
                result.push_str(&rest[..idx]);
                let tail = &rest[idx + 2..];
                if let Some(sc) = tail.find(';') {
                    let digits = &tail[..sc];
                    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(n) = digits.parse::<u32>() {
                            if let Some(c) = char::from_u32(n) {
                                result.push(c);
                                rest = &tail[sc + 1..];
                                continue;
                            }
                        }
                    }
                }
                // not a valid numeric entity — emit '&#' literally and keep walking
                result.push_str("&#");
                rest = tail;
            }
        }
    }
    decoded = result;
    // Collapse whitespace.
    let mut prev_space = false;
    let mut collapsed = String::with_capacity(decoded.len());
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            collapsed.push(c);
            prev_space = false;
        }
    }
    collapsed.trim().to_string()
}

fn strip_block(s: &str, open: &str, close: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    while let Some(start_rel) = lower[cursor..].find(open) {
        let start = cursor + start_rel;
        out.push_str(&s[cursor..start]);
        if let Some(end_rel) = lower[start..].find(close) {
            cursor = start + end_rel + close.len();
        } else {
            cursor = s.len();
            break;
        }
    }
    out.push_str(&s[cursor..]);
    out
}

async fn download_filing_files(
    client: &reqwest::Client,
    cache_dir: &Path,
    ticker: &str,
    accession_nodash: &str,
    base_url: &str,
    index_url: &str,
    primary_document: Option<&str>,
    download_all: bool,
    important_exhibits: bool,
    single_file: bool,
) -> Result<FilingDownloadResult> {
    let filing_dir = cache_dir
        .join("finance")
        .join("filings")
        .join(ticker)
        .join(accession_nodash);
    std::fs::create_dir_all(&filing_dir)?;

    let mut files = Vec::new();
    let mut primary_doc_path = None;
    let mut seen = std::collections::HashSet::new();
    if let Some(filename) = primary_document.and_then(sec_archive_filename) {
        seen.insert(filename.clone());
        let url = archive_file_url(base_url, &filename);
        let path = filing_dir.join(&filename);
        let downloaded =
            download_sec_archive_file(client, "primary_document", &filename, &url, &path).await?;
        primary_doc_path = Some(downloaded.path.clone());
        files.push(downloaded);
    }

    if single_file {
        if primary_doc_path.is_none() {
            return Err(Error::Provider(
                "single-file download requested but SEC submission has no primary document"
                    .to_string(),
            ));
        }
        return Ok(FilingDownloadResult {
            download_dir: filing_dir.display().to_string(),
            primary_doc_path,
            index_json_path: None,
            files,
        });
    }

    let index_path = filing_dir.join("index.json");
    let index_bytes = sec_get_bytes(client, index_url).await?;
    std::fs::write(&index_path, &index_bytes)?;
    files.insert(
        0,
        FilingDownload {
            kind: "index_json".to_string(),
            filename: "index.json".to_string(),
            url: index_url.to_string(),
            path: index_path.display().to_string(),
            bytes: Some(index_bytes.len() as u64),
        },
    );

    if download_all || important_exhibits {
        let attachment_kind = if download_all {
            "attachment"
        } else {
            "important_attachment"
        };
        for filename in filing_index_document_names(&index_bytes)
            .into_iter()
            .filter(|filename| download_all || is_important_filing_attachment(filename))
        {
            if filename.eq_ignore_ascii_case("index.json") || !seen.insert(filename.clone()) {
                continue;
            }
            let url = archive_file_url(base_url, &filename);
            let path = filing_dir.join(&filename);
            files.push(
                download_sec_archive_file(client, attachment_kind, &filename, &url, &path).await?,
            );
        }
    }

    Ok(FilingDownloadResult {
        download_dir: filing_dir.display().to_string(),
        primary_doc_path,
        index_json_path: Some(index_path.display().to_string()),
        files,
    })
}

async fn download_sec_archive_file(
    client: &reqwest::Client,
    kind: &str,
    filename: &str,
    url: &str,
    path: &Path,
) -> Result<FilingDownload> {
    let bytes = sec_get_bytes(client, url).await?;
    std::fs::write(path, &bytes)?;
    Ok(FilingDownload {
        kind: kind.to_string(),
        filename: filename.to_string(),
        url: url.to_string(),
        path: path.display().to_string(),
        bytes: Some(bytes.len() as u64),
    })
}

fn archive_file_url(base_url: &str, filename: &str) -> String {
    format!("{}{}", base_url, filename.trim_start_matches('/'))
}

fn sec_archive_filename(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let filename = Path::new(trimmed).file_name()?.to_str()?.trim();
    if filename.is_empty() || filename == "." || filename == ".." {
        return None;
    }
    Some(filename.to_string())
}

#[derive(Deserialize)]
struct FilingIndexJson {
    directory: Option<FilingIndexDirectory>,
}

#[derive(Deserialize)]
struct FilingIndexDirectory {
    item: Option<Vec<FilingIndexItem>>,
}

#[derive(Deserialize)]
struct FilingIndexItem {
    name: Option<String>,
}

fn filing_index_document_names(index_bytes: &[u8]) -> Vec<String> {
    let Ok(index) = serde_json::from_slice::<FilingIndexJson>(index_bytes) else {
        return Vec::new();
    };
    index
        .directory
        .and_then(|directory| directory.item)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| item.name.as_deref().and_then(sec_archive_filename))
        .collect()
}

fn is_important_filing_attachment(filename: &str) -> bool {
    let normalized = filename.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "index.json" {
        return false;
    }

    let path = Path::new(&normalized);
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    if !matches!(extension, "htm" | "html" | "txt" | "pdf") {
        return false;
    }

    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(normalized.as_str());
    let support_file = stem.starts_with('r') && stem[1..].chars().all(|c| c.is_ascii_digit())
        || stem.contains("filingsummary")
        || stem.contains("metalinks")
        || stem.contains("index")
        || stem.contains("headers")
        || stem == "show"
        || stem == "report"
        || normalized.ends_with(".css")
        || normalized.ends_with(".js")
        || is_sec_complete_submission_text(&normalized);
    if support_file {
        return false;
    }

    [
        "ex99",
        "ex-99",
        "ex_99",
        "exhibit99",
        "exhibit-99",
        "exhibit_99",
        "earnings",
        "earning",
        "release",
        "slides",
        "presentation",
        "investor",
        "q1",
        "q2",
        "q3",
        "q4",
    ]
    .iter()
    .any(|needle| stem.contains(needle))
}

fn is_sec_complete_submission_text(filename: &str) -> bool {
    let Some(stem) = filename.strip_suffix(".txt") else {
        return false;
    };
    let parts = stem.split('-').collect::<Vec<_>>();
    parts.len() == 3
        && parts[0].len() == 10
        && parts[1].len() == 2
        && parts[2].len() == 6
        && parts
            .iter()
            .all(|part| part.chars().all(|c| c.is_ascii_digit()))
}

/// Cross-filer full-text search over SEC EDGAR (efts.sec.gov). Answers
/// "which filings mention X across the whole market" — the per-ticker path
/// above can only enumerate one known company's filings. Same-day freshness,
/// no auth beyond the standard SEC User-Agent policy.
pub async fn search_filings_fulltext(
    query: &str,
    forms: &[String],
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
    user_agent: Option<&str>,
) -> Result<serde_json::Value> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::InvalidInput("--search-text query cannot be empty".to_string()));
    }
    if limit == 0 {
        return Err(Error::InvalidInput("--limit must be >= 1".to_string()));
    }
    let limit = limit.min(50);
    let client = sec_client(user_agent)?;

    // Multi-word input becomes a phrase query unless the caller already
    // supplied quotes/operators of their own.
    let q = if query.contains('"') || !query.contains(char::is_whitespace) {
        query.to_string()
    } else {
        format!("\"{query}\"")
    };
    let forms_join = forms
        .iter()
        .map(|f| f.trim().to_ascii_uppercase())
        .filter(|f| !f.is_empty())
        .collect::<Vec<_>>()
        .join(",");

    // EDGAR FTS pages 10 hits at a time via a 0-based `from` offset.
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut total: Option<u64> = None;
    let mut total_is_lower_bound = false;
    let mut offset = 0usize;
    while results.len() < limit {
        let mut url = reqwest::Url::parse("https://efts.sec.gov/LATEST/search-index")
            .map_err(|e| Error::Provider(format!("efts url: {e}")))?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("q", &q);
            if !forms_join.is_empty() {
                qp.append_pair("forms", &forms_join);
            }
            if let Some(from) = from {
                qp.append_pair("startdt", from);
            }
            if let Some(to) = to {
                qp.append_pair("enddt", to);
            }
            if offset > 0 {
                qp.append_pair("from", &offset.to_string());
            }
        }
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("efts request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "efts full-text search returned http {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("efts parse failed: {e}")))?;
        if total.is_none() {
            total = body
                .pointer("/hits/total/value")
                .and_then(|v| v.as_u64());
            // EDGAR reports totals above 10000 as a floor ("gte"), not an
            // exact count — surface that instead of a suspiciously round number.
            total_is_lower_bound = body
                .pointer("/hits/total/relation")
                .and_then(|v| v.as_str())
                .map(|r| r == "gte")
                .unwrap_or(false);
        }
        let hits = body
            .pointer("/hits/hits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if hits.is_empty() {
            break;
        }
        let page_len = hits.len();
        for hit in hits {
            if results.len() >= limit {
                break;
            }
            let src = hit.get("_source").cloned().unwrap_or_default();
            // display_names[0] looks like "Empery Digital Inc.  (EMPD)  (CIK 0001829794)"
            let display = src
                .get("display_names")
                .and_then(|v| v.get(0))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let ticker = display
                .split('(')
                .map(str::trim)
                .filter_map(|part| part.strip_suffix(')').map(str::trim))
                .find(|part| {
                    !part.starts_with("CIK")
                        && !part.is_empty()
                        && part.len() <= 6
                        && part.chars().all(|c| c.is_ascii_uppercase() || c == '.' || c == '-')
                });
            let company = display.split('(').next().unwrap_or("").trim().to_string();
            let cik = src
                .get("ciks")
                .and_then(|v| v.get(0))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_start_matches('0')
                .to_string();
            let adsh = src.get("adsh").and_then(|v| v.as_str()).unwrap_or("").to_string();
            // _id is "adsh:filename" — enough to build the document URL.
            let doc_url = hit
                .get("_id")
                .and_then(|v| v.as_str())
                .and_then(|id| id.split_once(':'))
                .map(|(a, file)| {
                    format!(
                        "https://www.sec.gov/Archives/edgar/data/{}/{}/{}",
                        cik,
                        a.replace('-', ""),
                        file
                    )
                });
            let mut row = serde_json::json!({
                "company": company,
                "cik": cik,
                "form": src.get("form").cloned().unwrap_or(serde_json::Value::Null),
                "file_date": src.get("file_date").cloned().unwrap_or(serde_json::Value::Null),
                "accession_number": adsh,
            });
            if let Some(t) = ticker {
                row["ticker"] = serde_json::json!(t);
            }
            if let Some(items) = src.get("items").filter(|v| !v.is_null()) {
                row["items"] = items.clone();
            }
            if let Some(u) = doc_url {
                row["url"] = serde_json::json!(u);
            }
            results.push(row);
        }
        offset += page_len;
        if page_len < 10 {
            break;
        }
    }

    let mut out = serde_json::json!({
        "query": query,
        "forms": if forms_join.is_empty() { serde_json::Value::Null } else { serde_json::json!(forms_join) },
        "from": from,
        "to": to,
        "total_matches": total,
        "returned": results.len(),
        "results": results,
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    if total_is_lower_bound {
        out["total_matches_is_lower_bound"] = serde_json::json!(true);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn important_attachment_filter_keeps_agent_useful_exhibits() {
        for filename in [
            "orcl-ex99_1.htm",
            "meta-03312026xexhibit991.htm",
            "amdq126earningsslidesfin.htm",
            "q12026991.htm",
            "d136714dex993.htm",
            "investor-presentation.pdf",
        ] {
            assert!(is_important_filing_attachment(filename), "{filename}");
        }
    }

    #[test]
    fn important_attachment_filter_drops_sec_support_noise() {
        for filename in [
            "R1.htm",
            "R104.htm",
            "MetaLinks.json",
            "FilingSummary.xml",
            "report.css",
            "Show.js",
            "0000002488-26-000072.txt",
            "amdq126earningsslidesfin001.jpg",
            "0000002488-26-000072-xbrl.zip",
        ] {
            assert!(!is_important_filing_attachment(filename), "{filename}");
        }
    }
}
