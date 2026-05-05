use super::*;
use futures::StreamExt;
use ibapi::accounts::{
    types::{AccountGroup, AccountId},
    AccountSummaryResult, AccountSummaryTags, AccountUpdate, PositionUpdate,
};
use ibapi::contracts::tick_types::TickType;
use ibapi::contracts::{Contract, ContractDetails, SecurityType};
use ibapi::market_data::historical::{
    BarSize as HistoricalBarSize, Duration as HistoricalDuration,
    WhatToShow as HistoricalWhatToShow,
};
use ibapi::market_data::realtime::TickTypes;
use ibapi::market_data::{MarketDataType, TradingHours};
use ibapi::orders::{CancelOrder, Order, OrderStatus, Orders, PlaceOrder, TimeInForce};
use ibapi::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration as StdDuration;
use time::OffsetDateTime;
use tokio::time::{timeout, Instant};

#[derive(Debug, Default, Deserialize)]
struct SnapshotPayload {
    #[serde(default)]
    tickers: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AccountSummaryPayload {
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    tags: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AccountScopedPayload {
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlaceOrderPayload {
    #[serde(default)]
    account: Option<String>,
    side: String,
    #[serde(default)]
    order_type: Option<String>,
    quantity: f64,
    #[serde(default)]
    limit_price: Option<f64>,
    #[serde(default)]
    stop_price: Option<f64>,
    #[serde(default)]
    tif: Option<String>,
    contract: ContractInput,
}

#[derive(Debug, Deserialize)]
struct CancelOrderPayload {
    order_id: i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ContractInput {
    symbol: String,
    #[serde(default)]
    sec_type: Option<String>,
    #[serde(default)]
    exchange: Option<String>,
    #[serde(default)]
    primary_exchange: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    expiry: Option<String>,
    #[serde(default)]
    strike: Option<f64>,
    #[serde(default)]
    right: Option<String>,
    #[serde(default)]
    multiplier: Option<String>,
    #[serde(default)]
    trading_class: Option<String>,
}

#[derive(Debug, Default)]
struct SnapshotAccumulator {
    current_price: Option<f64>,
    previous_close: Option<f64>,
    open: Option<f64>,
    day_low: Option<f64>,
    day_high: Option<f64>,
    last_timestamp: Option<i64>,
}

impl SnapshotAccumulator {
    fn has_observation(&self) -> bool {
        self.current_price.is_some()
            || self.previous_close.is_some()
            || self.open.is_some()
            || self.day_low.is_some()
            || self.day_high.is_some()
            || self.last_timestamp.is_some()
    }
}

#[derive(Debug, Default)]
struct OptionQuoteAccumulator {
    bid: Option<f64>,
    ask: Option<f64>,
    last: Option<f64>,
    volume: Option<u64>,
    open_interest: Option<u64>,
    iv_bid: Option<f64>,
    iv_ask: Option<f64>,
    iv_last: Option<f64>,
    iv_model: Option<f64>,
    delta: Option<f64>,
    gamma: Option<f64>,
    theta: Option<f64>,
    vega: Option<f64>,
}

pub fn resolve_ibkr_connection(
    overrides: Option<&IbkrConnectionConfig>,
) -> Result<IbkrConnectionConfig> {
    credentials::resolve_ibkr_connection(overrides).map_err(Error::Provider)
}

pub async fn invoke_ibkr_bridge(
    action: &str,
    payload: Value,
    overrides: Option<&IbkrConnectionConfig>,
) -> Result<Value> {
    let connection = resolve_ibkr_connection(overrides)?;
    dispatch_ibkr_action(action, payload, &connection).await
}

/// Parse an IBKR ticker spec.  Supports rich syntax:
///   `FUT:CL:NYMEX:202506`   → sec_type=FUT, symbol=CL, exchange=NYMEX, expiry=202506
///   `CASH:EUR:IDEALPRO:USD`  → sec_type=CASH, symbol=EUR, exchange=IDEALPRO, currency=USD
///   `FUT:ES:CME`             → sec_type=FUT, symbol=ES, exchange=CME (front month)
///   `AAPL`                   → sec_type=STK, symbol=AAPL, exchange=SMART (default)
/// Map well-known international exchanges to their default currency so that
/// `build_contract` doesn't fall back to USD for non-US listings.
fn exchange_default_currency(exchange: &str) -> Option<&'static str> {
    match exchange {
        // Gulf / Middle-East
        "TADAWUL" => Some("SAR"),
        "DFM" => Some("AED"),
        "ADX" => Some("AED"),
        "QSE" => Some("QAR"),
        "BHB" => Some("BHD"),
        "MSM" => Some("OMR"),
        "KSE" => Some("KWD"),
        // Major international (add as needed)
        "SEHK" | "HKEX" => Some("HKD"),
        "TSE" => Some("JPY"),
        "LSE" | "LSEETF" => Some("GBP"),
        "SBF" | "IBIS" | "AEB" => Some("EUR"),
        "ASX" => Some("AUD"),
        "SGX" => Some("SGD"),
        "JSE" => Some("ZAR"),
        "BMF" | "BOVESPA" => Some("BRL"),
        "NSE" | "BSE" => Some("INR"),
        "KSE2" | "KRX" => Some("KRW"),
        "TWSE" => Some("TWD"),
        _ => None,
    }
}

fn parse_ibkr_ticker(raw: &str) -> (String, ContractInput) {
    let parts: Vec<&str> = raw.split(':').collect();
    let known_sec_types = [
        "STK", "FUT", "OPT", "CASH", "IND", "BOND", "CMDTY", "FOP", "WAR", "CFD", "CRYPTO",
    ];
    if parts.len() >= 2 && known_sec_types.contains(&parts[0].to_ascii_uppercase().as_str()) {
        let sec_type = parts[0].to_ascii_uppercase();
        let symbol = parts.get(1).unwrap_or(&"").to_ascii_uppercase();
        let exchange = parts.get(2).map(|s| s.to_string());
        let fourth = parts.get(3).map(|s| s.to_string());

        // Field 4 semantics vary by sec_type:
        //   CASH  → currency (counter currency)
        //   STK   → currency override (e.g. STK:EMAAR:DFM:AED)
        //   other → expiry
        let (expiry, currency) = if sec_type == "CASH" {
            // CASH requires a quote (counter) currency.  4-field form
            // provides it explicitly (e.g. CASH:EUR:IDEALPRO:USD).
            // 3-field form (CASH:EUR:IDEALPRO) defaults to USD — the
            // overwhelmingly common counter currency for FX pairs.
            (None, Some(fourth.unwrap_or_else(|| "USD".to_string())))
        } else if sec_type == "STK" {
            // 4th field = explicit currency; if absent, infer from exchange
            let cur = fourth.or_else(|| {
                exchange
                    .as_deref()
                    .and_then(|ex| exchange_default_currency(ex))
                    .map(|c| c.to_string())
            });
            (None, cur)
        } else {
            (fourth, None)
        };

        // Display name preserves the original spec
        let display = raw.to_string();
        (
            display,
            ContractInput {
                symbol,
                sec_type: Some(sec_type),
                exchange,
                currency,
                expiry,
                ..ContractInput::default()
            },
        )
    } else {
        // Plain ticker — default to STK on SMART
        let symbol = raw.to_ascii_uppercase();
        (
            symbol.clone(),
            ContractInput {
                symbol,
                sec_type: Some("STK".to_string()),
                exchange: Some("SMART".to_string()),
                ..ContractInput::default()
            },
        )
    }
}

pub async fn fetch_ibkr_snapshot(req: &SnapshotRequest) -> Result<Vec<TickerSnapshot>> {
    let connection = resolve_ibkr_connection(req.ibkr.as_ref())?;
    let client = connect_client(&connection).await?;
    apply_market_data_type(&client, &connection).await?;
    let collected_at = Utc::now();
    let market_data_type = requested_market_data_type(&connection);
    let timeout_secs = connection.timeout_secs.unwrap_or(15);

    normalize_tickers(&req.tickers)
        .into_iter()
        .map(|ticker| {
            let (display_name, contract_input) = parse_ibkr_ticker(&ticker);
            let client = &client;
            async move {
            let detail = resolve_contract_detail(
                client,
                &contract_input,
                timeout_secs,
            )
            .await?;
            let data = fetch_contract_snapshot(&client, &detail.contract, timeout_secs)
                .await
                .map_err(|err| match err {
                    Error::Provider(msg)
                        if msg == "timeout waiting for ibkr snapshot ticks".to_string() =>
                    {
                        Error::Provider(format!(
                            "timeout waiting for ibkr snapshot ticks for {display_name}"
                        ))
                    }
                    other => other,
                })?;

            let observed_at = data
                .last_timestamp
                .and_then(|ts| Utc.timestamp_opt(ts, 0).single())
                .unwrap_or(collected_at);
            let state = match market_data_type {
                1 => FreshnessState::Live,
                2 => FreshnessState::Eod,
                3 | 4 => FreshnessState::Delayed,
                _ => FreshnessState::Unknown,
            };
            let fallback_price = data.current_price.or(data.previous_close);
            let used_market_closed_fallback =
                data.current_price.is_none() && data.previous_close.is_some();

            // Enrich the name with contract month/local symbol so the output
            // shows WHICH contract was resolved (e.g. "Light Sweet Crude Oil CLK6 202505")
            let local_sym = detail.contract.local_symbol.to_string();
            let expiry = detail.contract.last_trade_date_or_contract_month.clone();
            let enriched_name = {
                let base = &detail.long_name;
                let mut parts = vec![base.clone()];
                if !local_sym.is_empty() { parts.push(local_sym.clone()); }
                if !expiry.is_empty() { parts.push(expiry); }
                parts.join(" ")
            };
            Ok(TickerSnapshot {
                ticker: display_name,
                currency: non_empty(detail.contract.currency.to_string()),
                exchange: non_empty(detail.contract.exchange.to_string()),
                short_name: non_empty(detail.long_name.clone()),
                long_name: Some(enriched_name),
                current_price: fallback_price,
                previous_close: data.previous_close,
                open: data.open,
                day_low: data.day_low,
                day_high: data.day_high,
                price: fallback_price,
                daily_return: match (fallback_price, data.previous_close) {
                    (Some(px), Some(prev)) if prev.is_finite() && prev != 0.0 => {
                        Some((px / prev) - 1.0)
                    }
                    _ => None,
                },
                market_cap: None,
                enterprise_value: None,
                shares_outstanding: None,
                float_shares: None,
                last_split_factor: None,
                last_split_date: None,
                freshness: Freshness::new(
                    observed_at,
                    collected_at,
                    state,
                    FreshnessOrigin::ProviderTimestamp,
                    FreshnessQuality::Exact,
                ),
                price_source_kind: "ibkr".to_string(),
                session_state: market_data_type_name(market_data_type).to_string(),
                market_closed_fallback: used_market_closed_fallback,
                effective_at: Some(observed_at),
                clock_status: None,
                integrity_note: None,
            })
        }})
        .collect::<futures::stream::FuturesOrdered<_>>()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

pub async fn fetch_ibkr_timeseries(
    req: &TimeseriesRequest,
) -> Result<(Vec<TickerSeries>, Vec<TimeseriesError>)> {
    let connection = resolve_ibkr_connection(req.ibkr.as_ref())?;
    let client = connect_client(&connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(30);
    let now = Utc::now();
    let mut end = req.as_of.unwrap_or(now);
    if end > now {
        end = now;
    }

    let duration = ibkr_duration_for_span(&req.range)?;
    let bar_size = ibkr_bar_size(&req.granularity)?;
    let end_date = to_offset_datetime(end)?;

    let mut series_out = Vec::new();
    let mut errors = Vec::new();

    for ticker in normalize_tickers(&req.tickers) {
        let (display_name, contract_input) = parse_ibkr_ticker(&ticker);
        let detail = match resolve_contract_detail(
            &client,
            &contract_input,
            timeout_secs,
        )
        .await
        {
            Ok(detail) => detail,
            Err(err) => {
                errors.push(TimeseriesError {
                    ticker: display_name,
                    stage: Some("ibkr".to_string()),
                    message: err.to_string(),
                });
                continue;
            }
        };

        let history = match timeout(
            StdDuration::from_secs(timeout_secs),
            client.historical_data(
                &detail.contract,
                Some(end_date),
                duration,
                bar_size,
                Some(match contract_input.sec_type.as_deref() {
                    Some("CASH") | Some("IND") => HistoricalWhatToShow::MidPoint,
                    _ => HistoricalWhatToShow::Trades,
                }),
                TradingHours::Extended,
            ),
        )
        .await
        {
            Ok(Ok(history)) => history,
            Ok(Err(err)) => {
                errors.push(TimeseriesError {
                    ticker: display_name,
                    stage: Some("ibkr".to_string()),
                    message: map_ibapi_error(err),
                });
                continue;
            }
            Err(_) => {
                errors.push(TimeseriesError {
                    ticker: display_name,
                    stage: Some("ibkr".to_string()),
                    message: format!(
                        "timeout retrieving ibkr history for {}",
                        detail.contract.symbol
                    ),
                });
                continue;
            }
        };

        let candles = history
            .bars
            .into_iter()
            .map(|bar| {
                let t = to_chrono_datetime(bar.date)?;
                Ok(Candle {
                    t,
                    o: bar.open,
                    h: bar.high,
                    l: bar.low,
                    c: bar.close,
                    v: Some(bar.volume),
                    kind: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let upstream = display_name.clone();
        series_out.push(TickerSeries {
            ticker: display_name,
            candles,
            source: Some("ibkr".to_string()),
            upstream_id: Some(upstream),
        });
    }

    Ok((series_out, errors))
}

pub async fn fetch_ibkr_search(req: &SearchRequest) -> Result<SearchResponse> {
    let started = std::time::Instant::now();
    let generated_at = Utc::now();
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Err(Error::InvalidInput("search query is required".to_string()));
    }

    let policy_mode = req.policy_mode.unwrap_or_default();
    let policy_file = req.policy_file.as_deref().map(std::path::Path::new);
    let resolved_policy = crate::finance::policy::load_policy(policy_file, policy_mode)?;
    let connection = resolve_ibkr_connection(req.ibkr.as_ref())?;
    let client = connect_client(&connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(10);
    let descriptions = timeout(
        StdDuration::from_secs(timeout_secs),
        client.matching_symbols(&query),
    )
    .await
    .map_err(|_| Error::Provider("timeout requesting ibkr symbol search".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let mut seen = std::collections::HashSet::<String>::new();
    let mut results = Vec::new();
    for (idx, description) in descriptions.into_iter().enumerate() {
        let contract = description.contract;
        let symbol = contract.symbol.to_string();
        let exchange = non_empty(contract.primary_exchange.to_string())
            .or_else(|| non_empty(contract.exchange.to_string()));
        let dedupe_key = format!(
            "{}|{}|{}",
            symbol,
            exchange.clone().unwrap_or_default(),
            contract.security_type
        );
        if !seen.insert(dedupe_key) {
            continue;
        }
        results.push(SearchItem {
            symbol,
            name: non_empty(contract.description).or_else(|| non_empty(contract.local_symbol)),
            exchange,
            asset_type: Some(contract.security_type.to_string()),
            score: Some((100usize.saturating_sub(idx)) as f64),
        });
        if results.len() >= 20 {
            break;
        }
    }

    let macro_items: Vec<SearchItem> = resolved_policy
        .policy
        .macro_catalog
        .indicators
        .iter()
        .map(|ind| SearchItem {
            symbol: ind.id.clone(),
            name: Some(ind.name.clone()),
            exchange: Some("FRED".into()),
            asset_type: Some("MACRO".into()),
            score: None,
        })
        .collect();
    let query_lower = query.to_lowercase();
    let suggestions = if query_lower.len() > 2 {
        macro_items
            .into_iter()
            .filter(|item| {
                item.symbol.to_lowercase().contains(&query_lower)
                    || item
                        .name
                        .as_ref()
                        .map(|n| n.to_lowercase().contains(&query_lower))
                        .unwrap_or(false)
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(SearchResponse {
        query,
        generated_at,
        schema_version: "finance.search.v3".to_string(),
        preferred_provider: "yahoo".to_string(), // IBKR results are instrument-like
        yahoo_results: results,
        fred_results: suggestions,
        decision_trace: vec![
            "provider=ibkr".to_string(),
        ],
    })
}

pub async fn fetch_ibkr_options(req: &OptionsRequest) -> Result<OptionsResponse> {
    if req.multi_expiry {
        return Err(Error::InvalidInput(
            "ibkr provider does not support multi-expiry options summary yet".to_string(),
        ));
    }

    let connection = resolve_ibkr_connection(req.ibkr.as_ref())?;
    let client = connect_client(&connection).await?;
    apply_market_data_type(&client, &connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(20);
    let ticker = req.ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return Err(Error::InvalidInput("ticker is required".to_string()));
    }

    let detail = resolve_contract_detail(
        &client,
        &ContractInput {
            symbol: ticker.clone(),
            sec_type: Some("STK".to_string()),
            exchange: Some("SMART".to_string()),
            ..ContractInput::default()
        },
        timeout_secs,
    )
    .await?;

    let underlying_snapshot = fetch_contract_snapshot(&client, &detail.contract, timeout_secs).await?;
    let underlying_price = underlying_snapshot
        .current_price
        .or(underlying_snapshot.previous_close)
        .unwrap_or(0.0);

    let mut chain_subscription = timeout(
        StdDuration::from_secs(timeout_secs),
        client.option_chain(
            &ticker,
            "SMART",
            detail.contract.security_type.clone(),
            detail.contract.contract_id,
        ),
    )
    .await
    .map_err(|_| Error::Provider("timeout requesting ibkr option chain".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let mut chains = collect_option_chains(&mut chain_subscription, timeout_secs).await?;
    let chain = if let Some(idx) = chains
        .iter()
        .position(|chain| chain.exchange.eq_ignore_ascii_case("SMART"))
    {
        chains.swap_remove(idx)
    } else {
        chains
            .into_iter()
            .next()
            .ok_or_else(|| Error::Provider(format!("no option chain returned for {}", ticker)))?
    };

    let mut expirations_raw = chain.expirations.clone();
    expirations_raw.sort();
    expirations_raw.dedup();
    let expirations: Vec<String> = expirations_raw
        .iter()
        .map(|expiry| format_ibkr_expiry(expiry))
        .collect();

    if req.list_expirations {
        return Ok(OptionsResponse {
            ticker,
            underlying_price,
            generated_at: Utc::now(),
            status: None,
            error: None,
            expirations,
            requested_expiry: req.expiry.clone(),
            selected_expiry: None,
            target_dte_days: req.target_dte_days,
            selected_days_to_expiry: None,
            auto_selected_expiry: false,
            selection_reason: None,
            calls: vec![],
            puts: vec![],
            metrics: None,
            note: None,
            multi_expiry_summary: None,
        });
    }

    let (
        selected_expiry_raw,
        selected_expiry,
        selected_days_to_expiry,
        auto_selected_expiry,
        selection_reason,
    ) = select_ibkr_expiry(&expirations_raw, req.expiry.as_deref(), req.target_dte_days)?;

    let effective_near_money_pct = req
        .near_money_pct
        .or_else(|| req.summary_only.then_some(10.0));
    let mut strikes = chain.strikes.clone();
    strikes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    strikes.dedup_by(|a, b| (*a - *b).abs() < 0.0001);
    if let Some(pct) = effective_near_money_pct {
        if pct > 0.0 && underlying_price > 0.0 {
            let low = underlying_price * (1.0 - pct / 100.0);
            let high = underlying_price * (1.0 + pct / 100.0);
            strikes.retain(|strike| *strike >= low && *strike <= high);
        }
    }
    if req.summary_only && strikes.len() > 16 && underlying_price > 0.0 {
        strikes.sort_by(|a, b| {
            (a - underlying_price)
                .abs()
                .partial_cmp(&(b - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        strikes.truncate(16);
        strikes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    }

    let mut calls = Vec::new();
    let mut puts = Vec::new();
    let include_calls = req
        .option_type
        .as_deref()
        .map(|t| !matches!(t.trim().to_ascii_lowercase().as_str(), "puts" | "put"))
        .unwrap_or(true);
    let include_puts = req
        .option_type
        .as_deref()
        .map(|t| !matches!(t.trim().to_ascii_lowercase().as_str(), "calls" | "call"))
        .unwrap_or(true);

    for strike in strikes {
        if include_calls {
            let contract =
                build_option_contract(&detail, &chain, &selected_expiry_raw, strike, "C")?;
            let quote = fetch_option_quote_snapshot(&client, &contract, true, timeout_secs).await?;
            calls.push(build_option_contract_row(
                &ticker,
                &selected_expiry,
                underlying_price,
                strike,
                "call",
                quote,
            ));
        }
        if include_puts {
            let contract =
                build_option_contract(&detail, &chain, &selected_expiry_raw, strike, "P")?;
            let quote =
                fetch_option_quote_snapshot(&client, &contract, false, timeout_secs).await?;
            puts.push(build_option_contract_row(
                &ticker,
                &selected_expiry,
                underlying_price,
                strike,
                "put",
                quote,
            ));
        }
    }

    build_options_response(
        ticker,
        underlying_price,
        expirations,
        req.expiry.clone(),
        selected_expiry,
        req.target_dte_days,
        selected_days_to_expiry,
        auto_selected_expiry,
        selection_reason,
        calls,
        puts,
        None,
    )
}

async fn collect_option_chains(
    subscription: &mut ibapi::subscriptions::Subscription<ibapi::contracts::OptionChain>,
    timeout_secs: u64,
) -> Result<Vec<ibapi::contracts::OptionChain>> {
    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    let mut chains = Vec::new();
    loop {
        let remaining = remaining_time(deadline)?;
        match timeout(remaining, subscription.next()).await {
            Ok(Some(Ok(chain))) => chains.push(chain),
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(chains)
}

fn select_ibkr_expiry(
    expirations_raw: &[String],
    requested_expiry: Option<&str>,
    target_dte_days: Option<i64>,
) -> Result<(String, String, Option<i64>, bool, Option<String>)> {
    if expirations_raw.is_empty() {
        return Err(Error::Provider(
            "no IBKR option expirations returned".to_string(),
        ));
    }

    let today = Utc::now().date_naive();
    let requested_raw = requested_expiry.map(|value| value.replace('-', ""));
    let selected_raw = if let Some(requested_raw) = requested_raw.clone() {
        expirations_raw
            .iter()
            .find(|expiry| *expiry == &requested_raw)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidInput(format!(
                    "requested expiry '{}' not found in IBKR chain",
                    requested_expiry.unwrap_or_default()
                ))
            })?
    } else if let Some(target_dte_days) = target_dte_days {
        expirations_raw
            .iter()
            .filter_map(|expiry| {
                parse_ibkr_expiry(expiry).map(|date| {
                    let dte = (date - today).num_days();
                    (expiry.clone(), dte, (dte - target_dte_days).abs())
                })
            })
            .min_by_key(|(_, _, delta)| *delta)
            .map(|(expiry, _, _)| expiry)
            .unwrap_or_else(|| expirations_raw[0].clone())
    } else {
        expirations_raw[0].clone()
    };

    let selected_date = parse_ibkr_expiry(&selected_raw);
    let selected_days_to_expiry = selected_date.map(|date| (date - today).num_days());
    let auto_selected_expiry = requested_expiry.is_none();
    let selection_reason = if requested_expiry.is_some() {
        Some("explicit_expiry".to_string())
    } else if target_dte_days.is_some() {
        Some("closest_target_dte".to_string())
    } else {
        Some("first_available_expiry".to_string())
    };

    Ok((
        selected_raw.clone(),
        format_ibkr_expiry(&selected_raw),
        selected_days_to_expiry,
        auto_selected_expiry,
        selection_reason,
    ))
}

fn parse_ibkr_expiry(raw: &str) -> Option<chrono::NaiveDate> {
    let trimmed = raw.trim();
    if trimmed.len() != 8 {
        return None;
    }
    chrono::NaiveDate::parse_from_str(trimmed, "%Y%m%d").ok()
}

fn format_ibkr_expiry(raw: &str) -> String {
    parse_ibkr_expiry(raw)
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| raw.to_string())
}

fn build_option_contract(
    detail: &ContractDetails,
    chain: &ibapi::contracts::OptionChain,
    expiry_raw: &str,
    strike: f64,
    right: &str,
) -> Result<Contract> {
    build_contract(&ContractInput {
        symbol: detail.contract.symbol.to_string(),
        sec_type: Some("OPT".to_string()),
        exchange: Some(if chain.exchange.trim().is_empty() {
            "SMART".to_string()
        } else {
            chain.exchange.clone()
        }),
        primary_exchange: non_empty(detail.contract.primary_exchange.to_string()),
        currency: non_empty(detail.contract.currency.to_string()),
        expiry: Some(expiry_raw.to_string()),
        strike: Some(strike),
        right: Some(right.to_string()),
        multiplier: non_empty(chain.multiplier.clone()),
        trading_class: non_empty(chain.trading_class.clone()),
    })
}

async fn fetch_option_quote_snapshot(
    client: &Client,
    contract: &Contract,
    is_call: bool,
    timeout_secs: u64,
) -> Result<OptionQuoteAccumulator> {
    let mut subscription = timeout(
        StdDuration::from_secs(timeout_secs),
        client
            .market_data(contract)
            .generic_ticks(&["100", "101"])
            .snapshot()
            .subscribe(),
    )
    .await
    .map_err(|_| Error::Provider("timeout creating ibkr option snapshot".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    let mut quote = OptionQuoteAccumulator::default();
    loop {
        let remaining = remaining_time(deadline)?;
        match timeout(remaining, subscription.next()).await {
            Ok(Some(Ok(TickTypes::Price(price)))) => {
                apply_option_price_tick(&mut quote, price.tick_type, price.price)
            }
            Ok(Some(Ok(TickTypes::PriceSize(price_size)))) => {
                apply_option_price_tick(&mut quote, price_size.price_tick_type, price_size.price);
                apply_option_size_tick(
                    &mut quote,
                    price_size.size_tick_type,
                    price_size.size,
                    is_call,
                );
            }
            Ok(Some(Ok(TickTypes::Size(size)))) => {
                apply_option_size_tick(&mut quote, size.tick_type, size.size, is_call)
            }
            Ok(Some(Ok(TickTypes::OptionComputation(computation)))) => {
                apply_option_computation_tick(&mut quote, computation)
            }
            Ok(Some(Ok(TickTypes::SnapshotEnd))) => break,
            Ok(Some(Ok(TickTypes::Notice(_)))) => {}
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(quote)
}

fn apply_option_price_tick(quote: &mut OptionQuoteAccumulator, tick_type: TickType, price: f64) {
    if !price.is_finite() || price < 0.0 {
        return;
    }
    match tick_type {
        TickType::Bid | TickType::DelayedBid => quote.bid = Some(price),
        TickType::Ask | TickType::DelayedAsk => quote.ask = Some(price),
        TickType::Last | TickType::DelayedLast => quote.last = Some(price),
        _ => {}
    }
}

fn apply_option_size_tick(
    quote: &mut OptionQuoteAccumulator,
    tick_type: TickType,
    size: f64,
    is_call: bool,
) {
    if !size.is_finite() || size < 0.0 {
        return;
    }
    let normalized = size.round().max(0.0) as u64;
    match tick_type {
        TickType::OptionCallVolume if is_call => quote.volume = Some(normalized),
        TickType::OptionPutVolume if !is_call => quote.volume = Some(normalized),
        TickType::OptionCallOpenInterest if is_call => quote.open_interest = Some(normalized),
        TickType::OptionPutOpenInterest if !is_call => quote.open_interest = Some(normalized),
        TickType::Volume if quote.volume.is_none() => quote.volume = Some(normalized),
        _ => {}
    }
}

fn apply_option_computation_tick(
    quote: &mut OptionQuoteAccumulator,
    computation: ibapi::contracts::OptionComputation,
) {
    match computation.field {
        TickType::BidOption | TickType::DelayedBidOption => {
            quote.iv_bid = computation.implied_volatility
        }
        TickType::AskOption | TickType::DelayedAskOption => {
            quote.iv_ask = computation.implied_volatility
        }
        TickType::LastOption | TickType::DelayedLastOption => {
            quote.iv_last = computation.implied_volatility
        }
        TickType::ModelOption | TickType::DelayedModelOption => {
            quote.iv_model = computation.implied_volatility;
            // Model tick carries the most reliable Greeks
            if computation.delta.is_some() {
                quote.delta = computation.delta;
            }
            if computation.gamma.is_some() {
                quote.gamma = computation.gamma;
            }
            if computation.theta.is_some() {
                quote.theta = computation.theta;
            }
            if computation.vega.is_some() {
                quote.vega = computation.vega;
            }
        }
        _ => {}
    }
}

fn implied_volatility_from_quote(quote: &OptionQuoteAccumulator) -> Option<f64> {
    quote
        .iv_model
        .or(quote.iv_last)
        .or_else(|| match (quote.iv_bid, quote.iv_ask) {
            (Some(bid), Some(ask)) => Some((bid + ask) / 2.0),
            (Some(iv), None) | (None, Some(iv)) => Some(iv),
            (None, None) => None,
        })
}

fn build_option_contract_row(
    ticker: &str,
    expiry: &str,
    underlying_price: f64,
    strike: f64,
    option_type: &str,
    quote: OptionQuoteAccumulator,
) -> OptionContract {
    let right_code = if option_type == "call" { "C" } else { "P" };
    let in_the_money = if underlying_price > 0.0 {
        if option_type == "call" {
            strike <= underlying_price
        } else {
            strike >= underlying_price
        }
    } else {
        false
    };
    OptionContract {
        contract_symbol: format!("{ticker}-{expiry}-{right_code}-{strike:.2}"),
        strike,
        expiry: expiry.to_string(),
        option_type: option_type.to_string(),
        bid: quote.bid.unwrap_or(0.0),
        ask: quote.ask.unwrap_or(0.0),
        last: quote.last.or(quote.bid).or(quote.ask).unwrap_or(0.0),
        change: 0.0,
        pct_change: 0.0,
        volume: quote.volume.unwrap_or(0),
        open_interest: quote.open_interest.unwrap_or(0),
        implied_volatility: implied_volatility_from_quote(&quote),
        in_the_money,
        delta: quote.delta,
        gamma: quote.gamma,
        theta: quote.theta,
        vega: quote.vega,
    }
}

fn build_options_response(
    ticker: String,
    underlying_price: f64,
    expirations: Vec<String>,
    requested_expiry: Option<String>,
    selected_expiry: String,
    target_dte_days: Option<i64>,
    selected_days_to_expiry: Option<i64>,
    auto_selected_expiry: bool,
    selection_reason: Option<String>,
    calls: Vec<OptionContract>,
    puts: Vec<OptionContract>,
    note: Option<String>,
) -> Result<OptionsResponse> {
    let total_call_volume: u64 = calls.iter().map(|c| c.volume).sum();
    let total_put_volume: u64 = puts.iter().map(|p| p.volume).sum();
    let total_call_oi: u64 = calls.iter().map(|c| c.open_interest).sum();
    let total_put_oi: u64 = puts.iter().map(|p| p.open_interest).sum();

    let put_call_ratio_volume = if total_call_volume > 0 {
        Some(total_put_volume as f64 / total_call_volume as f64)
    } else if total_put_volume > 0 {
        Some(f64::INFINITY.min(99.99))
    } else {
        None
    };
    let put_call_ratio_oi = if total_call_oi > 0 {
        Some(total_put_oi as f64 / total_call_oi as f64)
    } else if total_put_oi > 0 {
        Some(f64::INFINITY.min(99.99))
    } else {
        None
    };

    let atm_iv_call = {
        let mut candidates: Vec<&OptionContract> =
            calls.iter().filter(|c| c.implied_volatility.is_some()).collect();
        candidates.sort_by(|a, b| {
            (a.strike - underlying_price)
                .abs()
                .partial_cmp(&(b.strike - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.first().and_then(|c| c.implied_volatility)
    };
    let atm_iv_put = {
        let mut candidates: Vec<&OptionContract> =
            puts.iter().filter(|p| p.implied_volatility.is_some()).collect();
        candidates.sort_by(|a, b| {
            (a.strike - underlying_price)
                .abs()
                .partial_cmp(&(b.strike - underlying_price).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.first().and_then(|p| p.implied_volatility)
    };
    let skew_near_put_call_iv_ratio = match (atm_iv_call, atm_iv_put) {
        (Some(call_iv), Some(put_iv)) if call_iv > 0.0 && put_iv > 0.0 => {
            Some(put_iv / call_iv)
        }
        _ => None,
    };
    let has_iv_data = atm_iv_call.is_some() || atm_iv_put.is_some();
    let has_liquid_near_money =
        (total_call_volume + total_put_volume + total_call_oi + total_put_oi) > 0;
    let all_strikes: std::collections::BTreeSet<i64> = calls
        .iter()
        .map(|c| (c.strike * 100.0).round() as i64)
        .chain(puts.iter().map(|p| (p.strike * 100.0).round() as i64))
        .collect();
    let max_pain = all_strikes
        .iter()
        .min_by_key(|&&k| {
            let k_price = k as f64 / 100.0;
            let call_itm: f64 = calls
                .iter()
                .filter(|c| c.strike < k_price)
                .map(|c| (k_price - c.strike) * c.open_interest as f64)
                .sum();
            let put_itm: f64 = puts
                .iter()
                .filter(|p| p.strike > k_price)
                .map(|p| (p.strike - k_price) * p.open_interest as f64)
                .sum();
            ((call_itm + put_itm) * 100.0) as i64
        })
        .map(|&k| k as f64 / 100.0);

    let atm_iv = match (atm_iv_call, atm_iv_put) {
        (Some(call_iv), Some(put_iv)) => Some((call_iv + put_iv) / 2.0),
        (Some(call_iv), None) => Some(call_iv),
        (None, Some(put_iv)) => Some(put_iv),
        (None, None) => None,
    };
    let summary_quality = if !has_liquid_near_money {
        Some("illiquid".to_string())
    } else if !has_iv_data || total_call_oi == 0 || total_put_oi == 0 {
        Some("partial".to_string())
    } else {
        Some("usable".to_string())
    };
    let metrics = Some(OptionsMetrics {
        underlying_price,
        put_call_ratio_volume,
        put_call_ratio_oi,
        total_call_volume,
        total_put_volume,
        total_call_oi,
        total_put_oi,
        atm_iv_call,
        atm_iv_put,
        atm_iv,
        skew_near_put_call_iv_ratio,
        has_iv_data,
        has_liquid_near_money,
        max_pain,
        summary_quality,
        expirations_analyzed: Some(1),
    });

    Ok(OptionsResponse {
        ticker,
        underlying_price,
        generated_at: Utc::now(),
        status: None,
        error: None,
        expirations,
        requested_expiry,
        selected_expiry: Some(selected_expiry),
        target_dte_days,
        selected_days_to_expiry,
        auto_selected_expiry,
        selection_reason,
        calls,
        puts,
        metrics,
        note,
        multi_expiry_summary: None,
    })
}

async fn dispatch_ibkr_action(
    action: &str,
    payload: Value,
    connection: &IbkrConnectionConfig,
) -> Result<Value> {
    match action {
        "snapshot" => {
            let payload: SnapshotPayload = serde_json::from_value(payload)?;
            let req = SnapshotRequest {
                tickers: payload.tickers,
                as_of: None,
                provider: ProviderKind::Ibkr,
                ibkr: Some(connection.clone()),
            };
            let snapshots = fetch_ibkr_snapshot(&req).await?;
            Ok(json!({
                "ok": true,
                "provider": "ibkr",
                "snapshots": snapshots,
            }))
        }
        "account_summary" => {
            let payload: AccountSummaryPayload = serde_json::from_value(payload)?;
            fetch_account_summary(payload, connection).await
        }
        "positions" => {
            let payload: AccountScopedPayload = serde_json::from_value(payload)?;
            fetch_positions(payload, connection).await
        }
        "portfolio" => {
            let payload: AccountScopedPayload = serde_json::from_value(payload)?;
            fetch_portfolio(payload, connection).await
        }
        "open_orders" => fetch_open_orders(connection).await,
        "place_order" => {
            let payload: PlaceOrderPayload = serde_json::from_value(payload)?;
            place_order(payload, connection).await
        }
        "cancel_order" => {
            let payload: CancelOrderPayload = serde_json::from_value(payload)?;
            cancel_order(payload, connection).await
        }
        other => Err(Error::InvalidInput(format!(
            "unsupported ibkr action '{other}'"
        ))),
    }
}

async fn fetch_account_summary(
    payload: AccountSummaryPayload,
    connection: &IbkrConnectionConfig,
) -> Result<Value> {
    let client = connect_client(connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(10);
    let tags_raw = payload
        .tags
        .unwrap_or_else(|| AccountSummaryTags::ALL.join(","));
    let tags_vec: Vec<&str> = tags_raw
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .collect();
    let group = AccountGroup("All".to_string());
    let mut subscription = timeout(
        StdDuration::from_secs(timeout_secs),
        client.account_summary(&group, &tags_vec),
    )
    .await
    .map_err(|_| Error::Provider("timeout requesting ibkr account summary".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let mut accounts = BTreeMap::<String, BTreeMap<String, Value>>::new();
    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    loop {
        let remaining = remaining_time(deadline)?;
        match timeout(remaining, subscription.next()).await {
            Ok(Some(Ok(AccountSummaryResult::Summary(summary)))) => {
                accounts.entry(summary.account).or_default().insert(
                    summary.tag,
                    json!({ "value": summary.value, "currency": summary.currency }),
                );
            }
            Ok(Some(Ok(AccountSummaryResult::End))) => break,
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => {
                return Err(Error::Provider(
                    "timeout waiting for ibkr account summary".to_string(),
                ))
            }
        }
    }

    if let Some(account) = payload.account.filter(|v| v != "All") {
        let filtered = accounts.get(&account).cloned().unwrap_or_default();
        return Ok(json!({
            "ok": true,
            "provider": "ibkr",
            "accounts": {
                account: filtered
            }
        }));
    }

    Ok(json!({
        "ok": true,
        "provider": "ibkr",
        "accounts": accounts,
    }))
}

async fn fetch_positions(
    payload: AccountScopedPayload,
    connection: &IbkrConnectionConfig,
) -> Result<Value> {
    let client = connect_client(connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(10);
    let mut subscription = timeout(StdDuration::from_secs(timeout_secs), client.positions())
        .await
        .map_err(|_| Error::Provider("timeout requesting ibkr positions".to_string()))?
        .map_err(map_ibapi_error)
        .map_err(Error::Provider)?;

    let target_account = payload.account.or_else(|| connection.account.clone());
    let mut positions = Vec::new();
    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    loop {
        let remaining = remaining_time(deadline)?;
        match timeout(remaining, subscription.next()).await {
            Ok(Some(Ok(PositionUpdate::Position(position)))) => {
                if target_account
                    .as_ref()
                    .map(|account| account != &position.account)
                    .unwrap_or(false)
                {
                    continue;
                }
                positions.push(json!({
                    "account": position.account,
                    "contract": serialize_contract(&position.contract),
                    "position": position.position,
                    "avg_cost": position.average_cost,
                }));
            }
            Ok(Some(Ok(PositionUpdate::PositionEnd))) => break,
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => {
                return Err(Error::Provider(
                    "timeout waiting for ibkr positions".to_string(),
                ))
            }
        }
    }

    Ok(json!({
        "ok": true,
        "provider": "ibkr",
        "positions": positions,
    }))
}

async fn fetch_portfolio(
    payload: AccountScopedPayload,
    connection: &IbkrConnectionConfig,
) -> Result<Value> {
    let client = connect_client(connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(15);
    let account = resolve_target_account(
        &client,
        payload.account.or_else(|| connection.account.clone()),
    )
    .await?;
    let account_id = AccountId(account.clone());
    let mut subscription = timeout(
        StdDuration::from_secs(timeout_secs),
        client.account_updates(&account_id),
    )
    .await
    .map_err(|_| Error::Provider("timeout requesting ibkr portfolio".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let mut portfolio = Vec::new();
    let mut account_values = BTreeMap::<String, Value>::new();
    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    loop {
        let remaining = remaining_time(deadline)?;
        match timeout(remaining, subscription.next()).await {
            Ok(Some(Ok(AccountUpdate::PortfolioValue(value)))) => {
                portfolio.push(json!({
                    "account": value.account,
                    "contract": serialize_contract(&value.contract),
                    "position": value.position,
                    "market_price": value.market_price,
                    "market_value": value.market_value,
                    "average_cost": value.average_cost,
                    "unrealized_pnl": value.unrealized_pnl,
                    "realized_pnl": value.realized_pnl,
                }));
            }
            Ok(Some(Ok(AccountUpdate::AccountValue(value)))) => {
                account_values.insert(
                    value.key,
                    json!({
                        "value": value.value,
                        "currency": value.currency,
                    }),
                );
            }
            Ok(Some(Ok(AccountUpdate::End))) => break,
            Ok(Some(Ok(AccountUpdate::UpdateTime(_)))) => {}
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => {
                return Err(Error::Provider(
                    "timeout waiting for ibkr portfolio".to_string(),
                ))
            }
        }
    }

    Ok(json!({
        "ok": true,
        "provider": "ibkr",
        "account": account,
        "portfolio": portfolio,
        "account_values": account_values,
    }))
}

async fn fetch_open_orders(connection: &IbkrConnectionConfig) -> Result<Value> {
    let client = connect_client(connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(10);
    let mut subscription = timeout(
        StdDuration::from_secs(timeout_secs),
        client.all_open_orders(),
    )
    .await
    .map_err(|_| Error::Provider("timeout requesting ibkr open orders".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let mut orders = BTreeMap::<i32, Value>::new();
    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    loop {
        let remaining = remaining_time(deadline)?;
        match timeout(remaining, subscription.next()).await {
            Ok(Some(Ok(Orders::OrderData(order_data)))) => {
                orders.insert(
                    order_data.order_id,
                    json!({
                        "order_id": order_data.order_id,
                        "contract": serialize_contract(&order_data.contract),
                        "order": serialize_order(&order_data.order),
                        "status": order_data.order_state.status,
                    }),
                );
            }
            Ok(Some(Ok(Orders::OrderStatus(status)))) => {
                let entry = orders.entry(status.order_id).or_insert_with(
                    || json!({ "order_id": status.order_id, "status": status.status }),
                );
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("status".to_string(), json!(status.status));
                }
            }
            Ok(Some(Ok(Orders::Notice(_)))) => {}
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => {
                return Err(Error::Provider(
                    "timeout waiting for ibkr open orders".to_string(),
                ))
            }
        }
    }

    Ok(json!({
        "ok": true,
        "provider": "ibkr",
        "orders": orders.into_values().collect::<Vec<_>>(),
    }))
}

async fn place_order(
    payload: PlaceOrderPayload,
    connection: &IbkrConnectionConfig,
) -> Result<Value> {
    let client = connect_client(connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(15);
    let detail = resolve_contract_detail(&client, &payload.contract, timeout_secs).await?;
    let account = payload
        .account
        .clone()
        .or_else(|| connection.account.clone());
    let order = build_order(&payload, account)?;
    let order_id = timeout(
        StdDuration::from_secs(timeout_secs),
        client.next_valid_order_id(),
    )
    .await
    .map_err(|_| Error::Provider("timeout requesting next ibkr order id".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let mut subscription = timeout(
        StdDuration::from_secs(timeout_secs),
        client.place_order(order_id, &detail.contract, &order),
    )
    .await
    .map_err(|_| Error::Provider("timeout placing ibkr order".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let result = collect_place_order_result(
        &mut subscription,
        order_id,
        &detail.contract,
        &order,
        timeout_secs,
    )
    .await?;

    Ok(json!({
        "ok": true,
        "provider": "ibkr",
        "result": result,
    }))
}

async fn cancel_order(
    payload: CancelOrderPayload,
    connection: &IbkrConnectionConfig,
) -> Result<Value> {
    let client = connect_client(connection).await?;
    let timeout_secs = connection.timeout_secs.unwrap_or(10);
    let mut subscription = timeout(
        StdDuration::from_secs(timeout_secs),
        client.cancel_order(payload.order_id, ""),
    )
    .await
    .map_err(|_| Error::Provider("timeout sending ibkr cancel order request".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let result =
        collect_cancel_order_result(&mut subscription, payload.order_id, timeout_secs).await?;
    Ok(json!({
        "ok": true,
        "provider": "ibkr",
        "result": result,
    }))
}

async fn connect_client(connection: &IbkrConnectionConfig) -> Result<Client> {
    let host = connection
        .host
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("127.0.0.1");
    let client_id = match connection.client_id {
        Some(id) if id > 0 => id,
        _ => {
            // Pool of 32 client IDs (1-32) — the IB Gateway maximum.
            // PID mod 32 ensures parallel subagents spread across all slots
            // for maximum concurrent throughput while capping tab accumulation.
            (std::process::id() % 32 + 1) as i32
        }
    };

    let candidate_ports = if let Some(port) = connection.port {
        vec![port]
    } else {
        vec![4001, 4002, 7496, 7497]
    };

    let mut last_error = None;
    for port in candidate_ports {
        let address = format!("{host}:{port}");
        match Client::connect(&address, client_id).await {
            Ok(client) => return Ok(client),
            Err(err) => {
                last_error = Some(format!(
                    "no reachable TWS / IB Gateway on {address}: {}",
                    map_ibapi_error(err)
                ));
            }
        }
    }

    Err(Error::Provider(
        last_error.unwrap_or_else(|| "failed to connect to IBKR".to_string()),
    ))
}

async fn apply_market_data_type(client: &Client, connection: &IbkrConnectionConfig) -> Result<()> {
    client
        .switch_market_data_type(market_data_type_from_i32(requested_market_data_type(
            connection,
        )))
        .await
        .map_err(map_ibapi_error)
        .map_err(Error::Provider)
}

async fn resolve_contract_detail(
    client: &Client,
    input: &ContractInput,
    timeout_secs: u64,
) -> Result<ContractDetails> {
    let contract = build_contract(input)?;
    let details = timeout(
        StdDuration::from_secs(timeout_secs),
        client.contract_details(&contract),
    )
    .await
    .map_err(|_| {
        Error::Provider(format!(
            "timeout resolving ibkr contract details for {}",
            input.symbol
        ))
    })?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let mut all: Vec<ContractDetails> = details.into_iter().collect();
    if all.is_empty() {
        return Err(Error::Provider(format!(
            "no contract details found for {}",
            input.symbol
        )));
    }
    // For futures without an explicit expiry, pick the true front month:
    // the contract with the nearest expiry that is still tradeable.
    // IBKR may return them in any order, so sort by last_trade_date ascending
    // and take the first.
    if all.len() > 1
        && matches!(
            input.sec_type.as_deref(),
            Some("FUT") | Some("FOP")
        )
        && input.expiry.as_ref().map_or(true, |e| e.is_empty())
    {
        all.sort_by(|a, b| {
            a.contract
                .last_trade_date_or_contract_month
                .cmp(&b.contract.last_trade_date_or_contract_month)
        });
    }
    Ok(all.into_iter().next().unwrap())
}

async fn resolve_target_account(client: &Client, preferred: Option<String>) -> Result<String> {
    if let Some(account) = preferred.filter(|value| !value.trim().is_empty()) {
        return Ok(account);
    }

    let accounts = client
        .managed_accounts()
        .await
        .map_err(map_ibapi_error)
        .map_err(Error::Provider)?;

    accounts
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::Provider("no IBKR account available".to_string()))
}

async fn drain_snapshot_ticks(
    subscription: &mut ibapi::subscriptions::Subscription<TickTypes>,
    data: &mut SnapshotAccumulator,
    timeout_secs: u64,
) -> Result<()> {
    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    // Delayed data (market_data_type 3) never sends SnapshotEnd, so the full
    // timeout_secs would be burned on every call.  Instead, once we have at
    // least one observation we switch to a short idle timeout: if no new ticks
    // arrive within 500ms we assume the burst is over and return what we have.
    let idle_timeout = StdDuration::from_millis(500);
    loop {
        let remaining = remaining_time(deadline)?;
        let wait = if data.has_observation() {
            remaining.min(idle_timeout)
        } else {
            remaining
        };
        match timeout(wait, subscription.next()).await {
            Ok(Some(Ok(TickTypes::Price(price)))) => {
                apply_tick_price(data, price.tick_type, price.price)
            }
            Ok(Some(Ok(TickTypes::String(string_tick)))) => {
                apply_tick_string(data, string_tick.tick_type, &string_tick.value)
            }
            Ok(Some(Ok(TickTypes::SnapshotEnd))) => break,
            Ok(Some(Ok(TickTypes::PriceSize(price_size)))) => {
                apply_tick_price(data, price_size.price_tick_type, price_size.price);
            }
            Ok(Some(Ok(TickTypes::Notice(_)))) => {}
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => {
                if data.has_observation() {
                    break;
                }
                return Err(Error::Provider(
                    "timeout waiting for ibkr snapshot ticks".to_string(),
                ));
            }
        }
    }
    Ok(())
}

async fn fetch_contract_snapshot(
    client: &Client,
    contract: &Contract,
    timeout_secs: u64,
) -> Result<SnapshotAccumulator> {
    let mut subscription = timeout(
        StdDuration::from_secs(timeout_secs),
        client.market_data(contract).snapshot().subscribe(),
    )
    .await
    .map_err(|_| Error::Provider("timeout creating ibkr snapshot".to_string()))?
    .map_err(map_ibapi_error)
    .map_err(Error::Provider)?;

    let mut data = SnapshotAccumulator::default();
    drain_snapshot_ticks(&mut subscription, &mut data, timeout_secs).await?;
    Ok(data)
}

fn apply_tick_price(data: &mut SnapshotAccumulator, tick_type: TickType, price: f64) {
    if !price.is_finite() || price < 0.0 {
        return;
    }

    match tick_type {
        TickType::Last | TickType::DelayedLast => data.current_price = Some(price),
        TickType::MarkPrice if data.current_price.is_none() => data.current_price = Some(price),
        TickType::Close => data.previous_close = Some(price),
        TickType::Open | TickType::DelayedOpen => data.open = Some(price),
        TickType::Low | TickType::DelayedLow => data.day_low = Some(price),
        TickType::High | TickType::DelayedHigh => data.day_high = Some(price),
        _ => {}
    }
}

fn apply_tick_string(data: &mut SnapshotAccumulator, tick_type: TickType, value: &str) {
    match tick_type {
        TickType::LastTimestamp | TickType::DelayedLastTimestamp => {
            if let Some(epoch) = parse_epoch_seconds(value) {
                data.last_timestamp = Some(epoch);
            }
        }
        _ => {}
    }
}

fn build_contract(input: &ContractInput) -> Result<Contract> {
    let symbol = input.symbol.trim().to_ascii_uppercase();
    if symbol.is_empty() {
        return Err(Error::InvalidInput(
            "contract symbol is required".to_string(),
        ));
    }

    Ok(Contract {
        symbol: symbol.into(),
        security_type: parse_security_type(input.sec_type.as_deref().unwrap_or("STK")),
        exchange: input
            .exchange
            .clone()
            .unwrap_or_else(|| "SMART".to_string())
            .into(),
        primary_exchange: input.primary_exchange.clone().unwrap_or_default().into(),
        currency: input
            .currency
            .clone()
            .unwrap_or_else(|| "USD".to_string())
            .into(),
        last_trade_date_or_contract_month: input.expiry.clone().unwrap_or_default(),
        strike: input.strike.unwrap_or_default(),
        right: input.right.clone().unwrap_or_default(),
        multiplier: input.multiplier.clone().unwrap_or_default(),
        trading_class: input.trading_class.clone().unwrap_or_default(),
        ..Contract::default()
    })
}

fn build_order(payload: &PlaceOrderPayload, account: Option<String>) -> Result<Order> {
    let side = payload.side.trim().to_ascii_uppercase();
    let order_type = payload
        .order_type
        .clone()
        .unwrap_or_else(|| "MKT".to_string())
        .trim()
        .to_ascii_uppercase();
    let tif = payload
        .tif
        .clone()
        .unwrap_or_else(|| "DAY".to_string())
        .trim()
        .to_ascii_uppercase();

    if payload.quantity <= 0.0 {
        return Err(Error::InvalidInput("quantity must be > 0".to_string()));
    }

    let mut order = Order::default();
    order.action = parse_order_action(&side)?;
    order.order_type = order_type.clone();
    order.total_quantity = payload.quantity;
    order.tif = TimeInForce::from(tif.as_str());
    order.account = account.unwrap_or_default();

    if matches!(order_type.as_str(), "LMT" | "STP LMT") {
        order.limit_price = Some(payload.limit_price.ok_or_else(|| {
            Error::InvalidInput("limit_price is required for limit orders".to_string())
        })?);
    }
    if matches!(order_type.as_str(), "STP" | "STP LMT" | "TRAIL") {
        if let Some(stop_price) = payload.stop_price {
            order.aux_price = Some(stop_price);
        }
    }

    Ok(order)
}

async fn collect_place_order_result(
    subscription: &mut ibapi::subscriptions::Subscription<PlaceOrder>,
    order_id: i32,
    contract: &Contract,
    order: &Order,
    timeout_secs: u64,
) -> Result<Value> {
    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    let mut latest_status = json!({ "order_id": order_id });

    loop {
        let remaining = remaining_time(deadline)?;
        match timeout(remaining, subscription.next()).await {
            Ok(Some(Ok(PlaceOrder::OrderStatus(status)))) => {
                latest_status = serialize_order_status(&status);
                if is_terminal_order_status(&status.status) {
                    break;
                }
            }
            Ok(Some(Ok(PlaceOrder::OpenOrder(order_data)))) => {
                latest_status = json!({
                    "order_id": order_data.order_id,
                    "status": order_data.order_state.status,
                });
            }
            Ok(Some(Ok(PlaceOrder::ExecutionData(exec)))) => {
                latest_status = json!({
                    "order_id": exec.execution.order_id,
                    "status": "Filled",
                    "filled": exec.execution.shares,
                    "remaining": 0.0,
                    "avg_fill_price": exec.execution.average_price,
                    "perm_id": exec.execution.perm_id,
                    "parent_id": 0,
                    "last_fill_price": exec.execution.price,
                    "client_id": exec.execution.client_id,
                    "why_held": "",
                    "mkt_cap_price": 0.0,
                });
                break;
            }
            Ok(Some(Ok(PlaceOrder::CommissionReport(_)))) => {}
            Ok(Some(Ok(PlaceOrder::Message(_)))) => {}
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if let Some(obj) = latest_status.as_object_mut() {
        obj.insert("contract".to_string(), serialize_contract(contract));
        obj.insert("order".to_string(), serialize_order(order));
    }

    Ok(latest_status)
}

async fn collect_cancel_order_result(
    subscription: &mut ibapi::subscriptions::Subscription<CancelOrder>,
    order_id: i32,
    timeout_secs: u64,
) -> Result<Value> {
    let deadline = Instant::now() + StdDuration::from_secs(timeout_secs);
    let mut latest_status = json!({ "order_id": order_id });

    loop {
        let remaining = remaining_time(deadline)?;
        match timeout(remaining, subscription.next()).await {
            Ok(Some(Ok(CancelOrder::OrderStatus(status)))) => {
                latest_status = serialize_order_status(&status);
                if matches!(
                    status.status.as_str(),
                    "PendingCancel" | "Cancelled" | "ApiCancelled"
                ) {
                    break;
                }
            }
            Ok(Some(Ok(CancelOrder::Notice(_)))) => {}
            Ok(Some(Err(err))) => return Err(Error::Provider(map_ibapi_error(err))),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    Ok(latest_status)
}

fn serialize_contract(contract: &Contract) -> Value {
    json!({
        "con_id": contract.contract_id,
        "symbol": contract.symbol.to_string(),
        "sec_type": contract.security_type.to_string(),
        "exchange": contract.exchange.to_string(),
        "primary_exchange": contract.primary_exchange.to_string(),
        "currency": contract.currency.to_string(),
        "local_symbol": contract.local_symbol,
        "trading_class": contract.trading_class,
        "expiry": contract.last_trade_date_or_contract_month,
        "strike": if contract.strike == 0.0 { Value::Null } else { json!(contract.strike) },
        "right": contract.right,
        "multiplier": contract.multiplier,
    })
}

fn serialize_order(order: &Order) -> Value {
    json!({
        "action": order.action.to_string(),
        "order_type": order.order_type,
        "total_quantity": order.total_quantity,
        "limit_price": order.limit_price,
        "aux_price": order.aux_price,
        "tif": order.tif.to_string(),
        "account": order.account,
    })
}

fn serialize_order_status(status: &OrderStatus) -> Value {
    json!({
        "order_id": status.order_id,
        "status": status.status,
        "filled": status.filled,
        "remaining": status.remaining,
        "avg_fill_price": status.average_fill_price,
        "perm_id": status.perm_id,
        "parent_id": status.parent_id,
        "last_fill_price": status.last_fill_price,
        "client_id": status.client_id,
        "why_held": status.why_held,
        "mkt_cap_price": status.market_cap_price,
    })
}

fn requested_market_data_type(connection: &IbkrConnectionConfig) -> i32 {
    connection.market_data_type.unwrap_or(3)
}

fn market_data_type_name(kind: i32) -> &'static str {
    match kind {
        1 => "live",
        2 => "frozen",
        3 => "delayed",
        4 => "delayed_frozen",
        _ => "unknown",
    }
}

fn market_data_type_from_i32(kind: i32) -> MarketDataType {
    match kind {
        1 => MarketDataType::Realtime,
        2 => MarketDataType::Frozen,
        4 => MarketDataType::DelayedFrozen,
        _ => MarketDataType::Delayed,
    }
}

fn ibkr_bar_size(granularity: &Span) -> Result<HistoricalBarSize> {
    match (granularity.n, granularity.unit) {
        (1, SpanUnit::Minute) => Ok(HistoricalBarSize::Min),
        (2, SpanUnit::Minute) => Ok(HistoricalBarSize::Min2),
        (3, SpanUnit::Minute) => Ok(HistoricalBarSize::Min3),
        (5, SpanUnit::Minute) => Ok(HistoricalBarSize::Min5),
        (10, SpanUnit::Minute) => Ok(HistoricalBarSize::Min10),
        (15, SpanUnit::Minute) => Ok(HistoricalBarSize::Min15),
        (20, SpanUnit::Minute) => Ok(HistoricalBarSize::Min20),
        (30, SpanUnit::Minute) => Ok(HistoricalBarSize::Min30),
        (1, SpanUnit::Hour) => Ok(HistoricalBarSize::Hour),
        (2, SpanUnit::Hour) => Ok(HistoricalBarSize::Hour2),
        (3, SpanUnit::Hour) => Ok(HistoricalBarSize::Hour3),
        (4, SpanUnit::Hour) => Ok(HistoricalBarSize::Hour4),
        (8, SpanUnit::Hour) => Ok(HistoricalBarSize::Hour8),
        (1, SpanUnit::Day) => Ok(HistoricalBarSize::Day),
        (1, SpanUnit::Week) => Ok(HistoricalBarSize::Week),
        (1, SpanUnit::Month) => Ok(HistoricalBarSize::Month),
        _ => Err(Error::InvalidInput(format!(
            "ibkr provider does not support granularity {}",
            granularity.to_string_compact()
        ))),
    }
}

fn ibkr_duration_for_span(span: &Span) -> Result<HistoricalDuration> {
    let value = i32::try_from(span.n).map_err(|_| {
        Error::InvalidInput(format!(
            "ibkr range is too large: {}",
            span.to_string_compact()
        ))
    })?;
    match span.unit {
        SpanUnit::Minute => value
            .checked_mul(60)
            .map(HistoricalDuration::seconds)
            .ok_or_else(|| {
                Error::InvalidInput(format!(
                    "ibkr range is too large: {}",
                    span.to_string_compact()
                ))
            }),
        SpanUnit::Hour => value
            .checked_mul(3600)
            .map(HistoricalDuration::seconds)
            .ok_or_else(|| {
                Error::InvalidInput(format!(
                    "ibkr range is too large: {}",
                    span.to_string_compact()
                ))
            }),
        SpanUnit::Day => Ok(HistoricalDuration::days(value)),
        SpanUnit::Week => Ok(HistoricalDuration::weeks(value)),
        SpanUnit::Month => Ok(HistoricalDuration::months(value)),
        SpanUnit::Year => Ok(HistoricalDuration::years(value)),
    }
}

fn parse_security_type(raw: &str) -> SecurityType {
    SecurityType::from(raw.trim().to_ascii_uppercase().as_str())
}

fn parse_order_action(raw: &str) -> Result<ibapi::orders::Action> {
    match raw {
        "BUY" => Ok(ibapi::orders::Action::Buy),
        "SELL" => Ok(ibapi::orders::Action::Sell),
        "SSHORT" => Ok(ibapi::orders::Action::SellShort),
        "SLONG" => Ok(ibapi::orders::Action::SellLong),
        other => Err(Error::InvalidInput(format!(
            "unsupported ibkr order side '{other}'"
        ))),
    }
}

fn parse_epoch_seconds(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = trimmed.parse::<i64>().ok()?;
    if parsed > 1_000_000_000_000 {
        Some(parsed / 1000)
    } else {
        Some(parsed)
    }
}

fn to_offset_datetime(value: chrono::DateTime<Utc>) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(value.timestamp())
        .map_err(|e| Error::Provider(format!("invalid ibkr timestamp conversion: {e}")))
}

fn to_chrono_datetime(value: OffsetDateTime) -> Result<chrono::DateTime<Utc>> {
    Utc.timestamp_opt(value.unix_timestamp(), value.nanosecond())
        .single()
        .ok_or_else(|| Error::Provider("invalid ibkr timestamp".to_string()))
}

fn remaining_time(deadline: Instant) -> Result<StdDuration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| Error::Provider("timeout waiting for ibkr response".to_string()))
}

fn is_terminal_order_status(status: &str) -> bool {
    matches!(
        status,
        "Filled" | "Cancelled" | "ApiCancelled" | "Inactive" | "PendingCancel"
    )
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn map_ibapi_error(err: ibapi::Error) -> String {
    err.to_string()
}
