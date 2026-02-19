#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SecSubmissions {
    pub cik: String,
    pub name: Option<String>,
    pub filings: Option<SecFilings>,
}

#[derive(Deserialize)]
pub(super) struct SecFilings {
    pub recent: Option<SecRecent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SecRecent {
    pub accession_number: Vec<String>,
    pub filing_date: Vec<String>,
    #[serde(default)]
    pub report_date: Option<Vec<String>>,
    #[serde(default)]
    pub acceptance_date_time: Option<Vec<String>>,
    pub form: Vec<String>,
    #[serde(default)]
    pub items: Option<Vec<String>>,
    #[serde(default)]
    pub size: Option<Vec<u64>>,
    #[serde(default)]
    pub primary_document: Option<Vec<String>>,
    #[serde(default)]
    pub primary_doc_description: Option<Vec<String>>,
}

pub(crate) async fn sec_fetch_submissions(
    cik_padded: &str,
    fallback_name: &str,
    sec_dir: &Path,
    ua: Option<&str>,
) -> Result<SecSubmissions> {
    let client = sec_client(ua)?;
    let submissions_dir = sec_dir.join("submissions");
    std::fs::create_dir_all(&submissions_dir)?;
    let path = submissions_dir.join(format!("CIK{cik_padded}.json"));

    let use_cache = file_is_fresh(&path, SEC_SUBMISSIONS_TTL_SECS);
    let raw = if use_cache {
        std::fs::read_to_string(&path)?
    } else {
        let url = format!("https://data.sec.gov/submissions/CIK{cik_padded}.json");
        let text = sec_get_json(&client, &url).await?;
        std::fs::write(&path, &text)?;
        text
    };

    let mut parsed: SecSubmissions = serde_json::from_str(&raw)
        .map_err(|e| Error::Provider(format!("sec submissions parse failed: {e}")))?;
    if parsed.name.as_deref().unwrap_or("").trim().is_empty() {
        parsed.name = Some(fallback_name.to_string());
    }
    Ok(parsed)
}

fn file_is_fresh(path: &Path, max_age_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return false;
    };
    elapsed.as_secs() <= max_age_secs
}
