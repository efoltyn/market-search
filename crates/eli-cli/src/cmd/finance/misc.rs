fn build_search_ibkr_connection_config(
    account: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    client_id: Option<i32>,
    market_data_type: Option<i32>,
) -> eli_core::finance::IbkrConnectionConfig {
    eli_core::finance::IbkrConnectionConfig {
        account,
        host,
        port,
        client_id,
        market_data_type,
        timeout_secs: None,
    }
}

async fn cmd_finance_fundamentals(args: FinanceFundamentalsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let tickers: Vec<String> = args
        .tickers
        .iter()
        .map(|t| t.trim().to_ascii_uppercase())
        .filter(|t| !t.is_empty())
        .collect();

    if tickers.is_empty() {
        anyhow::bail!(
            "at least one ticker is required (e.g. --tickers NVDA or --tickers NVDA,AAPL)"
        );
    }

    let futs = tickers.iter().map(|ticker| {
        let req = eli_core::finance::FundamentalsRequest {
            ticker: ticker.clone(),
        };
        eli_core::finance::fetch_fundamentals(req)
    });

    let results = futures::future::join_all(futs).await;
    let resps: Vec<_> = results
        .into_iter()
        .map(|r| r.map_err(|e| anyhow::anyhow!(e)))
        .collect::<Result<Vec<_>>>()?;

    // Always emit an array — single-ticker callers see a 1-element array,
    // multi-ticker callers see N-element. Downstream `jq` consumers can
    // assume `.[]` works regardless of arity.
    if let Some(out_path) = args.out {
        let tickers_str = tickers.join(",");
        let wr = write_json_out_with_meta(
            out_path,
            &resps,
            "finance.fundamentals",
            &[format!("tickers={tickers_str}")],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }
    emit_tool_payload(&resps)?;

    Ok(())
}

async fn cmd_finance_search(args: FinanceSearchArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    // Resolve query from --query flag or positional arg.
    let resolved_query = args
        .query
        .or(args.query_positional)
        .ok_or_else(|| anyhow::anyhow!("search query required (use --query or positional arg)"))?;
    let query_for_meta = resolved_query.clone();
    let policy_mode = eli_core::finance::policy::parse_policy_mode(Some(&args.policy_mode))
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --policy-mode")?;
    let provider = match args.provider.trim().to_ascii_lowercase().as_str() {
        "auto" | "" => eli_core::finance::ProviderKind::Yahoo,
        "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "ibkr" => eli_core::finance::ProviderKind::Ibkr,
        other => anyhow::bail!("unsupported --provider '{other}' (supported: auto, yahoo, ibkr)"),
    };
    let use_ibkr = matches!(provider, eli_core::finance::ProviderKind::Ibkr);
    let req = eli_core::finance::SearchRequest {
        query: resolved_query,
        provider,
        ibkr: use_ibkr.then(|| {
            build_search_ibkr_connection_config(
                args.ibkr_account.clone(),
                args.ibkr_host.clone(),
                args.ibkr_port,
                args.ibkr_client_id,
                args.ibkr_market_data_type,
            )
        }),
        policy_file: args
            .policy_file
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        policy_mode: Some(policy_mode),
    };
    let resp = eli_core::finance::fetch_search(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch search")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.search",
            &[format!("query={query_for_meta}")],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    emit_tool_payload(&resp)?;
    Ok(())
}

async fn cmd_finance_filings(args: FinanceFilingsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let cache_dir = if let Some(path) = args.cache_dir {
        path
    } else {
        let paths = Paths::discover().context("discover paths")?;
        paths.ensure_dirs().context("ensure dirs")?;
        paths.cache_dir
    };

    let paths = Paths::discover().ok();
    let config = if let Some(p) = paths {
        config::load_or_default(&p).ok()
    } else {
        None
    };

    let user_agent = args.user_agent.or_else(|| config.and_then(|c| c.chat.sec_user_agent));

    // Cross-filer full-text search mode: no ticker needed, answers
    // "which companies filed documents mentioning X" market-wide.
    if let Some(query) = args.search_text.as_deref() {
        if args.ticker.is_some() {
            anyhow::bail!(
                "--ticker and --search-text are mutually exclusive: --search-text searches ALL filers (drop --ticker, or put the company name in the query)"
            );
        }
        // Same default form scope as ticker mode; pass --forms any to search
        // every form type.
        let effective_forms: Vec<String> = if args.forms.iter().all(|f| f.trim().is_empty()) {
            vec!["8-K".to_string(), "10-K".to_string(), "10-Q".to_string()]
        } else if args
            .forms
            .iter()
            .any(|f| matches!(f.trim().to_ascii_lowercase().as_str(), "any" | "all"))
        {
            Vec::new()
        } else {
            args.forms.clone()
        };
        let resp = eli_core::finance::search_filings_fulltext(
            query,
            &effective_forms,
            args.from.as_deref(),
            args.to.as_deref(),
            args.limit,
            user_agent.as_deref(),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("edgar full-text search")?;
        if let Some(out_path) = args.out {
            let wr = write_json_out_with_meta(
                out_path,
                &resp,
                "finance.filings.search",
                &[format!("query={query}")],
            )?;
            println!(
                "{{\"ok\":true,\"path\":{}}}",
                serde_json::to_string(&wr.out_path.display().to_string())
                    .unwrap_or_else(|_| "\"\"".to_string()),
            );
            return Ok(());
        }
        let mut payload = resp;
        round_json_floats(&mut payload);
        println!("{}", json_to_stdout_string(&payload)?);
        return Ok(());
    }

    let Some(ticker) = args.ticker else {
        anyhow::bail!("--ticker is required (or use --search-text for cross-filer search)");
    };
    let ticker_for_meta = ticker.clone();
    let req = eli_core::finance::FilingsRequest {
        ticker,
        forms: args.forms,
        limit: Some(if args.single_file { 1 } else { args.limit }),
        download: !args.no_download
            || args.include_text
            || args.download_all
            || args.single_file
            || args.press_release_text,
        download_all: args.download_all,
        important_exhibits: !args.primary_only && !args.single_file,
        single_file: args.single_file,
        raw_text: args.raw_text,
        press_release_text: args.press_release_text,
        include_text: args.include_text,
        max_chars: args.max_chars,
        user_agent,
    };

    let resp = eli_core::finance::fetch_filings(req, &cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch filings")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.filings",
            &[format!("ticker={ticker_for_meta}")],
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    emit_tool_payload(&resp)?;
    Ok(())
}

async fn cmd_finance_insider(args: FinanceInsiderArgs) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    paths.ensure_dirs().context("ensure dirs")?;
    let config = config::load_or_default(&paths).ok();
    let user_agent = args
        .user_agent
        .or_else(|| config.and_then(|c| c.chat.sec_user_agent));

    let req = eli_core::finance::InsiderRequest {
        ticker: args.ticker,
        days: args.days,
        limit: args.limit,
        summary_only: args.summary_only,
        user_agent,
    };
    let resp = eli_core::finance::fetch_insider(req, &paths.cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch insider transactions")?;
    let mut payload = serde_json::to_value(&resp).context("serialize response")?;
    round_json_floats(&mut payload);
    println!("{}", json_to_stdout_string(&payload)?);
    Ok(())
}

async fn cmd_finance_short(args: FinanceShortArgs) -> Result<()> {
    let resp = eli_core::finance::fetch_short_data(&args.ticker)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch finra short data")?;
    let mut payload = resp;
    round_json_floats(&mut payload);
    println!("{}", json_to_stdout_string(&payload)?);
    Ok(())
}
