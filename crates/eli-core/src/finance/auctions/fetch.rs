use super::super::*;
use crate::{Error, Result};
use tokio::time::{sleep, Duration as TokioDuration};

const TREASURY_API_BASE: &str =
    "https://api.fiscaldata.treasury.gov/services/api/fiscal_service/v1/accounting/od/auctions_query";

const FIELDS: &str = "record_date,cusip,security_type,security_term,auction_date,issue_date,maturity_date,high_yield,high_investment_rate,high_discnt_rate,high_discnt_margin,bid_to_cover_ratio,total_accepted,total_tendered,direct_bidder_tendered,direct_bidder_accepted,indirect_bidder_tendered,indirect_bidder_accepted,inflation_index_security,floating_rate";

fn parse_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn parse_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

fn normalized_security_type(item: &serde_json::Value) -> String {
    let raw = parse_string(item.get("security_type").unwrap_or(&serde_json::Value::Null))
        .unwrap_or_default();
    let raw_lower = raw.to_ascii_lowercase();
    let is_tips = item
        .get("inflation_index_security")
        .and_then(parse_string)
        .map(|s| s.eq_ignore_ascii_case("yes"))
        .unwrap_or(false);
    if is_tips {
        return "TIPS".to_string();
    }

    let is_frn = item
        .get("floating_rate")
        .and_then(parse_string)
        .map(|s| s.eq_ignore_ascii_case("yes"))
        .unwrap_or(false);
    if is_frn {
        return "FRN".to_string();
    }

    match raw_lower.as_str() {
        "bill" => "Bill".to_string(),
        "note" => "Note".to_string(),
        "bond" => "Bond".to_string(),
        _ => raw,
    }
}

pub async fn fetch_auctions(req: AuctionsRequest) -> Result<AuctionsResponse> {
    let limit = req.limit.unwrap_or(50).min(500);
    // When filtering by security type, fetch more rows from the API so we have
    // enough candidates after client-side filtering (bonds are ~10% of auctions).
    let fetch_size = if req.security_type.is_some() {
        (limit * 10).min(500)
    } else {
        limit
    };
    let today = Utc::now().format("%Y-%m-%d");
    let url = format!(
        "{}?fields={}&sort=-auction_date&page[size]={}&filter=auction_date:lte:{},bid_to_cover_ratio:gt:0",
        TREASURY_API_BASE, FIELDS, fetch_size, today
    );

    let client = &*crate::finance::shared_client::GENERAL;

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
            "Treasury auction API failed after 3 attempts: {last_err}"
        ))
    })?;

    let data = json
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Provider("missing 'data' array in Treasury API response".into()))?;

    let filter_type = req
        .security_type
        .as_ref()
        .map(|s| s.trim().to_ascii_lowercase());

    let mut auctions = Vec::with_capacity(data.len());
    for item in data {
        let security_type = normalized_security_type(item);

        // Filter by security type if requested.
        if let Some(ref ft) = filter_type {
            if security_type.to_ascii_lowercase() != *ft {
                continue;
            }
        }

        let cusip = parse_string(item.get("cusip").unwrap_or(&serde_json::Value::Null))
            .unwrap_or_default();
        let security_term = parse_string(item.get("security_term").unwrap_or(&serde_json::Value::Null))
            .unwrap_or_default();
        let auction_date = parse_string(item.get("auction_date").unwrap_or(&serde_json::Value::Null))
            .unwrap_or_default();
        let issue_date = item.get("issue_date").and_then(parse_string);
        let maturity_date = item.get("maturity_date").and_then(parse_string);

        // Treasury Direct uses different yield fields by security type:
        //   notes/bonds/TIPS → `high_yield`
        //   bills           → `high_investment_rate` (bond-equivalent yield), fallback `high_discnt_rate`
        //   FRNs            → `high_discnt_margin` (spread over the index)
        // We expose all three through one `high_yield` field so callers see one consistent name.
        let high_yield = item.get("high_yield").and_then(parse_f64).or_else(|| {
            match security_type.as_str() {
                "Bill" => item
                    .get("high_investment_rate")
                    .and_then(parse_f64)
                    .or_else(|| item.get("high_discnt_rate").and_then(parse_f64)),
                "FRN" => item.get("high_discnt_margin").and_then(parse_f64),
                _ => None,
            }
        });
        let bid_to_cover_ratio = item.get("bid_to_cover_ratio").and_then(parse_f64);
        let total_accepted = item.get("total_accepted").and_then(parse_f64);
        let total_tendered = item.get("total_tendered").and_then(parse_f64);
        let direct_bidder_accepted = item.get("direct_bidder_accepted").and_then(parse_f64);
        let indirect_bidder_accepted = item.get("indirect_bidder_accepted").and_then(parse_f64);

        let direct_bidder_pct = match (direct_bidder_accepted, total_accepted) {
            (Some(d), Some(t)) if t > 0.0 => Some((d / t * 100.0 * 100.0).round() / 100.0),
            _ => None,
        };
        let indirect_bidder_pct = match (indirect_bidder_accepted, total_accepted) {
            (Some(i), Some(t)) if t > 0.0 => Some((i / t * 100.0 * 100.0).round() / 100.0),
            _ => None,
        };

        // tail_bps would require when-issued yield which is not in the API response,
        // so we leave it as None.
        let tail_bps = None;

        auctions.push(AuctionResult {
            cusip,
            security_type,
            security_term,
            auction_date,
            issue_date,
            maturity_date,
            high_yield,
            bid_to_cover_ratio,
            total_accepted,
            total_tendered,
            direct_bidder_pct,
            indirect_bidder_pct,
            tail_bps,
        });
    }

    auctions.truncate(limit);
    let count = auctions.len();
    Ok(AuctionsResponse {
        generated_at: Utc::now(),
        auctions,
        count,
        filter: filter_type,
    })
}

#[cfg(test)]
mod tests {
    use super::normalized_security_type;

    #[test]
    fn classifies_tips_from_inflation_flag() {
        let row = serde_json::json!({
            "security_type": "Note",
            "inflation_index_security": "Yes",
            "floating_rate": "No",
        });
        assert_eq!(normalized_security_type(&row), "TIPS");
    }

    #[test]
    fn classifies_frn_from_floating_rate_flag() {
        let row = serde_json::json!({
            "security_type": "Note",
            "inflation_index_security": "No",
            "floating_rate": "Yes",
        });
        assert_eq!(normalized_security_type(&row), "FRN");
    }
}
