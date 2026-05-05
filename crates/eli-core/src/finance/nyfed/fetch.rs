use crate::{Error, Result};
use tokio::time::{sleep, Duration as TokioDuration};

const NYFED_RATES_URL: &str = "https://markets.newyorkfed.org/api/rates/all/latest.json";
const NYFED_RRP_URL: &str =
    "https://markets.newyorkfed.org/api/rp/reverserepo/all/results/last/5.json";
const NYFED_SOMA_URL: &str = "https://markets.newyorkfed.org/api/soma/summary.json";
const NYFED_PD_URL: &str =
    "https://markets.newyorkfed.org/api/pd/get/PDPOSGST-TOT_PDSORA-UTSETTOT_PDFTD-USTET_PDFTR-USTET.json";

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

fn parse_u32(v: &serde_json::Value) -> Option<u32> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().map(|n| n as u32),
        serde_json::Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    }
}

fn fetch_url_for_kind(kind: &str) -> Result<&'static str> {
    match kind {
        "rates" => Ok(NYFED_RATES_URL),
        "rrp" => Ok(NYFED_RRP_URL),
        "soma" => Ok(NYFED_SOMA_URL),
        "dealers" => Ok(NYFED_PD_URL),
        _ => Err(Error::InvalidInput(format!(
            "nyfed kind must be rates|rrp|soma|dealers, got '{kind}'"
        ))),
    }
}

fn dealer_label(keyid: &str) -> Option<&'static str> {
    match keyid {
        "PDPOSGST-TOT" => Some("Treasury dealer net position ex-TIPS"),
        "PDSORA-UTSETTOT" => Some("Treasury repo agreements ex-TIPS"),
        "PDFTD-USTET" => Some("Treasury fails to deliver ex-TIPS"),
        "PDFTR-USTET" => Some("Treasury fails to receive ex-TIPS"),
        _ => None,
    }
}

pub async fn fetch_nyfed(req: NyFedRequest) -> Result<NyFedResponse> {
    let client = &*crate::finance::shared_client::GENERAL;
    let kind = req.kind.to_ascii_lowercase();
    let url = fetch_url_for_kind(&kind)?;

    // Fetch with 3-attempt retry
    let mut last_err = String::new();
    let mut body: Option<serde_json::Value> = None;

    for attempt in 0u64..3 {
        if attempt > 0 {
            sleep(TokioDuration::from_millis(500 * attempt)).await;
        }
        match client.get(url).send().await {
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
            "nyfed {kind}: failed after 3 attempts: {last_err}"
        ))
    })?;

    match kind.as_str() {
        "rates" => parse_rates(&json, &kind),
        "rrp" => parse_rrp(&json, &kind),
        "soma" => parse_soma(&json, &kind),
        "dealers" => parse_dealers(&json, &kind),
        _ => unreachable!(),
    }
}

fn parse_rates(json: &serde_json::Value, kind: &str) -> Result<NyFedResponse> {
    // NY Fed rates endpoint returns { refRates: [ { type, effectiveDate, ... } ] }
    let items = json
        .get("refRates")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            Error::Provider(format!(
                "nyfed {kind}: missing 'refRates' array in response"
            ))
        })?;

    let mut rates = Vec::with_capacity(items.len());
    for item in items {
        let name = parse_string(
            item.get("type")
                .or_else(|| item.get("name"))
                .unwrap_or(&serde_json::Value::Null),
        )
        .unwrap_or_default();

        let Some(rate_pct) = item
            .get("percentRate")
            .or_else(|| item.get("rate"))
            .and_then(parse_f64)
        else {
            // Skip SOFR averages/index rows here; this tool is for current rates.
            continue;
        };

        let effective_date = parse_string(
            item.get("effectiveDate")
                .unwrap_or(&serde_json::Value::Null),
        )
        .unwrap_or_default();

        let percentile_1 = item.get("percentPercentile1").and_then(parse_f64);
        let percentile_25 = item.get("percentPercentile25").and_then(parse_f64);
        let percentile_75 = item.get("percentPercentile75").and_then(parse_f64);
        let percentile_99 = item.get("percentPercentile99").and_then(parse_f64);

        let volume_billions = item
            .get("volumeInBillions")
            .and_then(parse_f64)
            .or_else(|| {
                item.get("totalVolume")
                    .and_then(parse_f64)
                    .map(|v| v / 1_000_000_000.0)
            });

        rates.push(NyFedRateItem {
            name,
            rate_pct,
            percentile_1,
            percentile_25,
            percentile_75,
            percentile_99,
            volume_billions,
            effective_date,
        });
    }

    Ok(NyFedResponse {
        generated_at: Utc::now(),
        kind: kind.to_string(),
        rates: Some(rates),
        rrp: None,
        soma: None,
        dealers: None,
    })
}

fn parse_rrp(json: &serde_json::Value, kind: &str) -> Result<NyFedResponse> {
    // RRP endpoint returns { repo: { operations: [ ... ] } } or similar nested structure
    let operations = json
        .get("repo")
        .and_then(|r| r.get("operations"))
        .and_then(|v| v.as_array())
        .or_else(|| {
            // Fallback: try top-level array structures
            json.get("operations").and_then(|v| v.as_array())
        })
        .or_else(|| json.as_array())
        .ok_or_else(|| {
            Error::Provider(format!(
                "nyfed {kind}: could not find operations array in response"
            ))
        })?;

    let mut rrp_items = Vec::with_capacity(operations.len());
    for op in operations {
        let effective_date = parse_string(
            op.get("operationDate")
                .or_else(|| op.get("effectiveDate"))
                .or_else(|| op.get("dealDate"))
                .unwrap_or(&serde_json::Value::Null),
        )
        .unwrap_or_default();

        let total_raw = op
            .get("totalAmtAccepted")
            .or_else(|| op.get("totalAmt"))
            .or_else(|| op.get("totalAmount"))
            .and_then(parse_f64)
            .unwrap_or(0.0);

        // Convert to billions if value looks like it's in millions or raw dollars
        let total_billions = if total_raw > 1_000_000.0 {
            total_raw / 1_000_000_000.0
        } else if total_raw > 1_000.0 {
            total_raw / 1_000.0
        } else {
            total_raw
        };

        let counterparty_count = op
            .get("acceptedCpty")
            .or_else(|| op.get("participatingCpty"))
            .or_else(|| op.get("totalCounterpartiesAccepted"))
            .or_else(|| op.get("counterparties"))
            .or_else(|| op.get("participantCount"))
            .and_then(parse_u32)
            .unwrap_or(0);

        let rate_pct = op
            .get("percentAwardRate")
            .or_else(|| op.get("awardRate"))
            .or_else(|| op.get("percentOfferingRate"))
            .or_else(|| op.get("offeringRate"))
            .or_else(|| op.get("rate"))
            .and_then(parse_f64)
            .or_else(|| {
                op.get("details")
                    .and_then(|v| v.as_array())
                    .and_then(|rows| rows.first())
                    .and_then(|row| {
                        row.get("percentAwardRate")
                            .or_else(|| row.get("percentOfferingRate"))
                            .or_else(|| row.get("awardRate"))
                            .or_else(|| row.get("offeringRate"))
                            .and_then(parse_f64)
                    })
            })
            .unwrap_or(0.0);

        rrp_items.push(NyFedRrpItem {
            effective_date,
            total_billions,
            counterparty_count,
            rate_pct,
        });
    }

    Ok(NyFedResponse {
        generated_at: Utc::now(),
        kind: kind.to_string(),
        rates: None,
        rrp: Some(rrp_items),
        soma: None,
        dealers: None,
    })
}

fn parse_soma(json: &serde_json::Value, kind: &str) -> Result<NyFedResponse> {
    let summaries = json
        .get("soma")
        .and_then(|s| s.get("summary"))
        .and_then(|v| v.as_array())
        .or_else(|| json.get("summary").and_then(|v| v.as_array()))
        .ok_or_else(|| {
            Error::Provider(format!(
                "nyfed {kind}: could not find summary array in response"
            ))
        })?;

    let latest = summaries
        .iter()
        .max_by_key(|item| {
            parse_string(item.get("asOfDate").unwrap_or(&serde_json::Value::Null)).unwrap_or_default()
        })
        .ok_or_else(|| Error::Provider(format!("nyfed {kind}: summary array is empty")))?;

    let as_of_date = parse_string(latest.get("asOfDate").unwrap_or(&serde_json::Value::Null))
        .unwrap_or_default();
    let total_raw = latest
        .get("total")
        .and_then(parse_f64)
        .unwrap_or(0.0);

    let mut soma_items = Vec::new();
    for (field, label) in [
        ("bills", "Bills"),
        ("notesbonds", "Notes/Bonds"),
        ("tips", "TIPS"),
        ("frn", "FRN"),
        ("agencies", "Agencies"),
        ("mbs", "MBS"),
        ("cmbs", "CMBS"),
    ] {
        let Some(par_raw) = latest.get(field).and_then(parse_f64) else {
            continue;
        };
        if par_raw <= 0.0 {
            continue;
        }
        let par_value_billions = par_raw / 1_000_000_000.0;
        let percent_of_total = if total_raw > 0.0 {
            ((par_raw / total_raw * 100.0) * 100.0).round() / 100.0
        } else {
            0.0
        };
        soma_items.push(NyFedSomaItem {
            security_type: label.to_string(),
            par_value_billions,
            percent_of_total,
            as_of_date: as_of_date.clone(),
        });
    }

    Ok(NyFedResponse {
        generated_at: Utc::now(),
        kind: kind.to_string(),
        rates: None,
        rrp: None,
        soma: Some(soma_items),
        dealers: None,
    })
}

fn parse_dealers(json: &serde_json::Value, kind: &str) -> Result<NyFedResponse> {
    let items = json
        .get("pd")
        .and_then(|v| v.get("timeseries"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            Error::Provider(format!(
                "nyfed {kind}: could not find dealer data array in response"
            ))
        })?;

    let mut dealer_items: Vec<NyFedDealerItem> = Vec::new();
    for item in items {
        let keyid = parse_string(item.get("keyid").unwrap_or(&serde_json::Value::Null))
            .unwrap_or_default();
        let Some(description) = dealer_label(&keyid) else {
            continue;
        };

        // Preserve the upstream None — the prior `unwrap_or(0.0)` made retired
        // series (notably PDSORA-UTSETTOT repo, which now ships "*") read as a
        // hard zero in JSON, hiding the fact that the row no longer has a value.
        let value_millions = item.get("value").and_then(parse_f64);

        let report_date = parse_string(
            item.get("asofdate")
                .or_else(|| item.get("asOfDate"))
                .unwrap_or(&serde_json::Value::Null),
        )
        .unwrap_or_default();

        let candidate = NyFedDealerItem {
            description: description.to_string(),
            value_millions,
            report_date,
        };

        if let Some(existing) = dealer_items
            .iter_mut()
            .find(|existing| existing.description == candidate.description)
        {
            if candidate.report_date > existing.report_date {
                *existing = candidate;
            }
        } else {
            dealer_items.push(candidate);
        }
    }

    let order = [
        "Treasury dealer net position ex-TIPS",
        "Treasury repo agreements ex-TIPS",
        "Treasury fails to deliver ex-TIPS",
        "Treasury fails to receive ex-TIPS",
    ];
    dealer_items.sort_by_key(|item| {
        order
            .iter()
            .position(|label| *label == item.description)
            .unwrap_or(usize::MAX)
    });

    Ok(NyFedResponse {
        generated_at: Utc::now(),
        kind: kind.to_string(),
        rates: None,
        rrp: None,
        soma: None,
        dealers: Some(dealer_items),
    })
}
