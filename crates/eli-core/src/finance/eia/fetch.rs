use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const EIA_BASE: &str = "https://api.eia.gov/v2";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EiaObservation {
    pub period: String,
    pub value: f64,
    pub units: String,
    pub product: String,
    pub product_name: String,
    pub area: String,
    pub area_name: String,
    pub series: String,
    pub series_description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EiaSeries {
    pub label: String,
    pub observations: Vec<EiaObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EiaResponse {
    pub generated_at: DateTime<Utc>,
    pub preset: Option<String>,
    pub series: Vec<EiaSeries>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct EiaRequest {
    pub api_key: String,
    pub preset: Option<EiaPreset>,
    pub route: Option<String>,
    pub facets: Vec<(String, String)>,
    pub start: Option<String>,
    pub length: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EiaPreset {
    CrudeStocks,
    GasolineStocks,
    DistillateStocks,
    AllPetroleumStocks,
    NatGasStorage,
    NatGasPrices,
    CrudeProduction,
    ElectricityDemand,
    NuclearOutages,
    Steo,
}

impl EiaPreset {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "crude" | "crude_stocks" | "crude_oil" => Some(Self::CrudeStocks),
            "gasoline" | "gas" | "mogas" => Some(Self::GasolineStocks),
            "distillate" | "diesel" | "heating_oil" => Some(Self::DistillateStocks),
            "all" | "petroleum" | "all_stocks" => Some(Self::AllPetroleumStocks),
            "nat_gas" | "natgas" | "natural_gas" | "ng_storage" | "storage" => Some(Self::NatGasStorage),
            "ng_prices" | "henry_hub" | "gas_prices" => Some(Self::NatGasPrices),
            "crude_production" | "production" | "us_production" => Some(Self::CrudeProduction),
            "electricity" | "demand" | "grid" => Some(Self::ElectricityDemand),
            "nuclear" | "outages" | "nuclear_outages" => Some(Self::NuclearOutages),
            "steo" | "forecast" | "outlook" => Some(Self::Steo),
            _ => None,
        }
    }

    fn queries(&self) -> Vec<EiaQuerySpec> {
        match self {
            Self::CrudeStocks => vec![EiaQuerySpec {
                route: "petroleum/stoc/wstk/data/",
                label: "US Crude Oil Stocks (excl SPR)",
                facets: vec![("product", "EPC0"), ("process", "SAX"), ("duoarea", "NUS")], frequency: "weekly", data_col: "value",
            }],
            Self::GasolineStocks => vec![EiaQuerySpec {
                route: "petroleum/stoc/wstk/data/",
                label: "US Finished Motor Gasoline Stocks",
                facets: vec![("product", "EPM0F"), ("process", "SAE"), ("duoarea", "NUS")], frequency: "weekly", data_col: "value",
            }],
            Self::DistillateStocks => vec![EiaQuerySpec {
                route: "petroleum/stoc/wstk/data/",
                label: "US Distillate Fuel Oil Stocks",
                facets: vec![("product", "EPD0"), ("process", "SAE"), ("duoarea", "NUS")], frequency: "weekly", data_col: "value",
            }],
            Self::AllPetroleumStocks => vec![
                EiaQuerySpec {
                    route: "petroleum/stoc/wstk/data/",
                    label: "US Crude Oil Stocks (excl SPR)",
                    facets: vec![("product", "EPC0"), ("process", "SAX"), ("duoarea", "NUS")], frequency: "weekly", data_col: "value",
                },
                EiaQuerySpec {
                    route: "petroleum/stoc/wstk/data/",
                    label: "US Finished Motor Gasoline Stocks",
                    facets: vec![("product", "EPM0F"), ("process", "SAE"), ("duoarea", "NUS")], frequency: "weekly", data_col: "value",
                },
                EiaQuerySpec {
                    route: "petroleum/stoc/wstk/data/",
                    label: "US Distillate Fuel Oil Stocks",
                    facets: vec![("product", "EPD0"), ("process", "SAE"), ("duoarea", "NUS")], frequency: "weekly", data_col: "value",
                },
                EiaQuerySpec {
                    route: "petroleum/stoc/wstk/data/",
                    label: "Cushing OK Crude Oil Stocks",
                    facets: vec![("product", "EPC0"), ("process", "SAX"), ("duoarea", "YCUOK")], frequency: "weekly", data_col: "value",
                },
            ],
            Self::NatGasStorage => vec![EiaQuerySpec {
                route: "natural-gas/stor/wkly/data/",
                label: "Lower 48 Natural Gas Working Storage",
                facets: vec![("process", "SWO"), ("duoarea", "R48")], frequency: "weekly", data_col: "value",
            }],
            Self::NatGasPrices => vec![EiaQuerySpec {
                route: "natural-gas/pri/fut/data/",
                label: "Henry Hub Natural Gas Futures",
                facets: vec![("series", "RNGC1")], frequency: "daily", data_col: "value",
            }],
            Self::CrudeProduction => vec![EiaQuerySpec {
                route: "petroleum/crd/crpdn/data/",
                label: "US Crude Oil Production",
                facets: vec![],
                frequency: "monthly", data_col: "value",
            }],
            Self::ElectricityDemand => vec![EiaQuerySpec {
                route: "electricity/rto/daily-region-data/data/",
                label: "US Electricity Demand (daily)",
                facets: vec![("type", "D")], frequency: "daily", data_col: "value",
            }],
            Self::NuclearOutages => vec![EiaQuerySpec {
                route: "nuclear-outages/us-nuclear-outages/data/",
                label: "US Nuclear Outages",
                facets: vec![],
                frequency: "daily", data_col: "outage",
            // NOTE: this endpoint uses data[0]=outage not data[0]=value
            }],
            Self::Steo => vec![
                EiaQuerySpec {
                    route: "steo/data/",
                    label: "WTI Crude Price Forecast",
                    facets: vec![("seriesId", "WTIPUUS")],
                    frequency: "monthly", data_col: "value",
                },
                EiaQuerySpec {
                    route: "steo/data/",
                    label: "Brent Crude Price Forecast",
                    facets: vec![("seriesId", "BREPUUS")],
                    frequency: "monthly", data_col: "value",
                },
                EiaQuerySpec {
                    route: "steo/data/",
                    label: "Henry Hub Price Forecast",
                    facets: vec![("seriesId", "NGHHUUS")],
                    frequency: "monthly", data_col: "value",
                },
                EiaQuerySpec {
                    route: "steo/data/",
                    label: "US Crude Production Forecast",
                    facets: vec![("seriesId", "COPRPUS")],
                    frequency: "monthly", data_col: "value",
                },
            ],
        }
    }
}

struct EiaQuerySpec {
    route: &'static str,
    label: &'static str,
    facets: Vec<(&'static str, &'static str)>,
    frequency: &'static str, // weekly, monthly, daily, annual
    data_col: &'static str,  // usually "value", nuclear uses "outage"
}

pub async fn fetch_eia(req: EiaRequest) -> Result<EiaResponse> {
    let client = &*crate::finance::shared_client::GENERAL;
    let api_key = &req.api_key;
    if api_key.is_empty() {
        return Err(Error::InvalidInput(
            "EIA API key required. Set EIA_API_KEY env var or add [eia] api_key to ~/.config/eli/inv.toml. Register free at https://www.eia.gov/opendata/register.php".to_string(),
        ));
    }

    let queries = if let Some(ref preset) = req.preset {
        preset.queries()
    } else if let Some(ref route) = req.route {
        vec![EiaQuerySpec {
            route: Box::leak(route.clone().into_boxed_str()),
            label: "custom",
            facets: req.facets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>()
                .into_iter()
                .map(|(k, v)| {
                    // Safety: we need 'static refs but these are from the request.
                    // This is a workaround; in production use Cow or owned types.
                    let k: &'static str = Box::leak(k.to_string().into_boxed_str());
                    let v: &'static str = Box::leak(v.to_string().into_boxed_str());
                    (k, v)
                })
                .collect(),
            frequency: "weekly", data_col: "value",
        }]
    } else {
        return Err(Error::InvalidInput(
            "eia requires --preset or --route".to_string(),
        ));
    };

    let length = req.length.unwrap_or(52);
    let mut all_series = Vec::new();
    let mut warnings = Vec::new();

    for spec in &queries {
        let mut url = format!(
            "{}/{}?api_key={}&frequency={}&data[0]={}&sort[0][column]=period&sort[0][direction]=desc&length={}",
            EIA_BASE, spec.route, api_key, spec.frequency, spec.data_col, length
        );
        for (facet_key, facet_val) in &spec.facets {
            url.push_str(&format!("&facets[{}][]={}", facet_key, facet_val));
        }
        if let Some(ref start) = req.start {
            url.push_str(&format!("&start={}", start));
        }

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("eia fetch failed for {}: {e}", spec.label));
                continue;
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                warnings.push(format!(
                    "eia rate limited for {}. Try again later or register a key at https://www.eia.gov/opendata/",
                    spec.label
                ));
            } else {
                warnings.push(format!(
                    "eia returned {} for {}: {}",
                    status,
                    spec.label,
                    body.chars().take(200).collect::<String>()
                ));
            }
            continue;
        }

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("eia json parse failed for {}: {e}", spec.label));
                continue;
            }
        };

        let data = body
            .get("response")
            .and_then(|r| r.get("data"))
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();

        let mut observations = Vec::new();
        for row in &data {
            let period = row.get("period").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let value: f64 = row
                .get("value")
                .and_then(|v| v.as_str().or_else(|| v.as_f64().map(|_| "")))
                .and_then(|s| if s.is_empty() {
                    row.get("value").and_then(|v| v.as_f64())
                } else {
                    s.parse::<f64>().ok()
                })
                .unwrap_or(0.0);
            let units = row.get("units").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let product = row.get("product").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let product_name = row.get("product-name")
                .or_else(|| row.get("productName"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let area = row.get("duoarea").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let area_name = row.get("area-name")
                .or_else(|| row.get("areaName"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let series = row.get("series").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let series_description = row.get("series-description")
                .or_else(|| row.get("seriesDescription"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if value == 0.0 && period.is_empty() {
                continue;
            }

            observations.push(EiaObservation {
                period,
                value,
                units,
                product,
                product_name,
                area,
                area_name,
                series,
                series_description,
            });
        }

        all_series.push(EiaSeries {
            label: spec.label.to_string(),
            observations,
        });

        // Rate limit between queries.
        if queries.len() > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    Ok(EiaResponse {
        generated_at: Utc::now(),
        preset: req.preset.as_ref().map(|p| format!("{:?}", p)),
        series: all_series,
        warnings,
    })
}
