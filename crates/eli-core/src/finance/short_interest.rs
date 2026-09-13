use super::{Error, Result};

/// FINRA equity short data, no auth required (public `otcMarket` group).
/// Two feeds with different cadences, returned together as one picture:
/// - Consolidated short interest: bi-monthly settlement prints (mid-month +
///   month-end, published ~1-2 weeks after settlement). Shares short,
///   days-to-cover, change vs prior period.
/// - Reg SHO daily short volume: per-venue daily short-sale volume (T+1/T+2),
///   the fast-moving pressure gauge between settlement prints.
const FINRA_BASE: &str = "https://api.finra.org/data/group/otcMarket/name";

fn finra_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| Error::Provider(format!("finra client: {e}")))
}

async fn finra_query(
    client: &reqwest::Client,
    dataset: &str,
    filters: &[(&str, String)],
    limit: usize,
) -> Result<Vec<serde_json::Value>> {
    let body = serde_json::json!({
        "limit": limit,
        "compareFilters": filters
            .iter()
            .map(|(field, value)| serde_json::json!({
                "compareType": "EQUAL",
                "fieldName": field,
                "fieldValue": value,
            }))
            .collect::<Vec<_>>(),
    });
    let resp = client
        .post(format!("{FINRA_BASE}/{dataset}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("finra {dataset} request: {e}")))?;
    let status = resp.status();
    if status.as_u16() == 204 {
        return Ok(Vec::new());
    }
    if !status.is_success() {
        return Err(Error::Provider(format!("finra {dataset} http {status}")));
    }
    resp.json::<Vec<serde_json::Value>>()
        .await
        .map_err(|e| Error::Provider(format!("finra {dataset} parse: {e}")))
}

/// Candidate settlement dates, newest first: FINRA short-interest settles
/// mid-month and month-end (rolled back from weekends), publishing ~1-2
/// weeks later — so walk the last few candidates until one returns rows.
fn settlement_candidates(today: chrono::NaiveDate) -> Vec<String> {
    use chrono::Datelike;
    let mut out = Vec::new();
    let mut push_biz = |date: chrono::NaiveDate| {
        let mut d = date;
        while matches!(
            d.weekday(),
            chrono::Weekday::Sat | chrono::Weekday::Sun
        ) {
            d = d.pred_opt().unwrap_or(d);
        }
        out.push(d.format("%Y-%m-%d").to_string());
    };
    let mut cursor = today;
    for _ in 0..4 {
        let (y, m) = (
            chrono::Datelike::year(&cursor),
            chrono::Datelike::month(&cursor),
        );
        // month-end of the PRIOR month relative to cursor, then mid-month.
        let first_of_month = chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(cursor);
        let prior_month_end = first_of_month.pred_opt().unwrap_or(cursor);
        let mid = chrono::NaiveDate::from_ymd_opt(y, m, 15).unwrap_or(cursor);
        if mid <= today {
            push_biz(mid);
        }
        push_biz(prior_month_end);
        cursor = first_of_month.pred_opt().unwrap_or(cursor);
    }
    out.sort();
    out.reverse();
    out.dedup();
    out
}

pub async fn fetch_short_data(ticker: &str) -> Result<serde_json::Value> {
    let ticker = ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }
    let client = finra_client()?;
    let today = chrono::Utc::now().date_naive();

    // --- Consolidated short interest: walk settlement candidates ---
    let mut short_interest = serde_json::Value::Null;
    for settlement in settlement_candidates(today).into_iter().take(6) {
        let rows = finra_query(
            &client,
            "consolidatedShortInterest",
            &[
                ("settlementDate", settlement.clone()),
                ("symbolCode", ticker.clone()),
            ],
            5,
        )
        .await?;
        if let Some(row) = rows.first() {
            short_interest = serde_json::json!({
                "settlement_date": row.get("settlementDate"),
                "short_shares": row.get("currentShortPositionQuantity"),
                "prior_short_shares": row.get("previousShortPositionQuantity"),
                "change_shares": row.get("changePreviousNumber"),
                "change_pct": row.get("changePercent"),
                "days_to_cover": row.get("daysToCoverQuantity"),
                "avg_daily_volume": row.get("averageDailyVolumeQuantity"),
                "market": row.get("marketClassCode"),
            });
            break;
        }
    }

    // --- Reg SHO daily short volume: walk back to the last trading day ---
    let mut daily = serde_json::Value::Null;
    let mut probe = today;
    for _ in 0..7 {
        let date = probe.format("%Y-%m-%d").to_string();
        let rows = finra_query(
            &client,
            "regshoDaily",
            &[
                ("tradeReportDate", date.clone()),
                (
                    "securitiesInformationProcessorSymbolIdentifier",
                    ticker.clone(),
                ),
            ],
            20,
        )
        .await?;
        if !rows.is_empty() {
            let mut short_vol = 0.0;
            let mut exempt_vol = 0.0;
            let mut total_vol = 0.0;
            let mut venues = Vec::new();
            for row in &rows {
                let sv = row
                    .get("shortParQuantity")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let ev = row
                    .get("shortExemptParQuantity")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let tv = row
                    .get("totalParQuantity")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                short_vol += sv;
                exempt_vol += ev;
                total_vol += tv;
                venues.push(serde_json::json!({
                    "facility": row.get("reportingFacilityCode"),
                    "short_volume": sv,
                    "total_volume": tv,
                }));
            }
            daily = serde_json::json!({
                "date": date,
                "short_volume": short_vol,
                "short_exempt_volume": exempt_vol,
                "total_volume": total_vol,
                "short_volume_ratio": if total_vol > 0.0 { Some(short_vol / total_vol) } else { None },
                "venues": venues,
            });
            break;
        }
        probe = probe.pred_opt().unwrap_or(probe);
    }

    if short_interest.is_null() && daily.is_null() {
        return Err(Error::Provider(format!(
            "FINRA returned no short data for '{ticker}' — symbol may be unlisted or OTC-only"
        )));
    }

    Ok(serde_json::json!({
        "ticker": ticker,
        "short_interest": short_interest,
        "daily_short_volume": daily,
        "source": "finra",
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "notes": "short_interest settles bi-monthly (published ~1-2wk later); daily_short_volume is per-venue Reg SHO short-sale volume (off-exchange TRF facilities, not full-market)",
    }))
}
