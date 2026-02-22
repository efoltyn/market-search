async fn cmd_finance_macro(args: FinanceMacroArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let range = if args.range.is_empty() {
        None
    } else {
        match eli_core::finance::Span::parse(&args.range) {
            Ok(s) => Some(s),
            Err(e) => anyhow::bail!("invalid --range '{}': {}", args.range, e),
        }
    };

    let compare_to = if let Some(raw) = args.compare_to.as_deref() {
        Some(
            chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("invalid --compare-to '{raw}' (expected YYYY-MM-DD)"))?,
        )
    } else {
        None
    };

    let req = eli_core::finance::MacroRequest { range, compare_to };
    let resp = eli_core::finance::fetch_macro(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch macro")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.macro",
            &[format!("range={}", args.range)],
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

async fn cmd_finance_forex(args: FinanceForexArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let range = eli_core::finance::Span::parse(&args.range)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --range")?;
    let granularity = eli_core::finance::Span::parse(&args.granularity)
        .map_err(|e| anyhow::anyhow!(e))
        .context("parse --granularity")?;
    let as_of = if let Some(raw) = args.as_of.as_deref() {
        Some(
            eli_core::finance::parse_as_of(raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --as-of")?,
        )
    } else {
        None
    };
    let event_at = if let Some(raw) = args.event_at.as_deref() {
        Some(
            eli_core::finance::parse_event_at(raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --event-at")?,
        )
    } else {
        None
    };
    let event_window = if let Some(raw) = args.event_window.as_deref() {
        Some(
            eli_core::finance::Span::parse(raw)
                .map_err(|e| anyhow::anyhow!(e))
                .context("parse --event-window")?,
        )
    } else {
        None
    };
    let mut compare_as_of = Vec::new();
    for raw in &args.compare_as_of {
        let dt = eli_core::finance::parse_as_of(raw)
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| format!("parse --compare-as-of value '{raw}'"))?;
        compare_as_of.push(dt);
    }
    let mut horizons = Vec::new();
    for raw in &args.horizons {
        let span = eli_core::finance::Span::parse(raw)
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| format!("parse --horizons value '{raw}'"))?;
        horizons.push(span);
    }

    let cache_dir = if let Some(path) = args.cache_dir {
        path
    } else {
        let paths = Paths::discover().context("discover paths")?;
        paths.ensure_dirs().context("ensure dirs")?;
        paths.cache_dir
    };

    let req = eli_core::finance::ForexRequest {
        pairs: args.pairs.clone(),
        currencies: args.currencies.clone(),
        countries: args.countries.clone(),
        groups: args.groups.clone(),
        include_em: args.include_em,
        range,
        granularity,
        as_of,
        event_at,
        event_window,
        compare_as_of,
        horizons,
        max_pairs: args.max_pairs,
        recent_points: Some(args.recent_points),
        top: Some(args.top),
    };
    let resp = eli_core::finance::fetch_forex(req, &cache_dir)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch forex")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.forex",
            &[
                format!("range={}", args.range),
                format!("granularity={}", args.granularity),
                format!("pairs={}", args.pairs.len()),
                format!("currencies={}", args.currencies.len()),
                format!("countries={}", args.countries.len()),
                format!("groups={}", args.groups.len()),
                format!("event_at={}", args.event_at.is_some()),
                format!("event_window={}", args.event_window.as_deref().unwrap_or("")),
                format!("compare_as_of={}", args.compare_as_of.len()),
            ],
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

async fn cmd_finance_schedule(args: FinanceScheduleArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let kind = match args.kind.trim().to_ascii_lowercase().as_str() {
        "earnings" => eli_core::finance::ScheduleKind::Earnings,
        "macro" => eli_core::finance::ScheduleKind::Macro,
        "all" => eli_core::finance::ScheduleKind::All,
        other => anyhow::bail!("unsupported --kind '{other}' (supported: earnings, macro, all)"),
    };
    let macro_profile = match args.macro_profile.trim().to_ascii_lowercase().as_str() {
        "broad" => eli_core::finance::ScheduleMacroProfile::Broad,
        "market" => eli_core::finance::ScheduleMacroProfile::Market,
        "major" => eli_core::finance::ScheduleMacroProfile::Major,
        other => anyhow::bail!(
            "unsupported --macro-profile '{other}' (supported: broad, market, major)"
        ),
    };

    let (start_date, end_date) = if let Some(date) = args.date {
        if args.from.is_some() || args.to.is_some() {
            anyhow::bail!("use either --date or --from/--to");
        }
        (date.clone(), date)
    } else {
        let start = args
            .from
            .ok_or_else(|| anyhow::anyhow!("missing --date or --from"))?;
        let end = args.to.unwrap_or_else(|| start.clone());
        (start, end)
    };

    let req = eli_core::finance::ScheduleRequest {
        kind,
        start_date,
        end_date,
        tickers: args.ticker,
        major_only: args.major,
        macro_profile,
    };

    let resp = eli_core::finance::fetch_schedule(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch schedule")?;
    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.schedule",
            &[format!("kind={}", args.kind)],
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

async fn cmd_finance_rate_path(args: FinanceRatePathArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let source_mode = match args.source_mode.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(eli_core::finance::RatePathSourceMode::Auto),
        "meeting" => Some(eli_core::finance::RatePathSourceMode::Meeting),
        "fallback" => Some(eli_core::finance::RatePathSourceMode::Fallback),
        other => anyhow::bail!(
            "unsupported --source-mode '{other}' (supported: auto, meeting, fallback)"
        ),
    };
    let req = eli_core::finance::RatePathRequest {
        cache_dir: args.cache_dir.map(|p| p.to_string_lossy().to_string()),
        source_mode,
    };
    let resp = eli_core::finance::fetch_rate_path(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch rate path")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.rate_path", &[])?;
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

async fn cmd_finance_yield_curve(args: FinanceYieldCurveArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let mut compare_3mo = false;
    let mut compare_1y = false;
    for item in &args.compare {
        match item.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "3mo" => compare_3mo = true,
            "1y" => compare_1y = true,
            other => anyhow::bail!("invalid --compare value '{other}' (supported: 3mo,1y)"),
        }
    }

    let req = eli_core::finance::YieldCurveRequest {
        compare_3mo,
        compare_1y,
        strict: args.strict,
    };
    let resp = eli_core::finance::fetch_yield_curve(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch yield curve")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.yield_curve", &[])?;
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

async fn cmd_finance_dashboard(args: FinanceDashboardArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let req = eli_core::finance::DashboardRequest {
        preset: args.preset.clone(),
        max_ms: args.max_ms,
    };
    let resp = eli_core::finance::fetch_dashboard(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch dashboard")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(
            out_path,
            &resp,
            "finance.dashboard",
            &[format!("preset={}", args.preset)],
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

async fn cmd_finance_prices(args: FinancePricesArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let ids_for_meta = args.ids.clone();
    let req = eli_core::finance::PricesRequest {
        query: args.query,
        asset_type: args.asset_type,
        ids: args.ids,
        auto_select: args.auto_select,
    };

    let resp = eli_core::finance::fetch_prices(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch prices")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.prices", &ids_for_meta)?;
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
