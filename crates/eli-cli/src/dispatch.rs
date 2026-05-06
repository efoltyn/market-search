pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| {
                    "error,eli=warn,eli_cli=warn".to_string()
                }),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::try_parse()?;

    match cli.cmd {
        None => {
            // Default: show help. (Previously launched the chat agent; that's gone.)
            use clap::CommandFactory as _;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Command::Setup) => cmd_setup().await,
        Some(Command::Init) => cmd_init().await,
        Some(Command::Config { set, value }) => cmd_config(set, value).await,
        Some(Command::ToolInfo { path }) => cmd_tool_info(path),
        Some(Command::Finance { cmd }) => cmd_finance(cmd).await,
        Some(Command::Web { cmd }) => cmd_web(cmd).await,
        Some(Command::Mcp(args)) => {
            if let Some(McpSubcommand::Share(share_args)) = args.cmd {
                cmd_mcp_share(share_args).await
            } else if args.http {
                cmd_mcp_http(args.port).await
            } else {
                cmd_mcp().await
            }
        }
        Some(Command::Picks { cmd }) => cmd_picks(cmd).await,
    }
}

async fn cmd_finance(cmd: FinanceCommand) -> Result<()> {
    match cmd {
        FinanceCommand::Timeseries(args) => cmd_finance_timeseries(args).await,
        FinanceCommand::Fundamentals(args) => cmd_finance_fundamentals(args).await,
        FinanceCommand::Search(args) => cmd_finance_search(args).await,
        FinanceCommand::Filings(args) | FinanceCommand::Sec(args) => {
            cmd_finance_filings(args).await
        }
        FinanceCommand::Schedule(args) => cmd_finance_schedule(args).await,
        FinanceCommand::RatePath(args) => cmd_finance_rate_path(args).await,
        FinanceCommand::Odds(args) => cmd_finance_odds(args).await,
        FinanceCommand::Options(args) => cmd_finance_options(args).await,
        FinanceCommand::Sync(args) => cmd_finance_sync(args).await,
        FinanceCommand::Paper(args) => cmd_finance_paper(args).await,
        FinanceCommand::Ibkr(args) => cmd_finance_ibkr(args).await,
        FinanceCommand::Auctions(args) => cmd_finance_auctions(args).await,
        FinanceCommand::Cot(args) => cmd_finance_cot(args).await,
        FinanceCommand::Curve(args) => cmd_finance_curve(args).await,
        FinanceCommand::Nyfed(args) => cmd_finance_nyfed(args).await,
        FinanceCommand::Volsurface(args) => cmd_finance_volsurface(args).await,
        FinanceCommand::Stress(args) => cmd_finance_stress(args).await,
        FinanceCommand::Fiscal(args) => cmd_finance_fiscal(args).await,
        FinanceCommand::Ecb(args) => cmd_finance_ecb(args).await,
        FinanceCommand::Eia(args) => cmd_finance_eia(args).await,
        FinanceCommand::Bis(args) => cmd_finance_bis(args).await,
        FinanceCommand::Boj(args) => cmd_finance_boj(args).await,
        FinanceCommand::Boe(args) => cmd_finance_boe(args).await,
    }
}
