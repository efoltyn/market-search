pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| {
                    "error,eli=warn,eli_cli=warn,chromiumoxide=off,chromiumoxide::conn::raw_ws::parse_errors=off".to_string()
                }),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::try_parse()?;

    match cli.cmd {
        None => cmd_chat(cli.provider, cli.model, None).await,
        Some(Command::Setup) => cmd_setup().await,
        Some(Command::Init) => cmd_init().await,
        Some(Command::Config { set, value }) => cmd_config(set, value).await,
        Some(Command::ToolInfo { path }) => cmd_tool_info(path),
        Some(Command::Chat) => cmd_chat(cli.provider, cli.model, None).await,
        Some(Command::Debug) => cmd_chat(cli.provider, cli.model, Some(DisplayMode::Debug)).await,
        Some(Command::Raw) => cmd_chat(cli.provider, cli.model, Some(DisplayMode::Raw)).await,
        Some(Command::Research { query }) => cmd_research(query, cli.provider, cli.model).await,
        Some(Command::Tui) => cmd_tui(cli.provider, cli.model).await,
        Some(Command::Finance { cmd }) => cmd_finance(cmd).await,
        Some(Command::Web { cmd }) => cmd_web(cmd).await,
        Some(Command::Agent { cmd }) => cmd_agent(cmd, cli.provider, cli.model).await,
        Some(Command::Code(args)) => cmd_code(args).await,
        Some(Command::Sentinel { cmd }) => cmd_sentinel(cmd).await,
        Some(Command::Mcp(args)) => {
            if args.http {
                cmd_mcp_http(args.port).await
            } else {
                cmd_mcp().await
            }
        }
        Some(Command::Picks { cmd }) => cmd_picks(cmd).await,
        Some(Command::Serve(_args)) => anyhow::bail!("serve command temporarily disabled"),
    }
}

async fn cmd_research(
    query: String,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let mut cfg = config::load_or_create(&paths).context("load/create config")?;
    apply_overrides(&mut cfg, provider, model)?;

    // Research defaults: safe, autonomous, non-destructive.
    cfg.chat.mode = RunMode::Read;
    cfg.chat.approvals = ApprovalMode::Auto;
    cfg.chat.auto = true;
    cfg.chat.max_auto = cfg.chat.max_auto.min(12).max(1);
    cfg.chat.compact_trigger_tokens = Some(
        cfg.chat
            .resolved_compact_trigger_tokens()
            .unwrap_or(100_000)
            .min(30_000),
    );
    if env_truthy("ELI_AGENT_FAST") {
        cfg.chat.max_auto = cfg.chat.max_auto.min(6).max(1);
        cfg.chat.compact_trigger_tokens = Some(
            cfg.chat
                .resolved_compact_trigger_tokens()
                .unwrap_or(30_000)
                .min(15_000),
        );
        cfg.chat.max_cmds = cfg.chat.max_cmds.min(3).max(1);
        if cfg.chat.max_tokens.is_none() {
            cfg.chat.max_tokens = Some(3200);
        }
    }
    // Force plain/non-footer output when external clients request it.
    if env_truthy("ELI_PLAIN_OUTPUT") || env_truthy("ELI_NO_FOOTER") {
        cfg.chat.display_mode = DisplayMode::Brain;
    }

    let adapter = eli_adapters::build_from_chat_config(&cfg.chat).context("build adapter")?;
    let adapter: Arc<dyn LlmAdapter> = Arc::from(adapter);

    let cwd = std::env::current_dir().context("get cwd")?;
    let project_root = cfg
        .chat
        .resolved_project_root(&cwd)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve project root")?;

    ensure_eli_research_brain(&project_root).context("ensure eli_research/ELI.md")?;

    let diff_engine = DiffEngine::new(project_root.clone()).context("init diff engine")?;
    let command_runner = CommandRunner::new(
        cfg.chat.timeout_secs,
        cfg.chat.max_cmds,
        cfg.chat.parallel_commands,
        project_root.clone(),
    );

    let store = SessionStore::new(&paths);
    let session_id = uuid::Uuid::new_v4().to_string();

    // Ensure instincts directory exists
    let instincts_dir = project_root.join("instincts");
    if !instincts_dir.exists() {
        let _ = std::fs::create_dir_all(&instincts_dir);
    }

    info!(session_id = %session_id, provider = %cfg.chat.provider, model = %cfg.chat.model, "starting research");

    let mut memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
    memory.set_system(eli_core::contract::system_prompt());

    // Inject existing instincts into memory
    if instincts_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&instincts_dir) {
            for entry in entries.flatten() {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    let filename = entry.file_name().to_string_lossy().to_string();
                    memory.push(ChatMessage::system(format!(
                        "INSTINCT ({filename}):\n{content}"
                    )));
                }
            }
        }
    }
    let mut undo_stack: Vec<Vec<DiffResult>> = Vec::new();
    let mut state = SessionState::new(&cfg.chat);
    state.load_recent_research(&project_root, 12);
    if let Ok(agent_context) = std::env::var("ELI_AGENT_CONTEXT") {
        let ctx = agent_context.trim();
        if !ctx.is_empty() {
            memory.push(ChatMessage::system(format!(
                "AGENT EXECUTION CONTEXT:\n{ctx}"
            )));
        }
    }

    if is_trivial_query(&query) {
        let answer = "Hello. What should I focus on?";
        let assistant = serde_json::json!({
            "plan": format!(
                "MODE: READ | APPROVALS: AUTO | ROOT: {} | Trivial query detected; no tool calls needed.",
                project_root.display()
            ),
            "checklist": [],
            "focus": "Clarify user intent",
            "status": "DONE",
            "commands": [],
            "commands_parallel": false,
            "screen": [],
            "diffs": [],
            "subagents": [],
            "synthesis": {
                "summary": [],
                "answer": answer,
                "next_steps": []
            },
            "ask_user": "",
            "notes": answer
        })
        .to_string();

        store
            .append(
                &session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::UserMessage {
                        content: query.clone(),
                    },
                },
            )
            .await?;
        store
            .append(
                &session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::AssistantMessage { content: assistant },
                },
            )
            .await?;

        println!("{answer}");
        return Ok(());
    }

    if has_interactive_terminal() {
        print_banner(&cfg.chat, &project_root, &state);
    }

    run_agent_steps(
        &cfg.chat,
        adapter.clone(),
        &diff_engine,
        &command_runner,
        &store,
        &paths.data_dir,
        &session_id,
        &project_root,
        &mut memory,
        &mut undo_stack,
        &mut state,
        AgentProfile::Research,
        query,
        Vec::new(),
    )
    .await?;

    if has_interactive_terminal() && !env_truthy("ELI_PLAIN_OUTPUT") && !env_truthy("ELI_NO_FOOTER")
    {
        print_cost_stats(&state, &cfg.chat);
    }

    Ok(())
}

fn is_trivial_query(query: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return true;
    }
    matches!(
        q.as_str(),
        "hi" | "hello" | "hey" | "yo" | "sup" | "hola" | "good morning" | "good afternoon"
    )
}

fn is_quick_market_query(query: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return false;
    }
    q.contains("market today")
        || q.contains("what happened")
        || q.contains("what did you think")
        || q.contains("price of")
        || q.contains("stock price")
}

async fn cmd_finance(cmd: FinanceCommand) -> Result<()> {
    match cmd {
        FinanceCommand::Timeseries(args) => cmd_finance_timeseries(args).await,
        FinanceCommand::Snapshot(args) => cmd_finance_snapshot(args).await,
        FinanceCommand::Fundamentals(args) => cmd_finance_fundamentals(args).await,
        FinanceCommand::Search(args) => cmd_finance_search(args).await,
        FinanceCommand::Filings(args) | FinanceCommand::Sec(args) => {
            cmd_finance_filings(args).await
        }
        FinanceCommand::News(args) => cmd_finance_news(args).await,
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
