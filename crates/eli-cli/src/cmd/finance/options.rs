fn build_options_ibkr_connection_config(
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

async fn cmd_finance_options(args: FinanceOptionsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    if args.summary && args.expirations {
        anyhow::bail!("use only one of --summary or --expirations");
    }
    if args.all && args.expirations {
        anyhow::bail!("use only one of --all or --expirations");
    }

    let option_type = match args
        .option_type
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
    {
        None => None,
        Some(t) if t == "both" || t.is_empty() => None,
        Some(t) if t == "calls" || t == "puts" => Some(t),
        Some(other) => anyhow::bail!("invalid --type '{other}' (expected calls|puts|both)"),
    };

    let ticker_for_meta = args.ticker.clone();
    let provider = match args.provider.trim().to_ascii_lowercase().as_str() {
        "yahoo" => eli_core::finance::ProviderKind::Yahoo,
        "ibkr" => eli_core::finance::ProviderKind::Ibkr,
        other => anyhow::bail!("unsupported --provider '{other}' (supported: yahoo, ibkr)"),
    };
    let use_ibkr = matches!(provider, eli_core::finance::ProviderKind::Ibkr);
    let req = eli_core::finance::OptionsRequest {
        ticker: args.ticker,
        provider,
        ibkr: use_ibkr.then(|| {
            build_options_ibkr_connection_config(
                args.ibkr_account.clone(),
                args.ibkr_host.clone(),
                args.ibkr_port,
                args.ibkr_client_id,
                args.ibkr_market_data_type,
            )
        }),
        expiry: args.expiry,
        target_dte_days: args.target_dte,
        option_type,
        near_money_pct: args.near_money,
        summary_only: args.summary,
        list_expirations: args.expirations,
        multi_expiry: args.all,
        num_expiries: if args.all { Some(100) } else { None },
    };

    let resp = eli_core::finance::fetch_options(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch options")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.options",
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

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_sync(args: FinanceSyncArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }
    if let Some(max_pages) = args.max_pages {
        eprintln!(
            "[sync] debug frontier sample enabled via --max-pages={max_pages}; use plain `eli finance sync` for exhaustive provider coverage"
        );
    }

    let sources = if args.sources.is_empty() {
        None
    } else {
        Some(args.sources)
    };

    let policy_mode = eli_core::finance::policy::parse_policy_mode(Some(&args.policy_mode))
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --policy-mode")?;
    let resolved_policy =
        eli_core::finance::policy::load_policy(args.policy_file.as_deref(), policy_mode)
            .map_err(|e| anyhow::anyhow!(e))
            .context("load policy")?;

    let req = eli_core::finance::OddsSyncRequest {
        sources,
        cache_dir: args.cache_dir.map(|p| p.to_string_lossy().to_string()),
        max_pages: args.max_pages,
        strict: args.strict,
        include_sports: args.include_sports,
        include_historical: args.include_historical,
        stream_refresh: args.stream_refresh,
        refresh_heartbeat_hours: args.refresh_heartbeat_hours,
        stream_refresh_timeout_secs: args.stream_refresh_timeout_secs,
    };

    let mut resp = eli_core::finance::sync_odds(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("sync prediction markets")?;
    resp.schema_version = "finance.sync.v2".to_string();
    resp.applied_policy = eli_core::finance::AppliedPolicy {
        mode: resolved_policy.mode,
        sources: resolved_policy.sources.clone(),
    };
    // decision_trace stays as run_sync set it; the CLI does not annotate the response
    // with mode/policy noise that already appears in the request or response shape.

    if let Some(out_path) = args.out {
        let mut meta_bits: Vec<String> = Vec::new();
        if let Some(max_pages) = args.max_pages {
            meta_bits.push(format!("sync_mode=frontier_sample"));
            meta_bits.push(format!("debug_max_pages={max_pages}"));
        } else {
            meta_bits.push("sync_mode=exhaustive".to_string());
        }
        let wr = write_json_out_with_meta(out_path, &resp, "finance.sync", &meta_bits)?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    if !args.full {
        compact_sync_stdout_payload(&mut resp, &resolved_policy.policy.stdout_compaction);
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

fn compact_sync_stdout_payload(
    resp: &mut eli_core::finance::OddsSyncResponse,
    policy: &eli_core::finance::policy::StdoutCompactionPolicy,
) {
    for source in &mut resp.sources {
        if let Some(analytics) = source.analytics.as_mut() {
            analytics.top_categories.truncate(policy.top_categories);
        }
        if let Some(delta) = source.delta.as_mut() {
            compact_delta_lists(
                &mut delta.top_probability_moves,
                &mut delta.top_yes_price_moves,
                &mut delta.top_volume_moves,
                policy,
            );
        }
    }

    if let Some(analysis) = resp.analysis.as_mut() {
        analysis.top_categories.truncate(policy.top_categories);
        analysis
            .top_markets_by_volume
            .truncate(policy.top_markets_by_volume);
        analysis
            .top_markets_by_informative_volume
            .truncate(policy.top_markets_by_informative_volume);
        analysis
            .anomalous_zero_yes_markets
            .truncate(policy.top_anomalous_zero_yes_markets);
        analysis
            .near_even_high_volume_markets
            .truncate(policy.top_near_even_high_volume_markets);
        analysis
            .high_confidence_high_volume_markets
            .truncate(policy.top_high_confidence_high_volume_markets);
    }

    if let Some(delta) = resp.delta.as_mut() {
        compact_delta_lists(
            &mut delta.top_probability_moves,
            &mut delta.top_yes_price_moves,
            &mut delta.top_volume_moves,
            policy,
        );
    }
}

fn compact_delta_lists(
    probability_moves: &mut Vec<eli_core::finance::OddsSyncMarketDelta>,
    yes_price_moves: &mut Vec<eli_core::finance::OddsSyncMarketDelta>,
    volume_moves: &mut Vec<eli_core::finance::OddsSyncMarketDelta>,
    policy: &eli_core::finance::policy::StdoutCompactionPolicy,
) {
    probability_moves.truncate(policy.top_probability_moves);
    yes_price_moves.truncate(policy.top_yes_price_moves);
    volume_moves.truncate(policy.top_volume_moves);
    for delta in probability_moves
        .iter_mut()
        .chain(yes_price_moves.iter_mut())
        .chain(volume_moves.iter_mut())
    {
        compact_market_delta(delta, policy);
    }
}

fn compact_market_delta(
    delta: &mut eli_core::finance::OddsSyncMarketDelta,
    policy: &eli_core::finance::policy::StdoutCompactionPolicy,
) {
    truncate_in_place(&mut delta.title, policy.max_title_chars);
    if let Some(category) = delta.category.as_mut() {
        truncate_in_place(category, policy.max_category_chars);
        if category.trim().is_empty() {
            delta.category = None;
        }
    }
    delta.previous_probability_yes = None;
    delta.previous_yes_price = None;
    delta.previous_volume = None;
    delta.previous_status = None;
}

fn truncate_in_place(value: &mut String, max_chars: usize) {
    if value.chars().count() <= max_chars {
        return;
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...");
    *value = out;
}
