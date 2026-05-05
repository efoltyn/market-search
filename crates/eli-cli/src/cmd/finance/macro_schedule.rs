/// Disk cache shared with timeseries: same ProjectDirs cache root, SHA-keyed,
/// atomic writes (tmp + rename) so concurrent MCP calls never read partial JSON.
fn cli_cache_path(prefix: &str, input: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let hash = format!("{:x}", h.finalize());
    let dir = directories::ProjectDirs::from("dev", "eli", "eli")
        .map(|d| d.cache_dir().join("finance").join(prefix))
        .unwrap_or_else(|| std::env::temp_dir().join("eli-cache").join("finance").join(prefix));
    dir.join(format!("{}.json", &hash[..16]))
}

fn cli_cache_read(path: &Path, ttl_secs: u64) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let age = meta.modified().ok()?.elapsed().ok()?;
    if age.as_secs() > ttl_secs {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn cli_cache_write(path: &Path, data: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic: write to temp file in same dir, then rename.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn schedule_fred_cache_mode() -> &'static str {
    if eli_core::finance::has_fred_api_attachment_hint() {
        "fred-on"
    } else {
        "fred-off"
    }
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

    let min_market_cap = args.min_cap.map(|s| parse_market_cap_threshold(&s)).transpose()?;
    let time_filter = args.time;
    const SCHEDULE_CACHE_SCHEMA_VERSION: &str = "v5-numeric-earnings-fields";

    // Cache: schedule data is static per day — 1 hour TTL.
    // Key must include ALL params that change the output.
    let fred_cache_mode = schedule_fred_cache_mode();
    let cache_input = format!(
        "{SCHEDULE_CACHE_SCHEMA_VERSION}|{fred_cache_mode}|{kind:?}|{start_date}|{end_date}|{macro_profile:?}|{min_market_cap:?}|{time_filter:?}|{:?}|{}",
        &args.ticker, args.major,
    );

    let req = eli_core::finance::ScheduleRequest {
        kind,
        start_date,
        end_date,
        tickers: args.ticker,
        major_only: args.major,
        macro_profile,
        min_market_cap,
        time_filter,
    };
    let cache_path = cli_cache_path("schedule", &cache_input);
    const SCHEDULE_TTL: u64 = 3600; // 1 hour

    let json = if let Some(cached) = cli_cache_read(&cache_path, SCHEDULE_TTL) {
        cached
    } else {
        let resp = eli_core::finance::fetch_schedule(req)
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .context("fetch schedule")?;
        let j = serde_json::to_string_pretty(&resp).context("serialize response")?;
        cli_cache_write(&cache_path, &j);
        j
    };

    if let Some(out_path) = args.out {
        let resp: eli_core::finance::ScheduleResponse =
            serde_json::from_str(&json).context("deserialize cached schedule")?;
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

    println!("{json}");
    Ok(())
}

fn parse_market_cap_threshold(s: &str) -> anyhow::Result<f64> {
    let s = s.trim().to_ascii_uppercase();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('T') {
        (n, 1e12)
    } else if let Some(n) = s.strip_suffix('B') {
        (n, 1e9)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1e6)
    } else if let Some(n) = s.strip_suffix('K') {
        (n, 1e3)
    } else {
        (s.as_str(), 1.0)
    };
    let num: f64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --min-cap value: {s}"))?;
    Ok(num * multiplier)
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

async fn cmd_finance_auctions(args: FinanceAuctionsArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let security_type = match args.security_type.trim().to_ascii_lowercase().as_str() {
        "all" | "" => None,
        other => Some(other.to_string()),
    };

    let req = eli_core::finance::AuctionsRequest {
        security_type,
        limit: Some(args.limit),
    };
    let resp = eli_core::finance::fetch_auctions(req)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("fetch auctions")?;

    if let Some(out_path) = args.out {
        let wr = write_json_out_with_meta(out_path, &resp, "finance.auctions", &[])?;
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

pub(crate) async fn cmd_finance_cot(args: FinanceCotArgs) -> Result<()> {
    if args.format.trim().to_ascii_lowercase() != "json" {
        anyhow::bail!("unsupported --format (only 'json' is implemented)");
    }

    let report = match args.report.trim().to_ascii_lowercase().as_str() {
        "auto" | "" => None, // let core auto-detect from query
        "disaggregated" | "disagg" | "commodities" => Some("disaggregated".to_string()),
        "financial" | "fin" | "tff" => Some("financial".to_string()),
        other => anyhow::bail!("invalid --report '{other}' (supported: auto, disaggregated, financial)"),
    };

    let req = eli_core::finance::CotRequest {
        query: args.query.clone(),
        weeks: Some(args.weeks),
        report,
        limit: args.limit,
    };

    // Cache: COT updates weekly (Fridays) — 6 hour TTL.
    // Key includes all params: query, weeks, report type.
    let cache_input = format!("{}|{}|{:?}|{:?}", args.query.as_deref().unwrap_or(""), args.weeks, req.report, args.limit);
    let cot_cache_path = cli_cache_path("cot", &cache_input);
    const COT_TTL: u64 = 6 * 3600; // 6 hours

    let json = if let Some(cached) = cli_cache_read(&cot_cache_path, COT_TTL) {
        cached
    } else {
        let resp = eli_core::finance::fetch_cot(req)
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .context("fetch cot")?;
        let j = serde_json::to_string_pretty(&resp).context("serialize response")?;
        cli_cache_write(&cot_cache_path, &j);
        j
    };

    if let Some(out_path) = args.out {
        let resp: eli_core::finance::CotResponse =
            serde_json::from_str(&json).context("deserialize cached cot")?;
        let wr = write_json_out_with_meta(out_path, &resp, "finance.cot", &[])?;
        println!(
            "{{\"ok\":true,\"path\":{},\"meta_path\":{}}}",
            serde_json::to_string(&wr.out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&wr.meta_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!("{json}");
    Ok(())
}
