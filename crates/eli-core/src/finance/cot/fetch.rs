use crate::{Error, Result};
use chrono::Utc;
use eli_finance_types::{CotPosition, CotRequest, CotResponse};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::time::{sleep, Duration};
use tracing::warn;

const DISAGGREGATED_ENDPOINT: &str = "https://publicreporting.cftc.gov/resource/72hh-3qpy.json";
const FINANCIAL_ENDPOINT: &str = "https://publicreporting.cftc.gov/resource/gpe5-46if.json";
const MAX_RETRIES: usize = 3;

fn parse_i64(val: &Value) -> i64 {
    match val {
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        Value::Number(n) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

fn str_field(row: &Value, key: &str) -> String {
    match &row[key] {
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn expand_query_aliases(query: &str) -> Vec<String> {
    let q = query.trim().to_ascii_lowercase();
    let mut terms = vec![q.clone()];

    let aliases: &[(&[&str], &[&str])] = &[
        // Treasury
        (
            &["10-year", "10y", "10yr", "ten year", "t-note"],
            &["ust 10y", "10-year"],
        ),
        (&["2-year", "2y", "2yr", "two year"], &["ust 2y", "2-year"]),
        (&["5-year", "5y", "5yr", "five year"], &["ust 5y", "5-year"]),
        (
            &["30-year", "30y", "30yr", "t-bond", "treasury bond"],
            &["ust bond", "ultra ust", "30-year"],
        ),
        (
            &["treasury", "treasuries"],
            &["ust ", "t-note", "t-bond", "ultra ust"],
        ),
        // Equity indices
        (
            &["s&p", "s&p 500", "sp500", "spx", "es"],
            &["s&p 500", "e-mini s&p"],
        ),
        (&["nasdaq", "nq"], &["nasdaq", "e-mini nasdaq"]),
        (&["dow", "djia", "ym"], &["djia", "dow jones"]),
        (&["russell", "rut"], &["russell", "e-mini russell"]),
        // Energy
        (
            &["crude", "wti", "oil", "cl"],
            &["crude oil", "wti-physical", "wti financial"],
        ),
        (
            &["nat gas", "natural gas", "ng", "natgas"],
            &["natural gas", "nat gas"],
        ),
        (&["brent"], &["brent"]),
        // Metals
        (&["gold", "gc", "xau"], &["gold"]),
        (&["silver", "si", "xag"], &["silver"]),
        (&["copper", "hg"], &["copper"]),
        (&["platinum", "pl"], &["platinum"]),
        // Ags
        (&["corn", "zc"], &["corn"]),
        (&["wheat", "zw"], &["wheat"]),
        (&["soybean", "soybeans", "zs"], &["soybean"]),
        // FX
        (&["euro", "eur", "eurusd"], &["euro fx", "euro"]),
        (&["yen", "jpy", "usdjpy"], &["japanese yen", "yen"]),
        (&["pound", "gbp", "cable"], &["british pound", "pound"]),
        (&["swiss", "chf", "franc"], &["swiss franc"]),
        (&["aussie", "aud"], &["australian dollar"]),
        (&["cad", "loonie"], &["canadian dollar"]),
        // Rates
        (
            &["eurodollar", "sofr", "fed funds"],
            &["sofr", "fed funds", "eurodollar"],
        ),
        (&["vix", "volatility"], &["vix", "volatility"]),
        (&["bitcoin", "btc"], &["bitcoin"]),
    ];

    for (triggers, expansions) in aliases {
        if triggers.iter().any(|t| q == *t || q.contains(t)) {
            for exp in *expansions {
                let e = exp.to_ascii_lowercase();
                if !terms.contains(&e) {
                    terms.push(e);
                }
            }
        }
    }

    terms
}

fn contract_priority_score(query: Option<&str>, contract_name: &str) -> i32 {
    let Some(query) = query else {
        return 0;
    };
    let q = query.trim().to_ascii_lowercase();
    let name = contract_name.to_ascii_lowercase();

    if ["crude", "wti", "oil", "cl"]
        .iter()
        .any(|needle| q == *needle || q.contains(needle))
    {
        if name.contains("new york mercantile exchange") {
            return 100;
        }
        if name.contains("ice futures europe") {
            return 50;
        }
        if name.contains("ice futures energy div") {
            return 40;
        }
    }

    // Natural gas: prefer Henry Hub (NYMEX) and ICE LD1 over regional basis/NGL contracts
    if ["nat gas", "natural gas", "ng", "natgas"]
        .iter()
        .any(|needle| q == *needle || q.contains(needle))
    {
        // ICE LD1 (Henry Hub financial) — largest OI nat gas contract globally
        if name.contains("nat gas ice ld1") {
            return 100;
        }
        // NYMEX Henry Hub benchmark (physical) — "NAT GAS NYME" or "HENRY HUB"
        if (name.contains("nat gas") || name.contains("henry hub"))
            && name.contains("new york mercantile exchange")
        {
            return 95;
        }
        // ICE penultimate (Henry Hub financial variant)
        if name.contains("nat gas ice pen") {
            return 80;
        }
        // Penalize basis/regional/NGL contracts that match "natural gas" broadly
        if name.contains("basis")
            || name.contains("differential")
            || name.contains("swing")
            || name.contains("propane")
            || name.contains("butane")
            || name.contains("ethane")
            || name.contains("gasoline")
        {
            return -50;
        }
    }

    0
}

/// Given a query, guess the best default report type.
/// Equity indices, FX, and rates are in the financial report; commodities in disaggregated.
fn infer_report_type(query: &str) -> &'static str {
    let q = query.trim().to_ascii_lowercase();
    const FINANCIAL_HINTS: &[&str] = &[
        "s&p",
        "sp500",
        "spx",
        "es",
        "nasdaq",
        "nq",
        "dow",
        "djia",
        "ym",
        "russell",
        "rut",
        "euro",
        "eur",
        "yen",
        "jpy",
        "pound",
        "gbp",
        "swiss",
        "chf",
        "aussie",
        "aud",
        "cad",
        "loonie",
        "bitcoin",
        "btc",
        "sofr",
        "eurodollar",
        "fed funds",
        "vix",
        "ust ",
        "ust 10y",
        "ust 2y",
        "ust 5y",
        "ust bond",
        "treasury",
        "treasuries",
        "10-year",
        "10y",
        "10yr",
        "2-year",
        "2y",
        "2yr",
        "5-year",
        "5y",
        "5yr",
        "30-year",
        "30y",
        "30yr",
        "t-note",
        "t-bond",
    ];
    if FINANCIAL_HINTS.iter().any(|h| q == *h || q.contains(h)) {
        "financial"
    } else {
        "disaggregated"
    }
}

pub async fn fetch_cot(req: CotRequest) -> Result<CotResponse> {
    let weeks = req.weeks.unwrap_or(12);
    let report = match req.report.as_deref() {
        Some(r) if !r.is_empty() => r,
        _ => req
            .query
            .as_deref()
            .map_or("disaggregated", infer_report_type),
    };

    let endpoint = match report {
        "financial" => FINANCIAL_ENDPOINT,
        _ => DISAGGREGATED_ENDPOINT,
    };

    let cutoff = Utc::now() - chrono::Duration::days(weeks as i64 * 7);
    let cutoff_str = cutoff.format("%Y-%m-%d").to_string();

    let where_clause = format!("report_date_as_yyyy_mm_dd >= '{cutoff_str}'");
    let url = format!(
        "{}?$where={}&$limit=5000&$order=report_date_as_yyyy_mm_dd DESC",
        endpoint,
        urlencoding::encode(&where_clause),
    );

    let rows = fetch_with_retry(&url).await?;

    // Filter by query if provided.
    // Search both market_and_exchange_names and commodity_name (CFTC renames contracts).
    let filtered: Vec<&Value> = if let Some(ref q) = req.query {
        let search_terms = expand_query_aliases(q);
        rows.iter()
            .filter(|row| {
                let name = str_field(row, "market_and_exchange_names").to_ascii_lowercase();
                let commodity = str_field(row, "commodity_name").to_ascii_lowercase();
                search_terms
                    .iter()
                    .any(|term| name.contains(term) || commodity.contains(term))
            })
            .collect()
    } else {
        rows.iter().collect()
    };

    // Group by contract name, collecting positions sorted by date desc.
    let mut by_contract: BTreeMap<String, Vec<CotPosition>> = BTreeMap::new();

    let is_financial = report == "financial";

    for row in &filtered {
        let contract_name = str_field(row, "market_and_exchange_names");
        let report_date = str_field(row, "report_date_as_yyyy_mm_dd");
        let open_interest = parse_i64(&row["open_interest_all"]);
        let commodity_name = {
            let s = str_field(row, "commodity_name");
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let futonly_or_combined = {
            let s = str_field(row, "futonly_or_combined");
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };

        let (spec_long, spec_short) = if is_financial {
            (
                parse_i64(&row["lev_money_positions_long"]),
                parse_i64(&row["lev_money_positions_short"]),
            )
        } else {
            (
                parse_i64(&row["m_money_positions_long_all"]),
                parse_i64(&row["m_money_positions_short_all"]),
            )
        };

        let (commercial_long, commercial_short) = if is_financial {
            (
                parse_i64(&row["dealer_positions_long_all"]),
                parse_i64(&row["dealer_positions_short_all"]),
            )
        } else {
            (
                parse_i64(&row["prod_merc_positions_long"]),
                parse_i64(&row["prod_merc_positions_short"]),
            )
        };

        let spec_net = spec_long - spec_short;
        let commercial_net = commercial_long - commercial_short;

        let spec_net_pct_oi = if open_interest > 0 {
            Some((spec_net as f64 / open_interest as f64 * 100.0 * 100.0).round() / 100.0)
        } else {
            None
        };

        let pos = CotPosition {
            contract_name: contract_name.clone(),
            report_date,
            open_interest,
            spec_net,
            spec_long,
            spec_short,
            commercial_net,
            commercial_long,
            commercial_short,
            spec_net_pct_oi,
            spec_net_change: None, // computed below
            report_family: Some(report.to_string()),
            futonly_or_combined,
            commodity_name,
        };

        by_contract.entry(contract_name).or_default().push(pos);
    }

    // Sort each contract's positions by date descending and compute week-over-week change.
    let mut all_positions: Vec<CotPosition> = Vec::new();
    let mut contracts_found = 0usize;

    for (_name, mut positions) in by_contract {
        positions.sort_by(|a, b| b.report_date.cmp(&a.report_date));
        contracts_found += 1;

        for i in 0..positions.len() {
            if i + 1 < positions.len() {
                positions[i].spec_net_change =
                    Some(positions[i].spec_net - positions[i + 1].spec_net);
            }
        }
        all_positions.extend(positions);
    }

    // Sort by open interest descending (biggest contracts first), then date desc.
    // This ensures the most relevant contracts (e.g. WTI on NYMEX) appear before
    // minor contracts (e.g. TMX differentials) that happen to match the query.
    all_positions.sort_by(|a, b| {
        // Group by contract, rank groups by max OI
        a.contract_name
            .cmp(&b.contract_name)
            .then(b.report_date.cmp(&a.report_date))
    });
    // Re-order contract groups by their latest OI descending.
    // Deprioritize contracts where all spec positions are 0 (no managed money breakdown).
    {
        let mut contract_oi: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        let mut contract_has_spec: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        for p in &all_positions {
            let entry = contract_oi.entry(p.contract_name.clone()).or_insert(0);
            *entry = (*entry).max(p.open_interest);
            if p.spec_long != 0 || p.spec_short != 0 {
                contract_has_spec.insert(p.contract_name.clone(), true);
            }
        }
        let query = req.query.as_deref();
        let mut order_vec: Vec<(String, i64, i32)> = contract_oi
            .into_iter()
            .map(|(name, oi)| {
                let mut priority = contract_priority_score(query, &name);
                // Deprioritize contracts with no spec data (all managed money = 0)
                if !contract_has_spec.get(&name).copied().unwrap_or(false) {
                    priority -= 200;
                }
                (name, oi, priority)
            })
            .collect();
        order_vec.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        let rank: std::collections::HashMap<String, usize> = order_vec
            .into_iter()
            .enumerate()
            .map(|(i, (name, _, _))| (name, i))
            .collect();
        all_positions.sort_by(|a, b| {
            let ra = rank.get(&a.contract_name).copied().unwrap_or(999);
            let rb = rank.get(&b.contract_name).copied().unwrap_or(999);
            ra.cmp(&rb).then(b.report_date.cmp(&a.report_date))
        });
    }

    // Cap to top N contracts by priority+OI to avoid flooding results with regional noise.
    let max_contracts = req.limit.unwrap_or(15);
    if contracts_found > max_contracts {
        // Collect the top N contract names from the already-sorted order.
        let mut seen = std::collections::HashSet::new();
        let mut top_names = Vec::new();
        for p in &all_positions {
            if seen.insert(p.contract_name.clone()) {
                top_names.push(p.contract_name.clone());
                if top_names.len() >= max_contracts {
                    break;
                }
            }
        }
        let top_set: std::collections::HashSet<&str> =
            top_names.iter().map(|s| s.as_str()).collect();
        all_positions.retain(|p| top_set.contains(p.contract_name.as_str()));
        contracts_found = top_names.len();
    }

    // Compute freshness metadata from the latest position date.
    // CFTC reports positions as-of Tuesday, released the following Friday at 3:30 PM ET.
    let (data_as_of, released_on, next_release, staleness) =
        if let Some(latest) = all_positions.first() {
            let as_of = &latest.report_date;
            // Parse the as-of date to compute release dates
            if let Ok(as_of_date) = chrono::NaiveDate::parse_from_str(&as_of[..10], "%Y-%m-%d") {
                let now = Utc::now().date_naive();
                // Released Friday after the as-of Tuesday (3 days later)
                let released = as_of_date + chrono::Duration::days(3);
                // Next release is the following Friday (10 days after as-of, i.e. 7 days after released)
                let next = released + chrono::Duration::days(7);
                let days_stale = (now - as_of_date).num_days();
                let stale_str = if days_stale <= 3 {
                    format!("{}d old (current — released this week)", days_stale)
                } else if days_stale <= 7 {
                    format!("{}d old (last week's positions)", days_stale)
                } else {
                    format!("{}d old (stale — multiple weeks behind)", days_stale)
                };
                (
                    Some(as_of_date.format("%Y-%m-%d").to_string()),
                    Some(released.format("%Y-%m-%d (Fri 3:30 PM ET)").to_string()),
                    Some(next.format("%Y-%m-%d (Fri 3:30 PM ET)").to_string()),
                    Some(stale_str),
                )
            } else {
                (Some(as_of[..10].to_string()), None, None, None)
            }
        } else {
            (None, None, None, None)
        };

    Ok(CotResponse {
        generated_at: Utc::now(),
        report_type: report.to_string(),
        positions: all_positions,
        contracts_found,
        query: req.query,
        data_as_of,
        released_on,
        next_release,
        staleness,
    })
}

#[cfg(test)]
mod tests {
    use super::contract_priority_score;

    #[test]
    fn crude_query_prefers_nymex_over_ice() {
        assert!(
            contract_priority_score(
                Some("crude"),
                "WTI FINANCIAL CRUDE OIL - NEW YORK MERCANTILE EXCHANGE"
            ) > contract_priority_score(
                Some("crude"),
                "CRUDE OIL, LIGHT SWEET-WTI - ICE FUTURES EUROPE"
            )
        );
    }
}

async fn fetch_with_retry(url: &str) -> Result<Vec<Value>> {
    let client = &*crate::finance::shared_client::GENERAL;

    for attempt in 0..MAX_RETRIES {
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    if attempt + 1 < MAX_RETRIES {
                        warn!(
                            attempt,
                            status = %status,
                            "CFTC API returned error, retrying"
                        );
                        sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
                        continue;
                    }
                    return Err(Error::Provider(format!(
                        "CFTC API returned {status}: {body}"
                    )));
                }

                let rows: Vec<Value> = resp
                    .json()
                    .await
                    .map_err(|e| Error::Provider(format!("parse CFTC JSON: {e}")))?;
                return Ok(rows);
            }
            Err(e) => {
                if attempt + 1 < MAX_RETRIES {
                    warn!(attempt, err = %e, "CFTC API request failed, retrying");
                    sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
                    continue;
                }
                return Err(Error::Provider(format!("CFTC API request failed: {e}")));
            }
        }
    }

    Err(Error::Provider("CFTC API: exhausted retries".to_string()))
}
