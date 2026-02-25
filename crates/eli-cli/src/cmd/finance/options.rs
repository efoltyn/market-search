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

    if let Some(out_path) = args.out {
        let mut meta_bits: Vec<String> = Vec::new();
        if let Some(max_pages) = args.max_pages {
            meta_bits.push(format!("max_pages={max_pages}"));
        } else {
            meta_bits.push("max_pages=unbounded".to_string());
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
        compact_sync_stdout_payload(&mut resp);
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

fn compact_sync_stdout_payload(resp: &mut eli_core::finance::OddsSyncResponse) {
    for source in &mut resp.sources {
        if let Some(analytics) = source.analytics.as_mut() {
            analytics.top_categories.truncate(5);
        }
        if let Some(delta) = source.delta.as_mut() {
            compact_delta_lists(
                &mut delta.top_probability_moves,
                &mut delta.top_yes_price_moves,
                &mut delta.top_volume_moves,
            );
        }
    }

    if let Some(analysis) = resp.analysis.as_mut() {
        analysis.top_categories.truncate(8);
        analysis.top_markets_by_volume.truncate(3);
        analysis.top_markets_by_informative_volume.truncate(3);
        analysis.anomalous_zero_yes_markets.truncate(2);
        analysis.near_even_high_volume_markets.truncate(2);
        analysis.high_confidence_high_volume_markets.truncate(2);
    }

    if let Some(delta) = resp.delta.as_mut() {
        compact_delta_lists(
            &mut delta.top_probability_moves,
            &mut delta.top_yes_price_moves,
            &mut delta.top_volume_moves,
        );
    }
}

fn compact_delta_lists(
    probability_moves: &mut Vec<eli_core::finance::OddsSyncMarketDelta>,
    yes_price_moves: &mut Vec<eli_core::finance::OddsSyncMarketDelta>,
    volume_moves: &mut Vec<eli_core::finance::OddsSyncMarketDelta>,
) {
    probability_moves.truncate(3);
    yes_price_moves.truncate(3);
    volume_moves.truncate(3);
    for delta in probability_moves
        .iter_mut()
        .chain(yes_price_moves.iter_mut())
        .chain(volume_moves.iter_mut())
    {
        compact_market_delta(delta);
    }
}

fn compact_market_delta(delta: &mut eli_core::finance::OddsSyncMarketDelta) {
    truncate_in_place(&mut delta.title, 96);
    if let Some(category) = delta.category.as_mut() {
        truncate_in_place(category, 32);
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
