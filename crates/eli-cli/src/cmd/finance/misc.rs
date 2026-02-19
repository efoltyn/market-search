async fn cmd_finance_fundamentals(args: FinanceFundamentalsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let ticker_for_meta = args.ticker.clone();
    let req = eli_core::finance::FundamentalsRequest {
        ticker: args.ticker,
    };
    let resp = eli_core::finance::fetch_fundamentals(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch fundamentals")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.fundamentals",
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

async fn cmd_finance_search(args: FinanceSearchArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let query_for_meta = args.query.clone();
    let req = eli_core::finance::SearchRequest { query: args.query };
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

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
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

    let user_agent = config.and_then(|c| c.chat.sec_user_agent);

    let ticker_for_meta = args.ticker.clone();
    let req = eli_core::finance::FilingsRequest {
        ticker: args.ticker,
        forms: args.forms,
        limit: Some(args.limit),
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

    let json = serde_json::to_string_pretty(&resp).context("serialize response")?;
    println!("{json}");
    Ok(())
}

