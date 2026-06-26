use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// BIS SDMX REST API v2.
/// Base: https://stats.bis.org/api/v2/data/dataflow/BIS/{dataflowId}/1.0/{key}?format=csv

const BIS_BASE: &str = "https://stats.bis.org/api/v2/data/dataflow/BIS";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BisObservation {
    pub period: String,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BisSeries {
    pub label: String,
    /// Synthesized identifier `"<REF_AREA>/<label_prefix>"` (e.g. `"GB/Policy Rate"`).
    /// NOT a real BIS SDMX key — for that, use `--dataset` + `--key` directly with
    /// dataflows like `WS_CBPOL`. Renamed from `key` to make the synthetic nature explicit.
    pub composite_key: String,
    pub ref_area: String,
    pub frequency: String,
    pub unit: Option<String>,
    pub observations: Vec<BisObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BisResponse {
    pub generated_at: DateTime<Utc>,
    #[serde(skip)] // internal SDMX dataflow id — not surfaced to consumers
    pub dataset: String,
    pub series: Vec<BisSeries>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BisRequest {
    pub preset: Option<BisPreset>,
    pub dataset: Option<String>,
    pub key: Option<String>,
    pub countries: Vec<String>,
    pub start_period: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BisPreset {
    PolicyRates,
    CentralBankAssets,
    CreditGap,
    PropertyPrices,
    EffectiveExchangeRates,
}

impl BisPreset {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "policy_rates" | "rates" | "cbpol" => Some(Self::PolicyRates),
            "assets" | "cb_assets" | "cbta" | "balance_sheet" => Some(Self::CentralBankAssets),
            "credit_gap" | "gap" | "credit" => Some(Self::CreditGap),
            "property" | "property_prices" | "housing" | "spp" => Some(Self::PropertyPrices),
            "eer" | "reer" | "effective_fx" => Some(Self::EffectiveExchangeRates),
            _ => None,
        }
    }

    fn queries(&self, countries: &[String]) -> Vec<BisQuerySpec> {
        let areas = if countries.is_empty() {
            "US+XM+JP+GB".to_string()
        } else {
            countries.join("+")
        };

        match self {
            Self::PolicyRates => vec![BisQuerySpec {
                dataflow: "WS_CBPOL",
                key: format!("M.{}", areas),
                label_prefix: "Policy Rate",
                unit: "percent",
            }],
            Self::CentralBankAssets => vec![BisQuerySpec {
                dataflow: "WS_CBTA",
                key: format!("Q.{}.B.XDC..B", areas),
                label_prefix: "CB Total Assets (local ccy)",
                unit: "billions_local_currency",
            }],
            Self::CreditGap => vec![BisQuerySpec {
                dataflow: "WS_CREDIT_GAP",
                key: format!("Q.{}.P.A.C", areas),
                label_prefix: "Credit-to-GDP Gap",
                unit: "pp_of_gdp",
            }],
            Self::PropertyPrices => vec![BisQuerySpec {
                dataflow: "WS_SPP",
                key: format!("Q.{}.R.628", areas),
                label_prefix: "Real Property Prices",
                unit: "index_2010_100",
            }],
            Self::EffectiveExchangeRates => vec![BisQuerySpec {
                dataflow: "WS_EER",
                key: format!("M.R.B.{}", areas),
                label_prefix: "REER (broad)",
                unit: "index",
            }],
        }
    }
}

struct BisQuerySpec {
    dataflow: &'static str,
    key: String,
    label_prefix: &'static str,
    unit: &'static str,
}

pub async fn fetch_bis(req: BisRequest) -> Result<BisResponse> {
    let client = &*crate::finance::shared_client::GENERAL;
    let start = req.start_period.as_deref().unwrap_or("2020-01");
    let mut warnings = Vec::new();

    let queries = if let Some(ref preset) = req.preset {
        preset.queries(&req.countries)
    } else if let (Some(ref ds), Some(ref key)) = (&req.dataset, &req.key) {
        vec![BisQuerySpec {
            dataflow: Box::leak(ds.clone().into_boxed_str()),
            key: key.clone(),
            label_prefix: "custom",
            unit: "",
        }]
    } else {
        return Err(Error::InvalidInput(
            "bis requires --preset (policy_rates|assets|credit_gap|property|eer) or --dataset + --key".to_string(),
        ));
    };

    let mut all_series = Vec::new();

    for spec in &queries {
        let url = format!(
            "{}/{}/1.0/{}?format=csv&startPeriod={}&detail=dataonly",
            BIS_BASE, spec.dataflow, spec.key, start
        );

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("bis fetch failed for {}: {e}", spec.dataflow));
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            if status.as_u16() == 404 {
                warnings.push(format!("bis no data for {}/{}", spec.dataflow, spec.key));
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            warnings.push(format!(
                "bis {} returned {}: {}",
                spec.dataflow,
                status,
                body.chars().take(200).collect::<String>()
            ));
            continue;
        }

        let body = resp
            .text()
            .await
            .map_err(|e| Error::Provider(format!("bis body read failed: {e}")))?;

        let parsed = parse_bis_csv(&body, spec.label_prefix, spec.unit);
        all_series.extend(parsed);

        if queries.len() > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    let dataset = queries
        .first()
        .map(|q| q.dataflow.to_string())
        .unwrap_or_default();

    Ok(BisResponse {
        generated_at: Utc::now(),
        dataset,
        series: all_series,
        warnings,
    })
}

/// Parse BIS SDMX CSV. Like ECB, TIME_PERIOD and OBS_VALUE are the last two columns.
/// Series are distinguished by the REF_AREA column.
fn parse_bis_csv(body: &str, label_prefix: &str, unit: &str) -> Vec<BisSeries> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(body.as_bytes());

    let headers = match rdr.headers() {
        Ok(h) => h.clone(),
        Err(_) => return Vec::new(),
    };

    let time_idx = match headers.iter().position(|c| c.trim() == "TIME_PERIOD") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let value_idx = match headers.iter().position(|c| c.trim() == "OBS_VALUE") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let freq_idx = headers.iter().position(|c| c.trim() == "FREQ");
    let area_idx = [
        "REF_AREA",
        "BORROWERS_CTY",
        "COUNTERPART_AREA",
        "COUNTRY",
        "AREA",
    ]
    .iter()
    .find_map(|name| headers.iter().position(|c| c.trim() == *name));

    let mut by_area: std::collections::BTreeMap<String, Vec<BisObservation>> =
        std::collections::BTreeMap::new();
    let mut freq_label: Option<String> = None;

    for record in rdr.records() {
        let record = match record {
            Ok(r) => r,
            Err(_) => continue,
        };

        let period = match record.get(time_idx) {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => continue,
        };
        let value: f64 = match record.get(value_idx).and_then(|v| v.trim().parse().ok()) {
            Some(v) => v,
            None => continue,
        };

        let area = area_idx
            .and_then(|i| record.get(i))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "??".to_string());

        if freq_label.is_none() {
            freq_label = freq_idx
                .and_then(|i| record.get(i))
                .map(|v| match v.trim() {
                    "D" => "daily",
                    "W" => "weekly",
                    "M" => "monthly",
                    "Q" => "quarterly",
                    "A" => "annual",
                    _ => "unknown",
                })
                .map(str::to_string);
        }

        by_area
            .entry(area)
            .or_default()
            .push(BisObservation { period, value });
    }

    let freq_label = freq_label.unwrap_or_else(|| "unknown".to_string());

    by_area
        .into_iter()
        .map(|(area, mut obs)| {
            obs.sort_by(|a, b| a.period.cmp(&b.period));
            obs.dedup_by(|a, b| a.period == b.period);
            BisSeries {
                label: format!("{} {}", area, label_prefix),
                composite_key: format!("{}/{}", area, label_prefix),
                ref_area: area,
                frequency: freq_label.clone(),
                unit: if unit.is_empty() {
                    None
                } else {
                    Some(unit.to_string())
                },
                observations: obs,
            }
        })
        .collect()
}
