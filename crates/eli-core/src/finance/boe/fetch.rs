use crate::{Error, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// Bank of England Statistical Interactive Database.
/// URL-parameter-based CSV downloads. No auth required.
///
/// Base: https://www.bankofengland.co.uk/boeapps/database/_iadb-fromshowcolumns.asp
/// Date format in request: DD/Mon/YYYY. Date in response: DD Mon YYYY.
/// Missing values: ".." (two dots).

const BOE_BASE: &str = "https://www.bankofengland.co.uk/boeapps/database/_iadb-fromshowcolumns.asp";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoeObservation {
    /// Observation date in YYYY-MM-DD format. Renamed from `date` to `period` to
    /// match ECB / BIS / BOJ / EIA observation field naming.
    pub period: String,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoeSeries {
    pub code: String,
    /// Provider-side identifier — the BOE Statistical Interactive Database series
    /// code (e.g. "IUDBEDR" for Bank Rate). This IS the canonical BOE identifier
    /// (kept as `key` per the cross-tool naming convention for round-trippable IDs).
    pub key: Option<String>,
    pub label: String,
    pub observations: Vec<BoeObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoeResponse {
    pub generated_at: DateTime<Utc>,
    pub preset: Option<String>,
    pub series: Vec<BoeSeries>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BoeRequest {
    pub preset: Option<BoePreset>,
    pub series_codes: Vec<String>,
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BoePreset {
    BankRate,
    Sonia,
    GiltYields,
    M4,
    Fx,
    All,
}

impl BoePreset {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bank_rate" | "rate" | "bankrate" => Some(Self::BankRate),
            "sonia" => Some(Self::Sonia),
            "gilt" | "gilts" | "yield" | "yields" => Some(Self::GiltYields),
            "m4" | "money" | "money_supply" => Some(Self::M4),
            "fx" | "gbpusd" | "forex" => Some(Self::Fx),
            "all" | "dashboard" => Some(Self::All),
            _ => None,
        }
    }

    fn series_codes(&self) -> Vec<(&'static str, &'static str)> {
        // (code, label)
        match self {
            Self::BankRate => vec![("IUDBEDR", "Bank Rate")],
            Self::Sonia => vec![("IUDSOIA", "SONIA")],
            Self::GiltYields => vec![
                ("IUDSNPY", "5Y Gilt Par Yield"),
                ("IUDMNPY", "10Y Gilt Par Yield"),
                ("IUDLNPY", "20Y Gilt Par Yield"),
            ],
            Self::M4 => vec![("LPMAUYN", "M4 Outstanding (NSA)")],
            Self::Fx => vec![
                ("XUDLUSS", "GBP/USD"),
                ("XUDLERS", "GBP/EUR"),
                ("XUDLJYS", "GBP/JPY"),
            ],
            Self::All => vec![
                ("IUDBEDR", "Bank Rate"),
                ("IUDSOIA", "SONIA"),
                ("IUDMNPY", "10Y Gilt Par Yield"),
                ("LPMAUYN", "M4 Outstanding (NSA)"),
                ("XUDLUSS", "GBP/USD"),
            ],
        }
    }
}

pub async fn fetch_boe(req: BoeRequest) -> Result<BoeResponse> {
    let client = &*crate::finance::shared_client::GENERAL;
    let mut warnings = Vec::new();

    let code_labels: Vec<(String, String)> = if let Some(ref preset) = req.preset {
        preset
            .series_codes()
            .into_iter()
            .map(|(c, l)| (c.to_string(), l.to_string()))
            .collect()
    } else if !req.series_codes.is_empty() {
        req.series_codes
            .iter()
            .map(|c| (c.clone(), c.clone()))
            .collect()
    } else {
        return Err(Error::InvalidInput(
            "boe requires --preset (bank_rate|sonia|gilts|m4|fx|all) or --codes".to_string(),
        ));
    };

    let codes_str = code_labels
        .iter()
        .map(|(c, _)| c.as_str())
        .collect::<Vec<_>>()
        .join(",");

    // BOE date format: DD/Mon/YYYY
    let start = req.start.as_deref().unwrap_or("01/Jan/2025");
    let end = req.end.as_deref().unwrap_or("now");

    let url = format!(
        "{}?csv.x=yes&Datefrom={}&Dateto={}&SeriesCodes={}&CSVF=TN&UsingCodes=Y&VPD=Y&VFD=N",
        BOE_BASE, start, end, codes_str
    );

    let resp = client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
        )
        .send()
        .await
        .map_err(|e| Error::Provider(format!("boe fetch failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Provider(format!("boe returned {}", resp.status())));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| Error::Provider(format!("boe body read failed: {e}")))?;

    // Parse TN CSV: first row is header "DATE,CODE1,CODE2,...", then data rows.
    let mut lines = body.lines();
    let header = match lines.next() {
        Some(h) => h,
        None => return Err(Error::Provider("boe returned empty response".to_string())),
    };

    let cols: Vec<&str> = header.split(',').collect();
    if cols.is_empty() || !cols[0].trim().eq_ignore_ascii_case("DATE") {
        return Err(Error::Provider(format!(
            "boe unexpected CSV header: {}",
            header.chars().take(100).collect::<String>()
        )));
    }

    // Map column index to code.
    let code_indices: Vec<(usize, String)> = cols
        .iter()
        .enumerate()
        .skip(1) // skip DATE column
        .map(|(i, c)| (i, c.trim().to_string()))
        .collect();

    let mut by_code: std::collections::BTreeMap<String, Vec<BoeObservation>> =
        std::collections::BTreeMap::new();

    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        if fields.is_empty() {
            continue;
        }

        // BOE date in response: "DD Mon YYYY" (e.g. "02 Jan 2025")
        let date_raw = fields[0].trim();
        let period = parse_boe_date(date_raw).unwrap_or_default();
        if period.is_empty() {
            continue;
        }

        for &(idx, ref code) in &code_indices {
            let val_raw = fields.get(idx).map(|s| s.trim()).unwrap_or("");
            if val_raw.is_empty() || val_raw == ".." {
                continue; // missing value
            }
            let value: f64 = match val_raw.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            by_code
                .entry(code.clone())
                .or_default()
                .push(BoeObservation {
                    period: period.clone(),
                    value,
                });
        }
    }

    let label_map: std::collections::HashMap<String, String> = code_labels
        .iter()
        .map(|(c, l)| (c.clone(), l.clone()))
        .collect();

    let series: Vec<BoeSeries> = by_code
        .into_iter()
        .map(|(code, obs)| {
            let label = label_map
                .get(&code)
                .cloned()
                .unwrap_or_else(|| code.clone());
            let key = if code.is_empty() {
                None
            } else {
                Some(code.clone())
            };
            BoeSeries {
                code,
                key,
                label,
                observations: obs,
            }
        })
        .collect();

    Ok(BoeResponse {
        generated_at: Utc::now(),
        preset: req.preset.as_ref().map(|p| format!("{:?}", p)),
        series,
        warnings,
    })
}

/// Parse BOE date "DD Mon YYYY" → "YYYY-MM-DD".
fn parse_boe_date(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() != 3 {
        return None;
    }
    let day: u32 = parts[0].parse().ok()?;
    let month = match parts[1].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i32 = parts[2].parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day).map(|d| d.format("%Y-%m-%d").to_string())
}
