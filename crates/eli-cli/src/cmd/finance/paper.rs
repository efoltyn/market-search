use eli_core::finance::{
    PaperCommand, PaperMode, PaperOrderAction, PaperProvider, PaperRequest, PaperSide,
};

async fn cmd_finance_paper(args: FinancePaperArgs) -> Result<()> {
    if args.format.trim() != "json" {
        return Err(anyhow::anyhow!("unsupported format: {}", args.format));
    }

    let req = PaperRequest {
        account: Some(args.account),
        command: map_paper_command(args.command),
        mode: map_paper_mode(args.mode),
        provider: match args.provider.as_deref() {
            Some(v) => Some(parse_paper_provider(v)?),
            None => None,
        },
        market_ticker: args.market,
        side: args.side.map(map_paper_side),
        action: args.action.map(map_paper_action),
        quantity: args.qty,
        limit_price: args.price,
        starting_cash: args.starting_cash,
        limit: args.limit,
        cache_dir: args.cache_dir.map(|p| p.to_string_lossy().to_string()),
    };

    let resp = eli_core::finance::run_paper(req).await?;
    let json = serde_json::to_string_pretty(&resp)?;

    if let Some(path) = args.out {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, json)?;
        println!(
            "{}",
            serde_json::json!({"ok": true, "path": path.to_string_lossy()})
        );
    } else {
        println!("{}", json);
    }

    Ok(())
}

fn map_paper_command(v: FinancePaperCommandArg) -> PaperCommand {
    match v {
        FinancePaperCommandArg::Trade => PaperCommand::Trade,
        FinancePaperCommandArg::Positions => PaperCommand::Positions,
        FinancePaperCommandArg::Trades => PaperCommand::Trades,
        FinancePaperCommandArg::Mark => PaperCommand::Mark,
        FinancePaperCommandArg::Reset => PaperCommand::Reset,
    }
}

fn map_paper_mode(v: FinancePaperModeArg) -> PaperMode {
    match v {
        FinancePaperModeArg::Simulated => PaperMode::Simulated,
        FinancePaperModeArg::KalshiDemo => PaperMode::KalshiDemo,
    }
}

fn parse_paper_provider(v: &str) -> Result<PaperProvider> {
    match v.trim().to_ascii_lowercase().as_str() {
        "kalshi" => Ok(PaperProvider::Kalshi),
        "polymarket" | "poly" => Ok(PaperProvider::Polymarket),
        other => Err(anyhow::anyhow!(
            "invalid --provider '{other}' (expected kalshi|polymarket)"
        )),
    }
}

fn map_paper_side(v: FinancePaperSideArg) -> PaperSide {
    match v {
        FinancePaperSideArg::Yes => PaperSide::Yes,
        FinancePaperSideArg::No => PaperSide::No,
    }
}

fn map_paper_action(v: FinancePaperOrderActionArg) -> PaperOrderAction {
    match v {
        FinancePaperOrderActionArg::Buy => PaperOrderAction::Buy,
        FinancePaperOrderActionArg::Sell => PaperOrderAction::Sell,
    }
}
