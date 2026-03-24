use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// BOJ Time-Series Data API (launched Feb 2026).
/// Base: https://www.stat-search.boj.or.jp/api/v1/getDataCode
/// No auth required. JSON response with SURVEY_DATES + VALUES parallel arrays.

const BOJ_BASE: &str = "https://www.stat-search.boj.or.jp/api/v1/getDataCode";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BojObservation {
    pub period: String,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BojSeries {
    pub code: String,
    pub name: String,
    pub unit: String,
    pub frequency: String,
    pub observations: Vec<BojObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BojResponse {
    pub generated_at: DateTime<Utc>,
    pub preset: Option<String>,
    pub series: Vec<BojSeries>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BojRequest {
    pub preset: Option<BojPreset>,
    pub db: Option<String>,
    pub codes: Vec<String>,
    pub start_date: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BojPreset {
    PolicyRate,
    CallRate,
    MonetaryBase,
    BalanceSheet,
    MoneyStock,
    Tankan,
    Fx,
}

impl BojPreset {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "policy_rate" | "rate" | "discount" => Some(Self::PolicyRate),
            "call_rate" | "call" | "overnight" => Some(Self::CallRate),
            "monetary_base" | "base" | "mb" => Some(Self::MonetaryBase),
            "balance_sheet" | "bs" | "assets" => Some(Self::BalanceSheet),
            "money_stock" | "m2" | "m3" | "money" => Some(Self::MoneyStock),
            "tankan" | "survey" => Some(Self::Tankan),
            "fx" | "usdjpy" | "forex" => Some(Self::Fx),
            _ => None,
        }
    }

    fn queries(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        // (db, codes_comma_sep, label)
        match self {
            Self::PolicyRate => vec![("IR01", "MADR1Z@D", "BOJ Basic Discount Rate (daily)")],
            Self::CallRate => vec![("FM01", "STRDCLUCON", "Call Rate (daily avg)")],
            Self::MonetaryBase => vec![("MD01", "MABS1AN11", "Monetary Base (monthly)")],
            Self::BalanceSheet => vec![("BS01", "MABJMTA", "BOJ Total Assets (monthly)")],
            Self::MoneyStock => vec![
                ("MD02", "MAM1NAM2M2MO", "M2 Money Stock"),
                ("MD02", "MAM1NAM3M3MO", "M3 Money Stock"),
            ],
            Self::Tankan => vec![
                ("CO", "TK99F1000601GCQ01000", "TANKAN Mfg Large DI (actual)"),
                ("CO", "TK99F2000601GCQ01000", "TANKAN Non-Mfg Large DI (actual)"),
            ],
            Self::Fx => vec![("FM08", "FXERD04", "USD/JPY (17:00 JST)")],
        }
    }
}

pub async fn fetch_boj(req: BojRequest) -> Result<BojResponse> {
    let client = &*crate::finance::shared_client::GENERAL;
    let start = req.start_date.as_deref().unwrap_or("202401");
    let mut warnings = Vec::new();

    let queries: Vec<(String, String, String)> = if let Some(ref preset) = req.preset {
        preset
            .queries()
            .into_iter()
            .map(|(db, codes, label)| (db.to_string(), codes.to_string(), label.to_string()))
            .collect()
    } else if let (Some(ref db), codes) = (&req.db, &req.codes) {
        if codes.is_empty() {
            return Err(Error::InvalidInput("boj requires --codes when using --db".to_string()));
        }
        vec![(db.clone(), codes.join(","), "custom".to_string())]
    } else {
        return Err(Error::InvalidInput(
            "boj requires --preset (policy_rate|call_rate|monetary_base|balance_sheet|money_stock|tankan|fx) or --db + --codes".to_string(),
        ));
    };

    let mut all_series = Vec::new();

    for (db, codes, label) in &queries {
        let url = format!(
            "{}?format=json&lang=en&db={}&code={}&startDate={}",
            BOJ_BASE, db, codes, start
        );

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("boj fetch failed for {db}/{codes}: {e}"));
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warnings.push(format!("boj {db} returned {status}: {}", body.chars().take(200).collect::<String>()));
            continue;
        }

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("boj json parse failed for {db}: {e}"));
                continue;
            }
        };

        // Check status.
        let status = body.get("STATUS").and_then(|v| v.as_str().or_else(|| v.as_i64().map(|_| ""))).unwrap_or("");
        if status != "200" && status != "" {
            let msg = body.get("MESSAGE").and_then(|v| v.as_str()).unwrap_or("unknown error");
            warnings.push(format!("boj {db}: {msg}"));
            continue;
        }

        let data = body.get("DATA").or_else(|| body.get("RESULTSET"));
        let items = match data.and_then(|d| d.as_array()) {
            Some(arr) => arr,
            None => {
                warnings.push(format!("boj {db}: no DATA array in response"));
                continue;
            }
        };

        for item in items {
            let code = item.get("SERIES_CODE").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = item.get("NAME_OF_TIME_SERIES").and_then(|v| v.as_str()).unwrap_or(label).to_string();
            let unit = item.get("UNIT").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let freq = item.get("FREQUENCY").and_then(|v| v.as_str()).unwrap_or("").to_string();

            let dates_val = item.get("VALUES").and_then(|v| v.get("SURVEY_DATES")).or_else(|| item.get("SURVEY_DATES"));
            let values_val = item.get("VALUES").and_then(|v| v.get("VALUES")).or_else(|| item.get("VALUES"));

            let (dates, values) = match (dates_val.and_then(|v| v.as_array()), values_val.and_then(|v| v.as_array())) {
                (Some(d), Some(v)) => (d, v),
                _ => continue,
            };

            let mut observations = Vec::new();
            for (d, v) in dates.iter().zip(values.iter()) {
                let period = match d {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => continue,
                };
                let value: f64 = match v {
                    serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
                    serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
                    serde_json::Value::Null => continue,
                    _ => continue,
                };
                observations.push(BojObservation { period, value });
            }

            if !observations.is_empty() {
                all_series.push(BojSeries {
                    code,
                    name,
                    unit,
                    frequency: freq,
                    observations,
                });
            }
        }

        // Rate limit between queries.
        if queries.len() > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
    }

    Ok(BojResponse {
        generated_at: Utc::now(),
        preset: req.preset.as_ref().map(|p| format!("{:?}", p)),
        series: all_series,
        warnings,
    })
}
