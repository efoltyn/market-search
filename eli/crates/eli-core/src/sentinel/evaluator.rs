use super::SubscriptionSpec;
use crate::finance::{fetch_odds, fetch_prices, OddsRequest, PricesRequest};
use chrono::{Datelike, Timelike, Utc};
use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, HashMapContext, Node, Value,
};
use regex::Regex;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug)]
pub struct VariableObservation {
    pub value: f64,
    pub source: String,
    pub instrument: String,
    pub endpoint: String,
    pub symbol_or_id: String,
}

#[derive(Clone, Debug)]
pub struct Evaluation {
    pub triggered: bool,
    pub observed_vars: BTreeMap<String, f64>,
    pub observations: BTreeMap<String, VariableObservation>,
}

#[derive(Clone, Debug)]
pub struct EvaluationFailure {
    pub connector: String,
    pub message: String,
}

impl std::fmt::Display for EvaluationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.connector, self.message)
    }
}

impl std::error::Error for EvaluationFailure {}

pub fn extract_var_names(expr: &str) -> Vec<String> {
    let Ok(re) = Regex::new(r"[A-Za-z_][A-Za-z0-9_]*") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for cap in re.find_iter(expr) {
        let token = cap.as_str();
        if matches!(
            token,
            "if" | "else" | "true" | "false" | "and" | "or" | "not" | "in"
        ) {
            continue;
        }
        if seen.insert(token.to_string()) {
            out.push(token.to_string());
        }
    }
    out
}

pub fn default_var_spec(var: &str) -> String {
    let lower = var.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("pyth_") {
        return format!("pyth:{rest}");
    }
    if let Some(rest) = lower.strip_prefix("poly_") {
        return format!("poly:{rest}");
    }
    if let Some(rest) = lower.strip_prefix("kalshi_") {
        return format!("kalshi:{rest}");
    }
    if lower == "cme_globex_open" {
        return "session:cme_globex_open".to_string();
    }
    "literal:0".to_string()
}

fn parse_var_spec(spec: &str) -> (String, String) {
    let trimmed = spec.trim();
    if let Some((kind, rest)) = trimmed.split_once(':') {
        return (
            kind.trim().to_ascii_lowercase(),
            rest.trim().to_ascii_lowercase(),
        );
    }
    ("literal".to_string(), trimmed.to_ascii_lowercase())
}

fn now_globex_open() -> bool {
    // Approximate ET using UTC-5 for v1 daemon gating.
    // This is sufficient for session-open alerting semantics.
    let et = Utc::now() - chrono::Duration::hours(5);
    let weekday = et.weekday().num_days_from_sunday();
    let mins = et.hour() * 60 + et.minute();
    let day_open = !(mins >= 17 * 60 && mins < 18 * 60);
    if weekday == 6 {
        mins >= 18 * 60
    } else if weekday == 5 {
        mins < 17 * 60
    } else {
        day_open
    }
}

fn normalized_pyth_query(alias: &str) -> String {
    match alias {
        // Pin common aliases to canonical Pyth symbols to avoid ambiguous auto-select.
        "wti" | "oil" | "cl" => "Commodities.USOILSPOT".to_string(),
        "brent" => "Commodities.UKOILSPOT".to_string(),
        "gold" | "gc" => "Metal.XAU/USD".to_string(),
        "silver" | "si" => "Metal.XAG/USD".to_string(),
        "btc" | "bitcoin" => "Crypto.BTC/USD".to_string(),
        "eth" | "ethereum" => "Crypto.ETH/USD".to_string(),
        "sol" | "solana" => "Crypto.SOL/USD".to_string(),
        "dxy" | "dollar" | "dollar_index" => "FX.USDXY".to_string(),
        "usdjpy" | "usd_jpy" | "jpy" | "yen" => "FX.USD/JPY".to_string(),
        other => other.replace('_', " "),
    }
}

fn odds_query_and_side(alias: &str) -> (String, bool) {
    let mut query = alias.to_string();
    let mut no_side = false;
    if let Some(stripped) = query.strip_suffix("_yes") {
        query = stripped.to_string();
    }
    if let Some(stripped) = query.strip_suffix("_no") {
        query = stripped.to_string();
        no_side = true;
    }
    (query.replace('_', " "), no_side)
}

async fn resolve_pyth(query_alias: &str) -> std::result::Result<VariableObservation, String> {
    // Support direct feed ID lookup via "id:<hex>" to bypass query search ambiguity.
    let (query_opt, asset_type_opt, ids) = if let Some(hex_id) = query_alias.strip_prefix("id:") {
        (None, None, vec![hex_id.to_string()])
    } else {
        let q = normalized_pyth_query(query_alias);
        // Use typed asset_type when the canonical symbol includes a known prefix,
        // so Pyth's feed search filters correctly and avoids cross-type ambiguity.
        let at = if q.starts_with("FX.") {
            "fx".to_string()
        } else if q.starts_with("Metal.") {
            "metal".to_string()
        } else if q.starts_with("Crypto.") {
            "crypto".to_string()
        } else if q.starts_with("Commodities.") {
            "commodities".to_string()
        } else {
            String::new()
        };
        (Some(q), Some(at), Vec::new())
    };
    let query = query_opt.clone().unwrap_or_default();
    let req = PricesRequest {
        query: query_opt,
        asset_type: asset_type_opt,
        ids,
        auto_select: true,
    };
    let resp = fetch_prices(req)
        .await
        .map_err(|e| format!("fetch prices failed: {e}"))?;
    let point = resp
        .prices
        .first()
        .ok_or_else(|| format!("no prices found for query '{query}'"))?;
    Ok(VariableObservation {
        value: point.value,
        source: "pyth".to_string(),
        instrument: point.symbol.clone(),
        endpoint: "prices".to_string(),
        symbol_or_id: point.symbol.clone(),
    })
}

async fn resolve_odds(
    provider: &str,
    query_alias: &str,
) -> std::result::Result<VariableObservation, String> {
    let (query, no_side) = odds_query_and_side(query_alias);
    let req = OddsRequest {
        provider: Some(provider.to_string()),
        disable_kalshi: false,
        series_ticker: None,
        event_ticker: None,
        market_ticker: None,
        // Keep status unset because Kalshi search+status can return empty sets
        // for valid queries. We filter to open/active locally below.
        status: None,
        limit: Some(20),
        cursor: None,
        max_pages: Some(1),
        include_orderbook: false,
        orderbook_depth: None,
        list_series: false,
        list_events: false,
        list_markets: true,
        list_tags: false,
        category: None,
        search: Some(query.clone()),
    };
    let resp = fetch_odds(req)
        .await
        .map_err(|e| format!("fetch odds failed: {e}"))?;

    let mut best = resp
        .available_markets
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.status
                .as_deref()
                .map(|s| {
                    let s = s.to_ascii_lowercase();
                    s == "open" || s == "active" || s.is_empty()
                })
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    best.sort_by(|a, b| b.volume.unwrap_or(0).cmp(&a.volume.unwrap_or(0)));
    let market = best
        .first()
        .ok_or_else(|| format!("no open markets found for query '{query}'"))?;
    let mut prob = market
        .probability_yes
        .or_else(|| market.yes_price.map(|p| (p as f64) / 100.0))
        .ok_or_else(|| format!("missing yes probability for market '{}'", market.ticker))?;
    if no_side {
        prob = 1.0 - prob;
    }
    Ok(VariableObservation {
        value: prob,
        source: provider.to_string(),
        instrument: market.ticker.clone(),
        endpoint: "odds".to_string(),
        symbol_or_id: market.ticker.clone(),
    })
}

async fn resolve_variable(
    name: &str,
    spec: &str,
    cache: &mut HashMap<String, VariableObservation>,
) -> std::result::Result<VariableObservation, EvaluationFailure> {
    let (kind, rest) = parse_var_spec(spec);
    let cache_key = format!("{kind}:{rest}");
    if let Some(existing) = cache.get(&cache_key) {
        return Ok(existing.clone());
    }

    let observed = match kind.as_str() {
        "pyth" => resolve_pyth(&rest).await.map_err(|e| EvaluationFailure {
            connector: "pyth".to_string(),
            message: e,
        })?,
        "poly" | "polymarket" => resolve_odds("polymarket", &rest)
            .await
            .map_err(|e| EvaluationFailure {
                connector: "polymarket".to_string(),
                message: e,
            })?,
        "kalshi" => resolve_odds("kalshi", &rest)
            .await
            .map_err(|e| EvaluationFailure {
                connector: "kalshi".to_string(),
                message: e,
            })?,
        "session" if rest == "cme_globex_open" => VariableObservation {
            value: if now_globex_open() { 1.0 } else { 0.0 },
            source: "session".to_string(),
            instrument: "cme_globex".to_string(),
            endpoint: "clock".to_string(),
            symbol_or_id: "cme_globex_open".to_string(),
        },
        "literal" => {
            let parsed = rest.parse::<f64>().map_err(|e| EvaluationFailure {
                connector: "literal".to_string(),
                message: format!("invalid literal for {name}: {e}"),
            })?;
            VariableObservation {
                value: parsed,
                source: "literal".to_string(),
                instrument: name.to_string(),
                endpoint: "literal".to_string(),
                symbol_or_id: name.to_string(),
            }
        }
        other => {
            return Err(EvaluationFailure {
                connector: other.to_string(),
                message: format!("unsupported variable spec '{spec}' for {name}"),
            })
        }
    };

    cache.insert(cache_key, observed.clone());
    Ok(observed)
}

pub async fn evaluate_subscription(
    sub: &SubscriptionSpec,
) -> std::result::Result<Evaluation, EvaluationFailure> {
    let tree: Node = build_operator_tree(&sub.expr).map_err(|e| EvaluationFailure {
        connector: "evalexpr".to_string(),
        message: format!("expression compile failed: {e}"),
    })?;
    let var_names = extract_var_names(&sub.expr);
    let mut context = HashMapContext::new();
    let mut observed_vars = BTreeMap::new();
    let mut observations = BTreeMap::new();
    let mut cache = HashMap::new();

    for name in var_names {
        let spec = sub
            .vars
            .get(&name)
            .cloned()
            .unwrap_or_else(|| default_var_spec(&name));
        let obs = resolve_variable(&name, &spec, &mut cache).await?;
        context
            .set_value(name.clone(), Value::Float(obs.value))
            .map_err(|e| EvaluationFailure {
                connector: "evalexpr".to_string(),
                message: format!("failed to set variable {name}: {e}"),
            })?;
        observed_vars.insert(name.clone(), obs.value);
        observations.insert(name, obs);
    }

    let triggered = tree
        .eval_boolean_with_context(&context)
        .map_err(|e| EvaluationFailure {
            connector: "evalexpr".to_string(),
            message: format!("expression evaluate failed: {e}"),
        })?;

    Ok(Evaluation {
        triggered,
        observed_vars,
        observations,
    })
}
