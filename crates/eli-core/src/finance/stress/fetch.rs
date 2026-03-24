const OFR_FSI_URL: &str =
    "https://www.financialresearch.gov/financial-stress-index/data/fsi.csv";

fn parse_opt_f64(s: &str) -> Option<f64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        trimmed.parse::<f64>().ok()
    }
}

pub async fn fetch_stress(req: StressRequest) -> Result<StressResponse> {
    let client = &*crate::finance::shared_client::GENERAL;
    let range_days = req.range_days.unwrap_or(30);

    let resp = client
        .get(OFR_FSI_URL)
        .send()
        .await
        .map_err(|e| Error::Other(format!("OFR FSI request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "OFR FSI returned HTTP {}",
            resp.status()
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| Error::Other(format!("OFR FSI body read failed: {e}")))?;

    let mut rows: Vec<StressDataPoint> = Vec::new();

    for (i, raw_line) in body.lines().enumerate() {
        // Strip BOM from the very first line if present
        let line = if i == 0 {
            raw_line.trim_start_matches('\u{FEFF}')
        } else {
            raw_line
        };

        // Skip header and empty lines
        if i == 0 || line.trim().is_empty() {
            continue;
        }

        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 2 {
            continue;
        }

        let date = cols[0].trim().to_string();
        // Validate date looks like YYYY-MM-DD
        if date.len() != 10 || NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
            continue;
        }

        let fsi = match cols.get(1).and_then(|s| parse_opt_f64(s)) {
            Some(v) => v,
            None => continue, // FSI itself is required
        };

        rows.push(StressDataPoint {
            date,
            fsi,
            // CSV columns: Date, OFR FSI, Credit, Equity valuation, Safe assets, Funding, Volatility
            credit: cols.get(2).and_then(|s| parse_opt_f64(s)),
            equity: cols.get(3).and_then(|s| parse_opt_f64(s)),
            safe_assets: cols.get(4).and_then(|s| parse_opt_f64(s)),
            funding: cols.get(5).and_then(|s| parse_opt_f64(s)),
            volatility: cols.get(6).and_then(|s| parse_opt_f64(s)),
        });
    }

    // Sort by date descending (latest first)
    rows.sort_by(|a, b| b.date.cmp(&a.date));

    // Take range_days + 1 rows (1 for latest, rest for history)
    rows.truncate(range_days + 1);

    if rows.is_empty() {
        return Err(Error::Other("OFR FSI CSV contained no valid data rows".into()));
    }

    let latest = rows.remove(0);
    let count = 1 + rows.len();

    Ok(StressResponse {
        generated_at: Utc::now(),
        latest,
        history: rows,
        count,
    })
}
