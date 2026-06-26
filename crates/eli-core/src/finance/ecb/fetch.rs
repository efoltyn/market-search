use crate::{Error, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

const ECB_BASE: &str = "https://data-api.ecb.europa.eu/service/data";

/// A single observation from the ECB SDMX API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EcbObservation {
    pub period: String,
    pub value: f64,
}

/// A single series returned by the ECB.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EcbSeries {
    pub key: String,
    #[serde(skip)] // internal SDMX dataflow id — not surfaced to consumers
    pub dataset: String,
    pub label: String,
    pub frequency: String,
    pub unit: Option<String>,
    pub observations: Vec<EcbObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EcbResponse {
    pub generated_at: DateTime<Utc>,
    pub preset: Option<String>,
    pub series: Vec<EcbSeries>,
}

#[derive(Clone, Debug)]
pub struct EcbRequest {
    pub preset: Option<EcbPreset>,
    pub dataset: Option<String>,
    pub key: Option<String>,
    pub start_period: Option<String>,
    pub end_period: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EcbPreset {
    Eurusd,
    FxMajors,
    Estr,
    M3,
    Euribor,
    YieldCurve,
    BalanceSheet,
}

impl EcbPreset {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "eurusd" => Some(Self::Eurusd),
            "fx" | "fx_majors" => Some(Self::FxMajors),
            "estr" | "str" | "euro_str" => Some(Self::Estr),
            "m3" | "money" | "money_supply" => Some(Self::M3),
            "euribor" => Some(Self::Euribor),
            "yield_curve" | "yc" | "yields" => Some(Self::YieldCurve),
            "balance_sheet" | "bs" | "assets" => Some(Self::BalanceSheet),
            _ => None,
        }
    }

    /// Returns (dataset, key, label, unit_hint).
    fn queries(
        &self,
    ) -> Vec<(
        &'static str,
        &'static str,
        &'static str,
        Option<&'static str>,
    )> {
        match self {
            Self::Eurusd => vec![("EXR", "D.USD.EUR.SP00.A", "EUR/USD", None)],
            Self::FxMajors => vec![
                ("EXR", "D.USD.EUR.SP00.A", "EUR/USD", None),
                ("EXR", "D.GBP.EUR.SP00.A", "EUR/GBP", None),
                ("EXR", "D.JPY.EUR.SP00.A", "EUR/JPY", None),
                ("EXR", "D.CHF.EUR.SP00.A", "EUR/CHF", None),
                ("EXR", "D.CNY.EUR.SP00.A", "EUR/CNY", None),
            ],
            Self::Estr => vec![("EST", "B.EU000A2X2A25.WT", "Euro STR", Some("percent"))],
            Self::M3 => vec![(
                "BSI",
                "M.U2.N.V.M30.X.1.U2.2300.Z01.E",
                "M3 Money Supply",
                Some("millions_eur"),
            )],
            Self::Euribor => vec![
                (
                    "FM",
                    "M.U2.EUR.RT.MM.EURIBOR1MD_.HSTA",
                    "EURIBOR 1M",
                    Some("percent"),
                ),
                (
                    "FM",
                    "M.U2.EUR.RT.MM.EURIBOR3MD_.HSTA",
                    "EURIBOR 3M",
                    Some("percent"),
                ),
                (
                    "FM",
                    "M.U2.EUR.RT.MM.EURIBOR6MD_.HSTA",
                    "EURIBOR 6M",
                    Some("percent"),
                ),
                (
                    "FM",
                    "M.U2.EUR.RT.MM.EURIBOR1YD_.HSTA",
                    "EURIBOR 12M",
                    Some("percent"),
                ),
            ],
            Self::YieldCurve => vec![
                (
                    "YC",
                    "B.U2.EUR.4F.G_N_A.SV_C_YM.SR_3M",
                    "EUR 3M yield",
                    Some("percent"),
                ),
                (
                    "YC",
                    "B.U2.EUR.4F.G_N_A.SV_C_YM.SR_1Y",
                    "EUR 1Y yield",
                    Some("percent"),
                ),
                (
                    "YC",
                    "B.U2.EUR.4F.G_N_A.SV_C_YM.SR_2Y",
                    "EUR 2Y yield",
                    Some("percent"),
                ),
                (
                    "YC",
                    "B.U2.EUR.4F.G_N_A.SV_C_YM.SR_5Y",
                    "EUR 5Y yield",
                    Some("percent"),
                ),
                (
                    "YC",
                    "B.U2.EUR.4F.G_N_A.SV_C_YM.SR_10Y",
                    "EUR 10Y yield",
                    Some("percent"),
                ),
                (
                    "YC",
                    "B.U2.EUR.4F.G_N_A.SV_C_YM.SR_30Y",
                    "EUR 30Y yield",
                    Some("percent"),
                ),
            ],
            Self::BalanceSheet => vec![(
                "BSI",
                "M.U2.N.C.T00.A.1.Z5.0000.Z01.E",
                "Eurosystem Total Assets",
                Some("millions_eur"),
            )],
        }
    }
}

pub async fn fetch_ecb(req: EcbRequest) -> Result<EcbResponse> {
    let client = &*crate::finance::shared_client::GENERAL;

    let queries: Vec<(String, String, String, Option<String>)> =
        if let Some(ref preset) = req.preset {
            preset
                .queries()
                .into_iter()
                .map(|(ds, key, label, unit)| {
                    (
                        ds.to_string(),
                        key.to_string(),
                        label.to_string(),
                        unit.map(|u| u.to_string()),
                    )
                })
                .collect()
        } else if let (Some(ref ds), Some(ref key)) = (&req.dataset, &req.key) {
            vec![(ds.clone(), key.clone(), format!("{}/{}", ds, key), None)]
        } else {
            return Err(Error::InvalidInput(
                "ecb requires either --preset or --dataset + --key".to_string(),
            ));
        };

    let start = req.start_period.as_deref().unwrap_or("2025-01-01");
    let end = req.end_period.as_deref();

    let mut all_series = Vec::new();

    for (dataset, key, label, unit) in &queries {
        let mut url = format!(
            "{}/{}/{}?format=csvdata&startPeriod={}&detail=dataonly",
            ECB_BASE, dataset, key, start
        );
        if let Some(end) = end {
            url.push_str(&format!("&endPeriod={}", end));
        }

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("ecb fetch failed for {label}: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            // 404 = no data for this key, not necessarily an error
            if status.as_u16() == 404 {
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!(
                "ecb returned {status} for {label}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| Error::Provider(format!("ecb body read failed for {label}: {e}")))?;

        let observations = parse_ecb_csv(&body, &key);

        if observations.is_empty() {
            continue;
        }

        // Detect frequency from first observation's period format.
        let freq = if observations[0].period.len() == 10 {
            "daily" // YYYY-MM-DD
        } else if observations[0].period.len() == 7 {
            "monthly" // YYYY-MM
        } else if observations[0].period.len() == 4 {
            "annual" // YYYY
        } else {
            "unknown"
        };

        all_series.push(EcbSeries {
            key: format!("{}/{}", dataset, key),
            dataset: dataset.clone(),
            label: label.clone(),
            frequency: freq.to_string(),
            unit: unit.clone(),
            observations,
        });

        // Small delay between requests to be polite.
        if queries.len() > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    Ok(EcbResponse {
        generated_at: Utc::now(),
        preset: req.preset.as_ref().map(|p| format!("{:?}", p)),
        series: all_series,
    })
}

/// Parse ECB SDMX CSV. TIME_PERIOD and OBS_VALUE are always the last two columns.
fn parse_ecb_csv(body: &str, _expected_key: &str) -> Vec<EcbObservation> {
    let mut lines = body.lines();
    let header = match lines.next() {
        Some(h) => h,
        None => return Vec::new(),
    };

    let cols: Vec<&str> = header.split(',').collect();
    let n = cols.len();
    if n < 2 {
        return Vec::new();
    }
    // TIME_PERIOD and OBS_VALUE are always the last two columns.
    let time_idx = n - 2;
    let value_idx = n - 1;

    let mut observations = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < n {
            continue;
        }
        let period = fields[time_idx].trim().to_string();
        let value: f64 = match fields[value_idx].trim().parse() {
            Ok(v) => v,
            Err(_) => continue, // skip rows with missing/empty values
        };
        observations.push(EcbObservation { period, value });
    }

    observations
}
