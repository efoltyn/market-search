use super::super::*;
use super::support::{sec_client, sec_fetch_submissions, sec_lookup_cik};

pub async fn fetch_insider(req: InsiderRequest, cache_dir: &Path) -> Result<InsiderResponse> {
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    let days = req.days.unwrap_or(90);
    let limit = req.limit.unwrap_or(50).clamp(1, 200);
    let cutoff_date = Utc::now() - Duration::days(days as i64);

    // Reuse existing SEC infrastructure
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

    let client = sec_client(req.user_agent.as_deref())?;
    let mut transactions: Vec<InsiderTransaction> = Vec::new();
    let mut insiders_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for i in 0..n {
        if transactions.len() >= limit {
            break;
        }

        let form = recent.form.get(i).cloned().unwrap_or_default();
        if form != "4" {
            continue;
        }

        let filing_date_str = recent.filing_date.get(i).cloned().unwrap_or_default();
        if filing_date_str.is_empty() {
            continue;
        }

        // Parse filing date and check against cutoff
        if let Ok(filing_date) = chrono::NaiveDate::parse_from_str(&filing_date_str, "%Y-%m-%d") {
            let filing_dt = DateTime::<Utc>::from_naive_utc_and_offset(
                filing_date.and_hms_opt(0, 0, 0).unwrap_or_default(),
                Utc,
            );
            if filing_dt < cutoff_date {
                continue;
            }
        }

        let accession = recent.accession_number.get(i).cloned().unwrap_or_default();
        if accession.is_empty() {
            continue;
        }

        let primary_doc = recent
            .primary_document
            .as_ref()
            .and_then(|v| v.get(i).cloned())
            .unwrap_or_default();

        // Find the XML file (primary doc often points to xslF345X05/something.xml)
        let accession_nodash = accession.replace('-', "");
        let xml_filename = if primary_doc.contains(".xml") {
            // Extract just the XML filename from paths like "xslF345X05/file.xml"
            primary_doc
                .split('/')
                .last()
                .unwrap_or(&primary_doc)
                .to_string()
        } else {
            // Try to find an XML file in the filing index
            format!("primary_doc.xml")
        };

        let xml_url = format!(
            "https://www.sec.gov/Archives/edgar/data/{}/{}/{}",
            cik_num, accession_nodash, xml_filename
        );

        // Fetch and parse Form 4 XML
        match fetch_form4_xml(&client, &xml_url).await {
            Ok(form4_txns) => {
                for txn in form4_txns {
                    insiders_seen.insert(txn.insider_name.clone());
                    let mut txn_with_filing_date = txn;
                    txn_with_filing_date.filing_date = filing_date_str.clone();
                    transactions.push(txn_with_filing_date);
                    if transactions.len() >= limit {
                        break;
                    }
                }
            }
            Err(_) => {
                // Try alternate XML path from filing index
                let index_url = format!(
                    "https://www.sec.gov/Archives/edgar/data/{}/{}/index.json",
                    cik_num, accession_nodash
                );
                if let Ok(xml_path) = find_form4_xml_from_index(&client, &index_url).await {
                    let alt_url = format!(
                        "https://www.sec.gov/Archives/edgar/data/{}/{}/{}",
                        cik_num, accession_nodash, xml_path
                    );
                    if let Ok(form4_txns) = fetch_form4_xml(&client, &alt_url).await {
                        for txn in form4_txns {
                            insiders_seen.insert(txn.insider_name.clone());
                            let mut txn_with_filing_date = txn;
                            txn_with_filing_date.filing_date = filing_date_str.clone();
                            transactions.push(txn_with_filing_date);
                            if transactions.len() >= limit {
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Rate limit: be nice to SEC
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }

    // Compute summary
    let mut buy_count = 0u32;
    let mut sell_count = 0u32;
    let mut buy_shares = 0.0f64;
    let mut sell_shares = 0.0f64;
    let mut buy_value = 0.0f64;
    let mut sell_value = 0.0f64;

    for txn in &transactions {
        let val = txn.value.unwrap_or(0.0);
        match txn.transaction_code.as_str() {
            "P" => {
                buy_count += 1;
                buy_shares += txn.shares;
                buy_value += val;
            }
            "S" => {
                sell_count += 1;
                sell_shares += txn.shares;
                sell_value += val;
            }
            _ => {}
        }
    }

    let summary = InsiderSummary {
        buy_count,
        sell_count,
        buy_shares,
        sell_shares,
        buy_value,
        sell_value,
        net_shares: buy_shares - sell_shares,
        net_value: buy_value - sell_value,
        unique_insiders: insiders_seen.len(),
    };

    let final_transactions = if req.summary_only {
        vec![]
    } else {
        transactions
    };

    Ok(InsiderResponse {
        ticker,
        company_name,
        cik: cik_str,
        generated_at: Utc::now(),
        days_lookback: days,
        summary,
        transactions: final_transactions,
    })
}

async fn fetch_form4_xml(client: &reqwest::Client, url: &str) -> Result<Vec<InsiderTransaction>> {
    let resp = client
        .get(url)
        .header("accept", "application/xml, text/xml")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("form4 fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "form4 fetch failed: http {} ({})",
            resp.status(),
            url
        )));
    }

    let xml = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("form4 read failed: {e}")))?;

    parse_form4_xml(&xml)
}

fn parse_form4_xml(xml: &str) -> Result<Vec<InsiderTransaction>> {
    let mut transactions = Vec::new();

    // Extract reporting owner info
    let owner_name = extract_xml_tag(xml, "rptOwnerName").unwrap_or_default();
    let officer_title = extract_xml_tag(xml, "officerTitle");
    let is_director = extract_xml_tag(xml, "isDirector")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);
    let is_officer = extract_xml_tag(xml, "isOfficer")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);
    let is_ten_percent_owner = extract_xml_tag(xml, "isTenPercentOwner")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    // Parse non-derivative transactions
    let mut cursor = 0usize;
    while let Some(start) = xml[cursor..].find("<nonDerivativeTransaction>") {
        let abs_start = cursor + start;
        let end = match xml[abs_start..].find("</nonDerivativeTransaction>") {
            Some(e) => abs_start + e + 27,
            None => break,
        };
        let txn_xml = &xml[abs_start..end];

        if let Some(txn) = parse_single_transaction(
            txn_xml,
            &owner_name,
            &officer_title,
            is_director,
            is_officer,
            is_ten_percent_owner,
        ) {
            transactions.push(txn);
        }

        cursor = end;
    }

    Ok(transactions)
}

fn parse_single_transaction(
    txn_xml: &str,
    owner_name: &str,
    officer_title: &Option<String>,
    is_director: bool,
    is_officer: bool,
    is_ten_percent_owner: bool,
) -> Option<InsiderTransaction> {
    let transaction_date = extract_xml_tag(txn_xml, "transactionDate")
        .and_then(|d| extract_xml_tag(&d, "value"))
        .unwrap_or_default();

    let transaction_code = extract_xml_tag(txn_xml, "transactionCode").unwrap_or_default();

    let shares_str = extract_xml_tag(txn_xml, "transactionShares")
        .and_then(|s| extract_xml_tag(&s, "value"))
        .unwrap_or_default();
    let shares: f64 = shares_str.parse().unwrap_or(0.0);

    let price_str = extract_xml_tag(txn_xml, "transactionPricePerShare")
        .and_then(|p| extract_xml_tag(&p, "value"))
        .unwrap_or_default();
    let price: Option<f64> = price_str.parse().ok();

    let acquired_disposed = extract_xml_tag(txn_xml, "transactionAcquiredDisposedCode")
        .and_then(|a| extract_xml_tag(&a, "value"))
        .unwrap_or_else(|| "D".to_string());

    let shares_owned_after_str = extract_xml_tag(txn_xml, "sharesOwnedFollowingTransaction")
        .and_then(|s| extract_xml_tag(&s, "value"))
        .unwrap_or_default();
    let shares_owned_after: Option<f64> = shares_owned_after_str.parse().ok();

    let value = price.map(|p| p * shares);

    if transaction_date.is_empty() || shares == 0.0 {
        return None;
    }

    Some(InsiderTransaction {
        filing_date: String::new(), // Will be filled in by caller
        transaction_date,
        insider_name: owner_name.to_string(),
        insider_title: officer_title.clone(),
        is_director,
        is_officer,
        is_ten_percent_owner,
        transaction_code,
        shares,
        price_per_share: price,
        value,
        acquired_disposed,
        shares_owned_after,
    })
}

async fn find_form4_xml_from_index(client: &reqwest::Client, index_url: &str) -> Result<String> {
    let resp = client
        .get(index_url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Provider(format!("index fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider("index fetch failed".to_string()));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("index parse failed: {e}")))?;

    // Look for XML file in directory listing
    if let Some(directory) = json.get("directory") {
        if let Some(items) = directory.get("item").and_then(|i| i.as_array()) {
            for item in items {
                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                    if name.ends_with(".xml") && !name.starts_with("xsl") {
                        return Ok(name.to_string());
                    }
                }
            }
        }
    }

    Err(Error::Provider("no xml file found in index".to_string()))
}
