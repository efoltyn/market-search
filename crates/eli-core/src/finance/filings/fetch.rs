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

    let limit = req.limit.unwrap_or(5).clamp(1, 25);
    let should_download = req.download || req.download_all || req.include_text;

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
            )
            .await?;
            doc.download_dir = Some(download.download_dir);
            doc.primary_doc_path = download.primary_doc_path;
            doc.index_json_path = download.index_json_path;
            doc.downloaded_files = download.files;
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

async fn download_filing_files(
    client: &reqwest::Client,
    cache_dir: &Path,
    ticker: &str,
    accession_nodash: &str,
    base_url: &str,
    index_url: &str,
    primary_document: Option<&str>,
    download_all: bool,
) -> Result<FilingDownloadResult> {
    let filing_dir = cache_dir
        .join("finance")
        .join("filings")
        .join(ticker)
        .join(accession_nodash);
    std::fs::create_dir_all(&filing_dir)?;

    let mut files = Vec::new();
    let index_path = filing_dir.join("index.json");
    let index_bytes = sec_get_bytes(client, index_url).await?;
    std::fs::write(&index_path, &index_bytes)?;
    files.push(FilingDownload {
        kind: "index_json".to_string(),
        filename: "index.json".to_string(),
        url: index_url.to_string(),
        path: index_path.display().to_string(),
        bytes: Some(index_bytes.len() as u64),
    });

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

    if download_all {
        for filename in filing_index_document_names(&index_bytes) {
            if filename.eq_ignore_ascii_case("index.json") || !seen.insert(filename.clone()) {
                continue;
            }
            let url = archive_file_url(base_url, &filename);
            let path = filing_dir.join(&filename);
            files.push(
                download_sec_archive_file(client, "attachment", &filename, &url, &path).await?,
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
