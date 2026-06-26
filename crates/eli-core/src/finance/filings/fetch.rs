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

    let limit = if req.single_file {
        1
    } else {
        req.limit.unwrap_or(5).clamp(1, 25)
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
    let submissions =
        sec_fetch_submissions(&cik_str, &company_name, &sec_dir, req.user_agent.as_deref()).await?;

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

    let mut selected_indexes = matching_indexes(&forms_set);
    if selected_indexes.is_empty() && used_default_forms {
        let foreign_forms = ["6-K", "20-F", "40-F"]
            .into_iter()
            .map(str::to_string)
            .collect::<std::collections::HashSet<_>>();
        selected_indexes = matching_indexes(&foreign_forms);
    }

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

        let mut doc = FilingDoc {
            form,
            filing_date,
            report_date,
            accession_number: accession,
            acceptance_datetime,
            items,
            primary_document,
            primary_doc_description,
            size,
            url: url.clone(),
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
        };

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
    let cleaned = if looks_html {
        strip_html_to_text(&raw)
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
            if let Ok(html) = std::fs::read_to_string(primary_path) {
                let parsed = strip_html_to_text(&html);
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
    let html = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let parsed = strip_html_to_text(&html);
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
