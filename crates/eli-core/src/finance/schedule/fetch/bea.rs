/// Fetches macro event dates from the BEA (Bureau of Economic Analysis) JSON API.
/// No API key required. Returns GDP, PCE, Trade, Corporate Profits with exact UTC times.
///
/// Endpoint: https://apps.bea.gov/API/signup/release_dates.json
/// Structure: { "Report Name": { "release_dates": ["ISO8601"], "to_be_rescheduled": [...] } }

use chrono::NaiveDate;

/// Mapping from BEA report names to (short title, FRED release_id).
const BEA_TITLE_MAP: &[(&str, &str, Option<u32>)] = &[
    ("Gross Domestic Product", "Gross Domestic Product", Some(53)),
    ("Personal Income and Outlays", "Personal Income and Outlays", Some(54)),
    (
        "U.S. International Trade in Goods and Services",
        "International Trade",
        Some(51),
    ),
    ("Corporate Profits", "Corporate Profits", None),
    (
        "U.S. International Transactions",
        "International Transactions",
        None,
    ),
];

const BEA_URL: &str = "https://apps.bea.gov/API/signup/release_dates.json";

pub async fn fetch_bea_macro_events(
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> Result<Vec<MacroScheduleEvent>> {
    let client = &*crate::finance::shared_client::GENERAL;
    let resp = client
        .get(BEA_URL)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("BEA calendar fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!(
            "BEA calendar returned {}",
            resp.status()
        )));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("BEA calendar parse failed: {e}")))?;

    let mut events = Vec::new();

    for (bea_name, short_title, fred_id) in BEA_TITLE_MAP {
        let Some(report) = body.get(*bea_name) else {
            continue;
        };
        let Some(dates) = report.get("release_dates").and_then(|v| v.as_array()) else {
            continue;
        };

        for date_val in dates {
            let Some(date_str) = date_val.as_str() else {
                continue;
            };
            // Parse ISO 8601: "2026-03-27T13:30:00+00:00"
            let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date_str)
                .or_else(|_| chrono::DateTime::parse_from_str(date_str, "%Y-%m-%dT%H:%M:%S%:z"))
            else {
                continue;
            };
            let naive_date = dt.date_naive();
            if naive_date < start_date || naive_date > end_date {
                continue;
            }

            // Convert UTC time to ET for display (ET = UTC-5 or UTC-4 during DST).
            // BEA releases are typically 08:30 ET. We'll compute from the UTC hour.
            let utc_hour = dt.time().hour();
            let utc_min = dt.time().minute();
            let time_et = if utc_hour >= 4 && utc_hour <= 18 {
                // Approximate: try UTC-4 (EDT) first; if that gives pre-6am, use UTC-5 (EST)
                let edt_hour = utc_hour.saturating_sub(4);
                let est_hour = utc_hour.saturating_sub(5);
                // BEA typically releases between 8:00 and 10:00 ET
                let (h, label) = if edt_hour >= 7 && edt_hour <= 11 {
                    (edt_hour, "ET")
                } else {
                    (est_hour, "ET")
                };
                Some(format!("{:02}:{:02} {}", h, utc_min, label))
            } else {
                None
            };

            events.push(MacroScheduleEvent {
                date: naive_date.to_string(),
                time: time_et,
                title: short_title.to_string(),
                release_id: *fred_id,
                release_url: Some("https://www.bea.gov/news/schedule".to_string()),
                source: "bea".to_string(),
            });
        }
    }

    events.sort_by(|a, b| a.date.cmp(&b.date).then(a.title.cmp(&b.title)));
    Ok(events)
}

use chrono::Timelike as _;
