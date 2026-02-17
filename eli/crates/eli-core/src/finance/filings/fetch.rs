use super::super::*;
use super::support::{
    best_effort_sec_filing_excerpt, html_to_text, sanitize_for_filename, sec_client,
    sec_fetch_submissions, sec_get_text, sec_lookup_cik,
};

pub async fn fetch_filings(req: FilingsRequest, cache_dir: &Path) -> Result<FilingsResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

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
    let excerpt_max = req.max_chars.unwrap_or(SEC_DEFAULT_TEXT_MAX_CHARS).max(256);

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

    for i in 0..n {
        let form = recent.form.get(i).cloned().unwrap_or_default();
        if !forms_set.contains(&form.to_ascii_uppercase()) {
            continue;
        }
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
            text_path: None,
            text_excerpt: None,
        };

        if req.include_text {
            if let Some(url) = url {
                // Download and convert to text; store on disk and return an excerpt inline.
                let raw = sec_get_text(&client, &url).await?;
                let text = html_to_text(&raw);

                let filings_dir = cache_dir.join("finance").join("filings").join(&ticker);
                std::fs::create_dir_all(&filings_dir)?;
                let safe_form = sanitize_for_filename(&doc.form);
                let path = filings_dir.join(format!("{accession_nodash}_{safe_form}.txt"));
                std::fs::write(&path, text.as_bytes())?;

                doc.text_path = Some(path.display().to_string());
                doc.text_excerpt = Some(best_effort_sec_filing_excerpt(
                    &text,
                    &doc.form,
                    doc.items.as_deref(),
                    excerpt_max,
                ));
            }
        }

        out.push(doc);
        if out.len() >= limit {
            break;
        }

        // Be nice to SEC: small delay between doc fetches.
        if req.include_text {
            tokio::time::sleep(StdDuration::from_millis(125)).await;
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
