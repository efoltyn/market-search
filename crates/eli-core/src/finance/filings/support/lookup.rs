pub(crate) async fn sec_lookup_cik(
    ticker: &str,
    sec_dir: &Path,
    ua: Option<&str>,
) -> Result<(String, String)> {
    let client = sec_client(ua)?;
    let map_path = sec_dir.join("company_tickers.json");

    let use_cache = file_is_fresh(&map_path, SEC_COMPANY_TICKERS_TTL_SECS);
    let raw = if use_cache {
        std::fs::read_to_string(&map_path)?
    } else {
        let url = "https://www.sec.gov/files/company_tickers.json";
        let text = sec_get_json(&client, url).await?;
        std::fs::write(&map_path, &text)?;
        text
    };

    #[derive(Deserialize)]
    struct Entry {
        cik_str: u64,
        ticker: String,
        title: String,
    }

    let parsed: std::collections::HashMap<String, Entry> = serde_json::from_str(&raw)
        .map_err(|e| Error::Provider(format!("sec map parse failed: {e}")))?;

    for (_k, entry) in parsed {
        if entry.ticker.trim().eq_ignore_ascii_case(ticker) {
            let cik_padded = format!("{:010}", entry.cik_str);
            return Ok((cik_padded, entry.title));
        }
    }

    Err(Error::InvalidInput(format!(
        "unknown ticker '{ticker}' for SEC filings"
    )))
}
