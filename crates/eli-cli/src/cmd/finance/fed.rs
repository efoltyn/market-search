async fn cmd_finance_nyfed(args: FinanceNyfedArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let req = eli_core::finance::NyFedRequest {
        kind: args.kind.clone(),
    };
    let resp = eli_core::finance::fetch_nyfed(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch nyfed")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.nyfed", &[])?;
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

async fn cmd_finance_volsurface(args: FinanceVolsurfaceArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let symbols = args.symbols.map(|s| {
        s.split(',')
            .map(|t| t.trim().to_uppercase())
            .filter(|t| !t.is_empty())
            .collect::<Vec<String>>()
    });

    let req = eli_core::finance::VolSurfaceRequest {
        symbols,
        history: args.history,
    };
    let resp = eli_core::finance::fetch_volsurface(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch volatility")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.volatility", &[])?;
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

async fn cmd_finance_stress(args: FinanceStressArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let req = eli_core::finance::StressRequest {
        range_days: Some(args.range),
    };
    let resp = eli_core::finance::fetch_stress(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch stress")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.stress", &[])?;
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

async fn cmd_finance_fiscal(args: FinanceFiscalArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let req = eli_core::finance::FiscalRequest {
        kind: args.kind.clone(),
    };
    let resp = eli_core::finance::fetch_fiscal(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch fiscal")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.fiscal", &[])?;
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

async fn cmd_finance_ecb(args: FinanceEcbArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let preset = args
        .preset
        .as_deref()
        .and_then(eli_core::finance::EcbPreset::from_str);

    if preset.is_none() && (args.dataset.is_none() || args.key.is_none()) {
        anyhow::bail!(
            "ecb requires --preset (eurusd|fx_majors|estr|m3|euribor|yield_curve|balance_sheet) or --dataset + --key"
        );
    }

    let req = eli_core::finance::EcbRequest {
        preset,
        dataset: args.dataset,
        key: args.key,
        start_period: Some(args.start),
        end_period: args.end,
    };

    let resp = eli_core::finance::fetch_ecb(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch ecb")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.ecb", &[])?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize ecb response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_eia(args: FinanceEiaArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let preset = args
        .preset
        .as_deref()
        .and_then(eli_core::finance::EiaPreset::from_str);

    if preset.is_none() && args.route.is_none() {
        anyhow::bail!(
            "eia requires --preset (crude|gasoline|distillate|all|nat_gas|ng_prices|electricity|nuclear|steo|energy) or --route"
        );
    }

    let api_key = eli_core::finance::resolve_eia_api_key()
        .map_err(|e| anyhow::anyhow!(e))?;

    let req = eli_core::finance::EiaRequest {
        api_key,
        preset,
        route: args.route,
        facets: Vec::new(),
        start: args.start,
        length: Some(args.length),
    };

    let resp = eli_core::finance::fetch_eia(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch eia")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.eia", &[])?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&resp).context("serialize eia response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_bis(args: FinanceBisArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }
    let preset = args.preset.as_deref().and_then(eli_core::finance::BisPreset::from_str);
    if preset.is_none() && (args.dataset.is_none() || args.key.is_none()) {
        anyhow::bail!("bis requires --preset (policy_rates|assets|credit_gap|property|eer) or --dataset + --key");
    }
    let countries: Vec<String> = args.countries
        .as_deref()
        .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
        .unwrap_or_default();
    let req = eli_core::finance::BisRequest {
        preset,
        dataset: args.dataset,
        key: args.key,
        countries,
        start_period: Some(args.start),
    };
    let resp = eli_core::finance::fetch_bis(req).await.map_err(|e| anyhow::anyhow!(e)).context("fetch bis")?;
    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.bis", &[])?;
        println!("{{\"ok\":true,\"path\":{},\"meta_path\":{}}}", serde_json::to_string(&wr.out_path.display().to_string()).unwrap_or_else(|_| "\"\"".to_string()), serde_json::to_string(&wr.meta_path.display().to_string()).unwrap_or_else(|_| "\"\"".to_string()));
        return Ok(());
    }
    let json = serde_json::to_string_pretty(&resp).context("serialize bis response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_boj(args: FinanceBojArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }
    let preset = args.preset.as_deref().and_then(eli_core::finance::BojPreset::from_str);
    if preset.is_none() && args.db.is_none() {
        anyhow::bail!("boj requires --preset (policy_rate|call_rate|monetary_base|balance_sheet|money_stock|tankan|fx) or --db + --codes");
    }
    let codes: Vec<String> = args.codes
        .as_deref()
        .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
        .unwrap_or_default();
    let req = eli_core::finance::BojRequest {
        preset,
        db: args.db,
        codes,
        start_date: Some(args.start),
    };
    let resp = eli_core::finance::fetch_boj(req).await.map_err(|e| anyhow::anyhow!(e)).context("fetch boj")?;
    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.boj", &[])?;
        println!("{{\"ok\":true,\"path\":{},\"meta_path\":{}}}", serde_json::to_string(&wr.out_path.display().to_string()).unwrap_or_else(|_| "\"\"".to_string()), serde_json::to_string(&wr.meta_path.display().to_string()).unwrap_or_else(|_| "\"\"".to_string()));
        return Ok(());
    }
    let json = serde_json::to_string_pretty(&resp).context("serialize boj response")?;
    println!("{json}");
    Ok(())
}

async fn cmd_finance_boe(args: FinanceBoeArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }
    let preset = args.preset.as_deref().and_then(eli_core::finance::BoePreset::from_str);
    if preset.is_none() && args.codes.is_none() {
        anyhow::bail!("boe requires --preset (bank_rate|sonia|gilts|m4|fx|all) or --codes");
    }
    let series_codes: Vec<String> = args.codes
        .as_deref()
        .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
        .unwrap_or_default();
    let req = eli_core::finance::BoeRequest {
        preset,
        series_codes,
        start: Some(args.start),
        end: Some(args.end),
    };
    let resp = eli_core::finance::fetch_boe(req).await.map_err(|e| anyhow::anyhow!(e)).context("fetch boe")?;
    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.boe", &[])?;
        println!("{{\"ok\":true,\"path\":{},\"meta_path\":{}}}", serde_json::to_string(&wr.out_path.display().to_string()).unwrap_or_else(|_| "\"\"".to_string()), serde_json::to_string(&wr.meta_path.display().to_string()).unwrap_or_else(|_| "\"\"".to_string()));
        return Ok(());
    }
    let json = serde_json::to_string_pretty(&resp).context("serialize boe response")?;
    println!("{json}");
    Ok(())
}
