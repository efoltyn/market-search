//! Business-entity lookups for states that publish their register on Socrata.
//!
//! Five states put a Secretary of State (or Comptroller) register on an open
//! Socrata portal, so one client serves all of them; only the column names and
//! the row shape differ. Each dataset is a periodic mirror, not the register
//! itself, and each covers a different slice — so every response names the
//! dataset and what it does and does not carry, especially whether it has the
//! registered agent, which is what a process server actually needs.

use super::{normalize_state, EntityRecord, EntityRequest, EntityResponse};
use crate::legal::shared_client;
use crate::{Error, Result};
use serde_json::Value;

const MAX_LIMIT: usize = 50;
/// Oregon and Pennsylvania return one row per address or officer, so fetch a
/// wider window than the answer and fold rows into entities.
const ROW_WINDOW: usize = 400;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// One row per entity.
    Flat,
    /// Several rows per entity, keyed by the id column.
    Grouped,
}

struct Source {
    code: &'static str,
    endpoint: &'static str,
    dataset_page: &'static str,
    source: &'static str,
    name_col: &'static str,
    id_col: &'static str,
    order: &'static str,
    shape: Shape,
    /// What this dataset is and is not — said on every answer.
    caveat: &'static str,
    map: fn(&[&Value]) -> EntityRecord,
}

pub(super) const STATES: &[&str] = &["ct", "ny", "or", "pa", "tx"];

static SOURCES: &[Source] = &[
    Source {
        code: "ct",
        endpoint: "https://data.ct.gov/resource/n7gp-d28j.json",
        dataset_page: "https://data.ct.gov/d/n7gp-d28j",
        source: "Connecticut Business Registry (data.ct.gov, dataset n7gp-d28j)",
        name_col: "name",
        id_col: "accountnumber",
        order: "date_registration DESC",
        shape: Shape::Flat,
        caveat: "Connecticut's open register carries status and the business address but not the \
                 registered agent; confirm the agent at https://service.ct.gov/business/s/onlinebusinesssearch",
        map: |rows| {
            let r = rows[0];
            EntityRecord {
                entity_id: s(r, "accountnumber"),
                entity_name: s(r, "name"),
                status: match (s(r, "status"), s(r, "sub_status")) {
                    (Some(a), Some(b)) => Some(format!("{a} ({b})")),
                    (a, _) => a,
                },
                entity_type: s(r, "business_type"),
                formation_date: date(r, "date_registration"),
                jurisdiction: s(r, "state_or_territory_formation").or_else(|| s(r, "formation_place")),
                principal_address: join(r, &["billingstreet", "billing_unit", "billingcity", "billingstate", "billingpostalcode"]),
                agent_name: None,
                agent_address: None,
                url: None,
            }
        },
    },
    Source {
        code: "ny",
        endpoint: "https://data.ny.gov/resource/n9v6-gdp6.json",
        dataset_page: "https://data.ny.gov/d/n9v6-gdp6",
        source: "New York Department of State, Active Corporations (data.ny.gov, dataset n9v6-gdp6)",
        name_col: "current_entity_name",
        id_col: "dos_id",
        order: "initial_dos_filing_date DESC",
        shape: Shape::Flat,
        caveat: "New York's dataset lists ACTIVE entities only, so a miss can mean dissolved or inactive; \
                 the 'agent' here is the DOS service-of-process address (BCL § 306 service on the \
                 Secretary of State is forwarded there), with a registered agent only where one was designated. \
                 Confirm at https://apps.dos.ny.gov/publicInquiry/",
        map: |rows| {
            let r = rows[0];
            EntityRecord {
                entity_id: s(r, "dos_id"),
                entity_name: s(r, "current_entity_name"),
                status: Some("Active".to_string()),
                entity_type: s(r, "entity_type"),
                formation_date: date(r, "initial_dos_filing_date"),
                jurisdiction: s(r, "jurisdiction"),
                principal_address: join(r, &["location_address_1", "location_address_2", "location_city", "location_state", "location_zip"]),
                agent_name: s(r, "registered_agent_name").or_else(|| s(r, "dos_process_name")),
                agent_address: join(r, &["registered_agent_address_1", "registered_agent_address_2", "registered_agent_city", "registered_agent_state", "registered_agent_zip"])
                    .or_else(|| join(r, &["dos_process_address_1", "dos_process_address_2", "dos_process_city", "dos_process_state", "dos_process_zip"])),
                url: None,
            }
        },
    },
    Source {
        code: "or",
        endpoint: "https://data.oregon.gov/resource/tckn-sxa6.json",
        dataset_page: "https://data.oregon.gov/d/tckn-sxa6",
        source: "Oregon Secretary of State, Active Businesses (data.oregon.gov, dataset tckn-sxa6)",
        name_col: "business_name",
        id_col: "registry_number",
        order: "registry_date DESC",
        shape: Shape::Grouped,
        caveat: "Oregon's dataset lists ACTIVE businesses only; the registered agent comes from its \
                 REGISTERED AGENT row. Confirm on the linked Business Registry record.",
        map: |rows| {
            let r = rows[0];
            let by_type = |t: &str| rows.iter().copied().find(|x| s(x, "associated_name_type").as_deref() == Some(t));
            let addr = |x: &Value| join(x, &["address", "address_continued", "city", "state", "zip"]);
            EntityRecord {
                entity_id: s(r, "registry_number"),
                entity_name: s(r, "business_name"),
                status: Some("Active".to_string()),
                entity_type: s(r, "entity_type"),
                formation_date: date(r, "registry_date"),
                jurisdiction: s(r, "jurisdiction"),
                principal_address: by_type("PRINCIPAL PLACE OF BUSINESS").and_then(addr),
                agent_name: by_type("REGISTERED AGENT").and_then(|a| {
                    s(a, "entity_of_record_name").or_else(|| join(a, &["first_name", "middle_name", "last_name"]))
                }),
                agent_address: by_type("REGISTERED AGENT").and_then(addr),
                url: r.get("business_details").and_then(|d| d.get("url")).and_then(Value::as_str).map(str::to_string),
            }
        },
    },
    Source {
        code: "pa",
        endpoint: "https://data.pa.gov/resource/xvd7-5r2c.json",
        dataset_page: "https://data.pa.gov/d/xvd7-5r2c",
        source: "Pennsylvania Department of State, Registered Businesses (data.pa.gov, dataset xvd7-5r2c)",
        name_col: "business_name",
        id_col: "filing_number",
        order: "creationdate DESC",
        shape: Shape::Grouped,
        caveat: "Pennsylvania's dataset lists the registered office address and named officers, not a \
                 status column or a commercial registered office provider; confirm at \
                 https://file.dos.pa.gov/search/business",
        map: |rows| {
            let r = rows[0];
            let officers: Vec<String> = rows
                .iter()
                .filter_map(|x| {
                    let who = join_sep(x, &["first_name", "middle_name", "last_name"], " ")?;
                    Some(match s(x, "party_type") {
                        Some(t) => format!("{who} ({t})"),
                        None => who,
                    })
                })
                .collect();
            EntityRecord {
                entity_id: s(r, "filing_number"),
                entity_name: s(r, "business_name"),
                status: None,
                entity_type: s(r, "typeofbusinessregistration"),
                formation_date: date(r, "creationdate"),
                jurisdiction: None,
                principal_address: join(r, &["address_line1", "address_line2", "city", "state", "zip"]),
                // The registered office is where PA process goes; officers are
                // listed alongside because the dataset has no agent column.
                agent_name: (!officers.is_empty()).then(|| format!("officers: {}", officers.join("; "))),
                agent_address: join(r, &["address_line1", "address_line2", "city", "state", "zip"]),
                url: None,
            }
        },
    },
    Source {
        code: "tx",
        endpoint: "https://data.texas.gov/resource/9cir-efmm.json",
        dataset_page: "https://data.texas.gov/d/9cir-efmm",
        source: "Texas Comptroller, Active Franchise Tax Permit Holders (data.texas.gov, dataset 9cir-efmm)",
        name_col: "taxpayer_name",
        id_col: "secretary_of_state_sos_or_coa_file_number",
        order: "sos_charter_date DESC",
        shape: Shape::Flat,
        caveat: "This is the Comptroller's franchise-tax roll, not the Secretary of State register: it \
                 has the SOS file number and right-to-transact status but NOT the registered agent. \
                 Get the agent from SOSDirect or the Comptroller's taxable-entity search \
                 (https://comptroller.texas.gov/taxes/franchise/account-status/search) using the file number.",
        map: |rows| {
            let r = rows[0];
            EntityRecord {
                entity_id: s(r, "secretary_of_state_sos_or_coa_file_number").or_else(|| s(r, "taxpayer_number")),
                entity_name: s(r, "taxpayer_name"),
                status: s(r, "right_to_transact_business_code").map(|c| match c.as_str() {
                    "A" => "Active — right to transact business".to_string(),
                    "D" => "Forfeited — right to transact business forfeited".to_string(),
                    "N" => "Not established".to_string(),
                    "U" => "Not established (no right to transact)".to_string(),
                    other => other.to_string(),
                }),
                entity_type: s(r, "taxpayer_organizational_type"),
                formation_date: date(r, "sos_charter_date"),
                jurisdiction: s(r, "taxpayer_state"),
                principal_address: join(r, &["taxpayer_address", "taxpayer_city", "taxpayer_state", "taxpayer_zip"]),
                agent_name: None,
                agent_address: None,
                url: None,
            }
        },
    },
];

pub(super) async fn fetch_entity(req: EntityRequest) -> Result<EntityResponse> {
    let code = normalize_state(&req.state);
    let src = SOURCES
        .iter()
        .find(|s| s.code == code)
        .ok_or_else(|| Error::InvalidInput(format!("no Socrata entity source for {code:?}")))?;

    let name = req.name.as_deref().map(str::trim).filter(|x| !x.is_empty());
    let id = req
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|x| !x.is_empty());
    let clause = match (id, name) {
        (Some(id), _) => format!("{} = '{}'", src.id_col, esc(id)),
        (None, Some(n)) => format!("upper({}) like upper('%{}%')", src.name_col, esc(n)),
        (None, None) => {
            return Err(Error::InvalidInput(format!(
                "an entity lookup needs a name (substring, case-insensitive) or the state's entity id ({})",
                src.id_col
            )))
        }
    };
    let limit = req.limit.clamp(1, MAX_LIMIT);
    let window = if src.shape == Shape::Grouped {
        ROW_WINDOW
    } else {
        limit.max(100)
    };

    let mut out = EntityResponse {
        generated_at: chrono::Utc::now(),
        state: code.clone(),
        returned: 0,
        entities: Vec::new(),
        source: Some(src.source.to_string()),
        source_url: Some(src.dataset_page.to_string()),
        warnings: vec![src.caveat.to_string()],
    };
    if req.status.as_deref().is_some_and(|s| !s.trim().is_empty()) {
        out.warnings.push(
            "status filtering is not applied for this state; filter the returned status yourself"
                .into(),
        );
    }

    let resp = shared_client::GENERAL
        .get(src.endpoint)
        .query(&[
            ("$where", clause.as_str()),
            ("$limit", &window.to_string()),
            ("$order", src.order),
        ])
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            out.warnings
                .push(format!("entity search request failed: {e}"));
            return Ok(out);
        }
    };
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // A rejected query must never read as "not registered".
        out.warnings.push(format!(
            "entity search failed with HTTP {status}; this is NOT a finding that the entity is unregistered — {}",
            body.chars().take(300).collect::<String>()
        ));
        return Ok(out);
    }
    let rows: Vec<Value> = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            out.warnings.push(format!(
                "entity search returned non-JSON ({e}); treating as no answer"
            ));
            return Ok(out);
        }
    };
    if rows.len() >= window {
        out.warnings.push(format!(
            "the search filled its {window}-row window, so matches may be missing; use a more specific name"
        ));
    }

    let mut records = group(&rows, src);
    if let Some(n) = name {
        let n = n.to_ascii_lowercase();
        // Exact name first, then names that start with the query, then the rest.
        records.sort_by_key(|r| {
            let rn = r
                .entity_name
                .clone()
                .unwrap_or_default()
                .to_ascii_lowercase();
            (rn != n, !rn.starts_with(&n), rn.len())
        });
    }
    let matched = records.len();
    records.truncate(limit);
    if matched > records.len() {
        out.warnings.push(format!(
            "{matched} entities matched; showing {} — narrow the name or raise limit",
            records.len()
        ));
    }
    if records.is_empty() {
        out.warnings.push(
            "no match in this dataset. Try a shorter or differently punctuated name (\"Inc\" vs \"Inc.\"), \
             and remember the caveat above about what this dataset covers"
                .into(),
        );
    }
    out.returned = records.len();
    out.entities = records;
    Ok(out)
}

/// Fold rows into entities, keeping first-seen order (the query is ordered).
fn group(rows: &[Value], src: &Source) -> Vec<EntityRecord> {
    if src.shape == Shape::Flat {
        return rows.iter().map(|r| (src.map)(&[r])).collect();
    }
    let mut order: Vec<String> = Vec::new();
    let mut buckets: std::collections::HashMap<String, Vec<&Value>> =
        std::collections::HashMap::new();
    for r in rows {
        let key = s(r, src.id_col).unwrap_or_default();
        if !buckets.contains_key(&key) {
            order.push(key.clone());
        }
        buckets.entry(key).or_default().push(r);
    }
    order.iter().map(|k| (src.map)(&buckets[k])).collect()
}

fn esc(input: &str) -> String {
    input.replace('\'', "''")
}

fn s(row: &Value, key: &str) -> Option<String> {
    row.get(key)
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_number().map(|n| n.to_string()))
        })
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn date(row: &Value, key: &str) -> Option<String> {
    // Socrata floating timestamps; CT uses 0001-01-01 for "no date".
    s(row, key)
        .map(|d| d.chars().take(10).collect::<String>())
        .filter(|d| !d.starts_with("0001"))
}

fn join(row: &Value, keys: &[&str]) -> Option<String> {
    join_sep(row, keys, ", ")
}

fn join_sep(row: &Value, keys: &[&str], sep: &str) -> Option<String> {
    let parts: Vec<String> = keys.iter().filter_map(|k| s(row, k)).collect();
    (!parts.is_empty()).then(|| parts.join(sep))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(code: &str) -> &'static Source {
        SOURCES.iter().find(|s| s.code == code).unwrap()
    }

    #[test]
    fn every_listed_state_has_a_source() {
        for c in STATES {
            assert!(SOURCES.iter().any(|s| s.code == *c), "{c}");
        }
        assert_eq!(STATES.len(), SOURCES.len());
    }

    #[test]
    fn oregon_rows_fold_into_one_entity_with_its_agent() {
        let rows: Vec<Value> = serde_json::from_str(
            r#"[
            {"registry_number":"8739914","business_name":"NIKE, INC.","entity_type":"DOMESTIC BUSINESS CORPORATION","registry_date":"1969-09-08T00:00:00.000","associated_name_type":"PRINCIPAL PLACE OF BUSINESS","address":"ONE BOWERMAN DRIVE","city":"BEAVERTON","state":"OR","zip":"97005","jurisdiction":"OR"},
            {"registry_number":"8739914","business_name":"NIKE, INC.","associated_name_type":"REGISTERED AGENT","entity_of_record_name":"UNITED AGENT GROUP INC.","address":"7185 SW SANDBURG ST STE 110","city":"PORTLAND","state":"OR","zip":"97223"}
        ]"#,
        )
        .unwrap();
        let recs = group(&rows, src("or"));
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].agent_name.as_deref(),
            Some("UNITED AGENT GROUP INC.")
        );
        assert_eq!(
            recs[0].agent_address.as_deref(),
            Some("7185 SW SANDBURG ST STE 110, PORTLAND, OR, 97223")
        );
        assert_eq!(
            recs[0].principal_address.as_deref(),
            Some("ONE BOWERMAN DRIVE, BEAVERTON, OR, 97005")
        );
        assert_eq!(recs[0].formation_date.as_deref(), Some("1969-09-08"));
    }

    #[test]
    fn new_york_falls_back_to_the_process_address() {
        let row: Value = serde_json::from_str(
            r#"{"dos_id":"4424185","current_entity_name":"BUTCHY'S WINE & SPIRITS, INC.","dos_process_name":"BUTCHY'S WINE & SPIRITS, INC.","dos_process_address_1":"23 BUSHNEIL PLACE","dos_process_city":"MOUNT VERNON","dos_process_state":"NY","dos_process_zip":"10550"}"#,
        )
        .unwrap();
        let r = (src("ny").map)(&[&row]);
        assert_eq!(
            r.agent_address.as_deref(),
            Some("23 BUSHNEIL PLACE, MOUNT VERNON, NY, 10550")
        );
    }

    #[test]
    fn connecticut_placeholder_date_is_dropped() {
        let row: Value =
            serde_json::from_str(r#"{"name":"X","date_registration":"0001-01-01T00:00:00.000"}"#)
                .unwrap();
        assert_eq!((src("ct").map)(&[&row]).formation_date, None);
    }

    #[test]
    fn quotes_in_names_are_escaped() {
        assert_eq!(esc("O'Brien's"), "O''Brien''s");
    }
}
