async fn cmd_finance_options(args: FinanceOptionsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    if args.summary && args.expirations {
        anyhow::bail!("use only one of --summary or --expirations");
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
    let req = eli_core::finance::OptionsRequest {
        ticker: args.ticker,
        expiry: args.expiry,
        option_type,
        near_money_pct: args.near_money,
        summary_only: args.summary,
        list_expirations: args.expirations,
        multi_expiry: false,
        num_expiries: None,
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

    let sources = if args.sources.is_empty() {
        None
    } else {
        Some(args.sources)
    };
    let kalshi_backfill_profile = match args
        .kalshi_backfill_profile
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "fast" => eli_core::finance::OddsSyncBackfillProfile::Fast,
        "balanced" => eli_core::finance::OddsSyncBackfillProfile::Balanced,
        "full" => eli_core::finance::OddsSyncBackfillProfile::Full,
        other => anyhow::bail!(
            "invalid --kalshi-backfill-profile '{other}' (expected: fast|balanced|full)"
        ),
    };

    let req = eli_core::finance::OddsSyncRequest {
        sources,
        cache_dir: args.cache_dir.map(|p| p.to_string_lossy().to_string()),
        max_pages: Some(args.max_pages),
        kalshi_backfill_profile,
        strict: args.strict,
    };

    let resp = eli_core::finance::sync_odds(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("sync prediction markets")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.sync",
            &[format!("max_pages={}", args.max_pages)],
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

async fn cmd_finance_news(args: FinanceNewsArgs) -> Result<()> {
    let ticker_for_meta = args.ticker.clone();
    let req = eli_core::finance::NewsRequest {
        ticker: args.ticker,
        date: args.date,
    };

    let resp = eli_core::finance::fetch_news(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.news",
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

    let json = serde_json::to_string_pretty(&resp)?;
    println!("{}", json);
    Ok(())
}
