use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PaperPositionState {
    provider: PaperProvider,
    market_ticker: String,
    side: PaperSide,
    quantity: f64,
    avg_price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    mark_price: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PaperAccountState {
    starting_cash: f64,
    cash: f64,
    realized_pnl: f64,
    positions: HashMap<String, PaperPositionState>,
    trades: Vec<PaperTradeFill>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PaperState {
    version: u32,
    updated_at: DateTime<Utc>,
    accounts: HashMap<String, PaperAccountState>,
}

impl Default for PaperState {
    fn default() -> Self {
        Self {
            version: 1,
            updated_at: Utc::now(),
            accounts: HashMap::new(),
        }
    }
}

pub async fn run_paper(req: PaperRequest) -> Result<PaperResponse> {
    if req.mode == PaperMode::KalshiDemo {
        return Err(Error::InvalidInput(
            "mode 'kalshi_demo' is not implemented yet; use mode='simulated'".to_string(),
        ));
    }

    let account_name = req
        .account
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("default")
        .to_string();

    let state_path = resolve_paper_state_path(req.cache_dir.as_deref());
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Provider(format!("create paper cache dir failed: {e}")))?;
    }

    let mut state = load_state(&state_path)?;
    let default_cash = req.starting_cash.unwrap_or(10_000.0).max(0.0);
    let account = state
        .accounts
        .entry(account_name.clone())
        .or_insert_with(|| PaperAccountState {
            starting_cash: default_cash,
            cash: default_cash,
            realized_pnl: 0.0,
            positions: HashMap::new(),
            trades: Vec::new(),
        });

    if req.command == PaperCommand::Reset {
        let reset_cash = req.starting_cash.unwrap_or(account.starting_cash).max(0.0);
        account.starting_cash = reset_cash;
        account.cash = reset_cash;
        account.realized_pnl = 0.0;
        account.positions.clear();
        account.trades.clear();
    }

    let mut last_trade = None;
    if req.command == PaperCommand::Trade {
        let provider = req.provider.clone().ok_or_else(|| {
            Error::InvalidInput("provider is required for paper trade".to_string())
        })?;
        let market_ticker = req
            .market_ticker
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                Error::InvalidInput("market_ticker is required for paper trade".to_string())
            })?
            .to_string();
        let side = req
            .side
            .clone()
            .ok_or_else(|| Error::InvalidInput("side is required for paper trade".to_string()))?;
        let action = req
            .action
            .clone()
            .ok_or_else(|| Error::InvalidInput("action is required for paper trade".to_string()))?;
        let quantity = req.quantity.ok_or_else(|| {
            Error::InvalidInput("quantity is required for paper trade".to_string())
        })?;

        if !quantity.is_finite() || quantity <= 0.0 {
            return Err(Error::InvalidInput(
                "quantity must be a positive finite number".to_string(),
            ));
        }

        let fill_price = if let Some(limit_price) = req.limit_price {
            if !limit_price.is_finite() {
                return Err(Error::InvalidInput(
                    "limit_price must be finite when provided".to_string(),
                ));
            }
            limit_price.clamp(0.0, 1.0)
        } else {
            let yes_prob = resolve_yes_probability(&provider, &market_ticker).await?;
            price_for_side(yes_prob, &side)
        };

        let fill_price = round6(fill_price);
        let quantity = round6(quantity);
        let notional = round6(fill_price * quantity);
        let position_key = format!(
            "{}:{}:{}",
            provider_tag(&provider),
            market_ticker,
            side_tag(&side)
        );

        let mut should_remove = false;
        {
            let pos = account
                .positions
                .entry(position_key.clone())
                .or_insert_with(|| PaperPositionState {
                    provider: provider.clone(),
                    market_ticker: market_ticker.clone(),
                    side: side.clone(),
                    quantity: 0.0,
                    avg_price: fill_price,
                    mark_price: Some(fill_price),
                });

            match action {
                PaperOrderAction::Buy => {
                    if account.cash + 1e-9 < notional {
                        return Err(Error::InvalidInput(format!(
                            "insufficient paper cash: required {:.4}, available {:.4}",
                            notional, account.cash
                        )));
                    }
                    account.cash = round6(account.cash - notional);
                    let new_qty = pos.quantity + quantity;
                    if new_qty > 0.0 {
                        pos.avg_price = round6(
                            ((pos.avg_price * pos.quantity) + (fill_price * quantity)) / new_qty,
                        );
                    }
                    pos.quantity = round6(new_qty);
                }
                PaperOrderAction::Sell => {
                    if pos.quantity + 1e-9 < quantity {
                        return Err(Error::InvalidInput(format!(
                            "insufficient position size: have {:.4}, requested {:.4}",
                            pos.quantity, quantity
                        )));
                    }
                    account.cash = round6(account.cash + notional);
                    account.realized_pnl =
                        round6(account.realized_pnl + ((fill_price - pos.avg_price) * quantity));
                    pos.quantity = round6(pos.quantity - quantity);
                    if pos.quantity <= 1e-9 {
                        should_remove = true;
                    }
                }
            }

            pos.mark_price = Some(fill_price);
        }

        if should_remove {
            account.positions.remove(&position_key);
        }

        let fill = PaperTradeFill {
            id: format!(
                "{}-{}",
                Utc::now().timestamp_millis(),
                account.trades.len() + 1
            ),
            ts: Utc::now(),
            provider,
            market_ticker,
            side,
            action,
            quantity,
            price: fill_price,
            notional,
            note: if req.limit_price.is_some() {
                Some("filled at user limit_price".to_string())
            } else {
                Some("filled at live midpoint probability".to_string())
            },
        };

        account.trades.push(fill.clone());
        if account.trades.len() > 5_000 {
            let drop_n = account.trades.len().saturating_sub(5_000);
            account.trades.drain(0..drop_n);
        }
        last_trade = Some(fill);
    }

    if req.command == PaperCommand::Trade
        || req.command == PaperCommand::Mark
        || req.command == PaperCommand::Positions
    {
        refresh_marks(account).await;
    }

    state.updated_at = Utc::now();
    save_state(&state_path, &state)?;

    let account = state
        .accounts
        .get(&account_name)
        .ok_or_else(|| Error::Provider("paper account missing after save".to_string()))?;

    let mut positions: Vec<PaperPosition> = account
        .positions
        .values()
        .map(|p| {
            let cost_basis = p.avg_price * p.quantity;
            let mark_price = p.mark_price;
            let market_value = mark_price.map(|m| m * p.quantity);
            let unrealized = mark_price.map(|m| (m - p.avg_price) * p.quantity);
            PaperPosition {
                provider: p.provider.clone(),
                market_ticker: p.market_ticker.clone(),
                side: p.side.clone(),
                quantity: p.quantity,
                avg_price: p.avg_price,
                mark_price,
                cost_basis: round6(cost_basis),
                market_value: market_value.map(round6),
                unrealized_pnl: unrealized.map(round6),
            }
        })
        .collect();

    positions.sort_by(|a, b| {
        provider_tag(&a.provider)
            .cmp(provider_tag(&b.provider))
            .then(a.market_ticker.cmp(&b.market_ticker))
            .then(side_tag(&a.side).cmp(side_tag(&b.side)))
    });

    let unrealized = positions
        .iter()
        .filter_map(|p| p.unrealized_pnl)
        .sum::<f64>();
    let equity = account.cash
        + positions
            .iter()
            .map(|p| p.market_value.unwrap_or(p.cost_basis))
            .sum::<f64>();

    let summary = PaperAccountSummary {
        cash: round6(account.cash),
        realized_pnl: round6(account.realized_pnl),
        unrealized_pnl: round6(unrealized),
        equity: round6(equity),
        positions_open: positions.len(),
        trades_total: account.trades.len(),
    };

    let trades = match req.command {
        PaperCommand::Trades => tail_trades(&account.trades, req.limit.unwrap_or(50)),
        PaperCommand::Trade => tail_trades(&account.trades, 10),
        _ => Vec::new(),
    };

    Ok(PaperResponse {
        generated_at: Utc::now(),
        account: account_name,
        command: req.command,
        mode: req.mode,
        summary,
        positions,
        trades,
        last_trade,
        state_path: state_path.to_string_lossy().to_string(),
    })
}

async fn refresh_marks(account: &mut PaperAccountState) {
    let keys: Vec<String> = account.positions.keys().cloned().collect();
    let mut resolved: HashMap<String, Option<f64>> = HashMap::new();

    for key in keys {
        let Some(pos) = account.positions.get(&key).cloned() else {
            continue;
        };

        let lookup = format!("{}:{}", provider_tag(&pos.provider), pos.market_ticker);
        let yes_prob = if let Some(existing) = resolved.get(&lookup).cloned() {
            existing
        } else {
            let fetched = resolve_yes_probability(&pos.provider, &pos.market_ticker)
                .await
                .ok();
            resolved.insert(lookup.clone(), fetched);
            fetched
        };

        if let Some(prob) = yes_prob {
            let mark = price_for_side(prob, &pos.side);
            if let Some(target) = account.positions.get_mut(&key) {
                target.mark_price = Some(mark);
            }
        }
    }
}

fn tail_trades(trades: &[PaperTradeFill], n: usize) -> Vec<PaperTradeFill> {
    if n == 0 {
        return Vec::new();
    }
    trades
        .iter()
        .rev()
        .take(n)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

async fn resolve_yes_probability(provider: &PaperProvider, market_ticker: &str) -> Result<f64> {
    let provider_name = provider_tag(provider).to_string();
    let odds = fetch_odds(OddsRequest {
        provider: Some(provider_name),
        market_ticker: Some(market_ticker.to_string()),
        ..Default::default()
    })
    .await?;

    let market = odds
        .markets
        .first()
        .ok_or_else(|| Error::Provider("market not found for paper fill".to_string()))?;

    if let Some(prob) = market.probability_yes {
        return Ok(prob.clamp(0.0, 1.0));
    }
    if let Some(cents) = market.yes_price {
        return Ok((cents as f64 / 100.0).clamp(0.0, 1.0));
    }

    Err(Error::Provider(
        "market price missing for paper fill".to_string(),
    ))
}

fn resolve_paper_state_path(cache_dir: Option<&str>) -> PathBuf {
    if let Some(dir) = cache_dir {
        let p = PathBuf::from(dir);
        if p.extension().is_some() {
            return p;
        }
        return p.join("paper_state.json");
    }

    directories::ProjectDirs::from("", "", "eli")
        .map(|d| d.cache_dir().join("finance").join("paper_state.json"))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-paper-state.json"))
}

fn load_state(path: &Path) -> Result<PaperState> {
    if !path.exists() {
        return Ok(PaperState::default());
    }

    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Provider(format!("read paper state failed: {e}")))?;
    serde_json::from_str::<PaperState>(&raw)
        .map_err(|e| Error::Provider(format!("parse paper state failed: {e}")))
}

fn save_state(path: &Path, state: &PaperState) -> Result<()> {
    let tmp_path = path.with_extension("json.tmp");
    let raw = serde_json::to_string_pretty(state)
        .map_err(|e| Error::Provider(format!("serialize paper state failed: {e}")))?;

    std::fs::write(&tmp_path, raw)
        .map_err(|e| Error::Provider(format!("write paper state temp failed: {e}")))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| Error::Provider(format!("commit paper state failed: {e}")))
}

fn provider_tag(provider: &PaperProvider) -> &'static str {
    match provider {
        PaperProvider::Kalshi => "kalshi",
        PaperProvider::Polymarket => "polymarket",
    }
}

fn side_tag(side: &PaperSide) -> &'static str {
    match side {
        PaperSide::Yes => "yes",
        PaperSide::No => "no",
    }
}

fn price_for_side(probability_yes: f64, side: &PaperSide) -> f64 {
    let p = probability_yes.clamp(0.0, 1.0);
    match side {
        PaperSide::Yes => p,
        PaperSide::No => (1.0 - p).clamp(0.0, 1.0),
    }
}

fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}
