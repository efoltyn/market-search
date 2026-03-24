use crate::{Error, Result};
use tokio::time::{sleep, Duration as TokioDuration};

const FISCAL_BASE: &str =
    "https://api.fiscaldata.treasury.gov/services/api/fiscal_service/";

fn parse_f64_fiscal(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::String(s) => {
            let cleaned = s.trim().replace(',', "");
            cleaned.parse::<f64>().ok()
        }
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn parse_string_fiscal(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

pub async fn fetch_fiscal(req: FiscalRequest) -> Result<FiscalResponse> {
    let client = &*crate::finance::shared_client::GENERAL;
    let kind = req.kind.to_ascii_lowercase();

    let url = match kind.as_str() {
        "debt" => format!(
            "{FISCAL_BASE}v2/accounting/od/debt_to_penny\
             ?sort=-record_date&page[size]=5\
             &fields=record_date,tot_pub_debt_out_amt,debt_held_public_amt,intragov_hold_amt"
        ),
        "statement" => format!(
            "{FISCAL_BASE}v1/accounting/dts/operating_cash_balance\
             ?sort=-record_date&page[size]=10\
             &fields=record_date,account_type,close_today_bal,open_today_bal,open_month_bal"
        ),
        "interest" => format!(
            "{FISCAL_BASE}v2/accounting/od/avg_interest_rates\
             ?sort=-record_date&page[size]=20\
             &fields=record_date,security_desc,avg_interest_rate_amt"
        ),
        _ => {
            return Err(Error::InvalidInput(format!(
                "fiscal kind must be debt|statement|interest, got '{kind}'"
            )))
        }
    };

    // Fetch with 3 retries, 500ms backoff (same pattern as auctions/fetch.rs)
    let mut last_err = String::new();
    let mut body: Option<serde_json::Value> = None;

    for attempt in 0u64..3 {
        if attempt > 0 {
            sleep(TokioDuration::from_millis(500 * attempt)).await;
        }
        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    last_err = format!("HTTP {}", resp.status());
                    continue;
                }
                match resp.json::<serde_json::Value>().await {
                    Ok(val) => {
                        body = Some(val);
                        break;
                    }
                    Err(e) => {
                        last_err = format!("json parse: {e}");
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = format!("request: {e}");
                continue;
            }
        }
    }

    let json = body.ok_or_else(|| {
        Error::Provider(format!(
            "Treasury fiscal API ({kind}) failed after 3 attempts: {last_err}"
        ))
    })?;

    let data = json
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            Error::Provider("missing 'data' array in Treasury fiscal API response".into())
        })?;

    match kind.as_str() {
        "debt" => {
            let items: Vec<FiscalDebtItem> = data
                .iter()
                .filter_map(|item| {
                    let record_date = parse_string_fiscal(
                        item.get("record_date").unwrap_or(&serde_json::Value::Null),
                    )?;
                    let total_debt_billions =
                        parse_f64_fiscal(item.get("tot_pub_debt_out_amt")?)? / 1_000_000_000.0;
                    let public_debt_billions = parse_f64_fiscal(
                        item.get("debt_held_public_amt")
                            .unwrap_or(&serde_json::Value::Null),
                    )
                    .map(|v| v / 1_000_000_000.0);
                    let intragovernmental_billions = parse_f64_fiscal(
                        item.get("intragov_hold_amt")
                            .unwrap_or(&serde_json::Value::Null),
                    )
                    .map(|v| v / 1_000_000_000.0);

                    Some(FiscalDebtItem {
                        record_date,
                        total_debt_billions,
                        public_debt_billions,
                        intragovernmental_billions,
                    })
                })
                .collect();

            Ok(FiscalResponse {
                generated_at: Utc::now(),
                kind,
                debt: Some(items),
                statement: None,
                interest: None,
            })
        }
        "statement" => {
            let items: Vec<FiscalStatementItem> = data
                .iter()
                .filter_map(|item| {
                    let record_date = parse_string_fiscal(
                        item.get("record_date").unwrap_or(&serde_json::Value::Null),
                    )?;
                    let account = parse_string_fiscal(
                        item.get("account_type").unwrap_or(&serde_json::Value::Null),
                    )
                    .unwrap_or_default();
                    let close_today_bal = parse_f64_fiscal(
                        item.get("close_today_bal")
                            .unwrap_or(&serde_json::Value::Null),
                    );
                    let open_today_bal = parse_f64_fiscal(
                        item.get("open_today_bal")
                            .unwrap_or(&serde_json::Value::Null),
                    );

                    Some(FiscalStatementItem {
                        record_date,
                        account,
                        close_today_bal,
                        open_today_bal,
                    })
                })
                .collect();

            Ok(FiscalResponse {
                generated_at: Utc::now(),
                kind,
                debt: None,
                statement: Some(items),
                interest: None,
            })
        }
        "interest" => {
            let items: Vec<FiscalInterestItem> = data
                .iter()
                .filter_map(|item| {
                    let record_date = parse_string_fiscal(
                        item.get("record_date").unwrap_or(&serde_json::Value::Null),
                    )?;
                    let security_desc = parse_string_fiscal(
                        item.get("security_desc").unwrap_or(&serde_json::Value::Null),
                    )
                    .unwrap_or_default();
                    let avg_interest_rate_pct = parse_f64_fiscal(
                        item.get("avg_interest_rate_amt")
                            .unwrap_or(&serde_json::Value::Null),
                    )?;

                    Some(FiscalInterestItem {
                        record_date,
                        security_desc,
                        avg_interest_rate_pct,
                    })
                })
                .collect();

            Ok(FiscalResponse {
                generated_at: Utc::now(),
                kind,
                debt: None,
                statement: None,
                interest: Some(items),
            })
        }
        _ => unreachable!(),
    }
}
