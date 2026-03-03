fn sentinel_paths_tuple(args: &SentinelPathArgs) -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
    (
        args.sentinel_dir.clone(),
        args.queue_file.clone(),
        args.packets_file.clone(),
    )
}

fn sentinel_severity_from_arg(arg: SentinelSeverityArg) -> eli_core::sentinel::Severity {
    match arg {
        SentinelSeverityArg::Low => eli_core::sentinel::Severity::Low,
        SentinelSeverityArg::Medium => eli_core::sentinel::Severity::Medium,
        SentinelSeverityArg::High => eli_core::sentinel::Severity::High,
        SentinelSeverityArg::Critical => eli_core::sentinel::Severity::Critical,
    }
}

fn parse_vars(raw: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut vars = std::collections::BTreeMap::new();
    for item in raw {
        let trimmed = item.trim();
        let Some((name, spec)) = trimmed.split_once('=') else {
            anyhow::bail!("invalid --var '{trimmed}' (expected name=provider:query)");
        };
        let name = name.trim();
        let spec = spec.trim();
        if name.is_empty() || spec.is_empty() {
            anyhow::bail!("invalid --var '{trimmed}' (name/spec must be non-empty)");
        }
        vars.insert(name.to_string(), spec.to_string());
    }
    Ok(vars)
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn cmd_sentinel(cmd: SentinelCommand) -> Result<()> {
    match cmd {
        SentinelCommand::Start(args) => cmd_sentinel_start(args).await,
        SentinelCommand::Stop(args) => cmd_sentinel_stop(args).await,
        SentinelCommand::Status(args) => cmd_sentinel_status(args),
        SentinelCommand::Subscribe(args) => cmd_sentinel_subscribe(args),
        SentinelCommand::Unsubscribe(args) => cmd_sentinel_unsubscribe(args),
        SentinelCommand::List(args) => cmd_sentinel_list(args),
        SentinelCommand::Test(args) => cmd_sentinel_test(args),
        SentinelCommand::Replay(args) => cmd_sentinel_replay(args),
        SentinelCommand::DaemonRun(args) => cmd_sentinel_daemon_run(args).await,
    }
}

async fn cmd_sentinel_start(args: SentinelStartArgs) -> Result<()> {
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    let paths = eli_core::sentinel::resolve_paths(sentinel_dir, queue_file, packets_file)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve sentinel paths")?;
    paths.ensure_dirs().map_err(|e| anyhow::anyhow!(e))?;

    if let Some(pid) = eli_core::sentinel::io::read_pid(&paths).map_err(|e| anyhow::anyhow!(e))? {
        if process_alive(pid) {
            let out = serde_json::json!({
                "ok": true,
                "status": "already_running",
                "pid": pid,
                "sentinel_dir": paths.root_dir.display().to_string(),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
            return Ok(());
        }
    }

    eli_core::sentinel::io::clear_stop_request(&paths).map_err(|e| anyhow::anyhow!(e))?;
    let exe = std::env::current_exe().context("resolve current executable")?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("sentinel")
        .arg("daemon-run")
        .arg("--interval-secs")
        .arg(args.interval_secs.max(1).to_string())
        .stdin(std::process::Stdio::null());

    command.arg("--sentinel-dir").arg(paths.root_dir.as_os_str());
    command.arg("--queue-file").arg(paths.queue_file.as_os_str());
    command.arg("--packets-file").arg(paths.packets_file.as_os_str());

    let log = eli_core::sentinel::io::open_log(&paths).map_err(|e| anyhow::anyhow!(e))?;
    let log_err = log.try_clone().context("clone sentinel log fd")?;
    command.stdout(std::process::Stdio::from(log));
    command.stderr(std::process::Stdio::from(log_err));

    let child = command.spawn().context("spawn sentinel daemon process")?;
    eli_core::sentinel::io::write_pid(&paths, child.id()).map_err(|e| anyhow::anyhow!(e))?;

    let out = serde_json::json!({
        "ok": true,
        "status": "started",
        "pid": child.id(),
        "sentinel_dir": paths.root_dir.display().to_string(),
        "queue_file": paths.queue_file.display().to_string(),
        "packets_file": paths.packets_file.display().to_string(),
        "log_file": paths.log_file.display().to_string(),
        "interval_secs": args.interval_secs.max(1),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn cmd_sentinel_stop(args: SentinelStopArgs) -> Result<()> {
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    let paths = eli_core::sentinel::resolve_paths(sentinel_dir, queue_file, packets_file)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve sentinel paths")?;
    paths.ensure_dirs().map_err(|e| anyhow::anyhow!(e))?;
    eli_core::sentinel::io::write_stop_request(&paths).map_err(|e| anyhow::anyhow!(e))?;

    let pid = eli_core::sentinel::io::read_pid(&paths).map_err(|e| anyhow::anyhow!(e))?;
    let mut kill_status = None;
    if let Some(pid) = pid {
        if process_alive(pid) {
            let status = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            kill_status = status.ok().map(|s| s.success());
        } else {
            kill_status = Some(false);
        }
    }
    let out = serde_json::json!({
        "ok": true,
        "status": "stop_requested",
        "pid": pid,
        "kill_sent": kill_status.unwrap_or(false),
        "sentinel_dir": paths.root_dir.display().to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn cmd_sentinel_status(args: SentinelStatusArgs) -> Result<()> {
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    let paths = eli_core::sentinel::resolve_paths(sentinel_dir, queue_file, packets_file)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve sentinel paths")?;
    let mut state =
        eli_core::sentinel::io::load_daemon_state(&paths).map_err(|e| anyhow::anyhow!(e))?;
    let pid = eli_core::sentinel::io::read_pid(&paths).map_err(|e| anyhow::anyhow!(e))?;
    let alive = pid.map(process_alive).unwrap_or(false);
    if pid.is_some() && !alive {
        let _ = eli_core::sentinel::io::clear_pid(&paths);
        state.pid = None;
        let _ = eli_core::sentinel::io::save_daemon_state(&paths, &state);
    }
    let out = serde_json::json!({
        "ok": true,
        "running": alive,
        "pid": if alive { pid } else { None },
        "state": state,
        "sentinel_dir": paths.root_dir.display().to_string(),
        "queue_file": paths.queue_file.display().to_string(),
        "packets_file": paths.packets_file.display().to_string(),
        "error_packets_file": paths.error_packets_file.display().to_string(),
        "subscriptions_file": paths.subscriptions_file.display().to_string(),
        "log_file": paths.log_file.display().to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn cmd_sentinel_subscribe(args: SentinelSubscribeArgs) -> Result<()> {
    let vars = parse_vars(&args.vars)?;
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    let spec = eli_core::sentinel::subscriptions::add_subscription(
        sentinel_dir,
        queue_file,
        packets_file,
        eli_core::sentinel::subscriptions::AddSubscriptionInput {
            name: args.name,
            expr: args.expr,
            vars,
            source_set: Vec::new(),
            cooldown_secs: Some(args.cooldown_secs.max(1)),
            severity: Some(sentinel_severity_from_arg(args.severity)),
            why_template: args.why,
            prompt_template: args.prompt_template,
            enabled: Some(args.enabled),
        },
    )
    .map_err(|e| anyhow::anyhow!(e))
    .context("add sentinel subscription")?;

    let out = serde_json::json!({
        "ok": true,
        "subscription": spec,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn cmd_sentinel_unsubscribe(args: SentinelUnsubscribeArgs) -> Result<()> {
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    let removed = eli_core::sentinel::subscriptions::remove_subscription(
        sentinel_dir,
        queue_file,
        packets_file,
        &args.id_or_name,
    )
    .map_err(|e| anyhow::anyhow!(e))
    .context("remove sentinel subscription")?;
    let out = serde_json::json!({
        "ok": true,
        "removed": removed,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn cmd_sentinel_list(args: SentinelListArgs) -> Result<()> {
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    let registry = eli_core::sentinel::subscriptions::list_subscriptions(
        sentinel_dir,
        queue_file,
        packets_file,
    )
    .map_err(|e| anyhow::anyhow!(e))
    .context("list sentinel subscriptions")?;
    let out = serde_json::json!({
        "ok": true,
        "count": registry.subscriptions.len(),
        "subscriptions": registry.subscriptions,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn cmd_sentinel_test(args: SentinelTestArgs) -> Result<()> {
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    let paths = eli_core::sentinel::resolve_paths(sentinel_dir, queue_file, packets_file)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve sentinel paths")?;
    paths.ensure_dirs().map_err(|e| anyhow::anyhow!(e))?;

    let (expr, vars, why) = match args.scenario.trim().to_ascii_lowercase().as_str() {
        "oil_spike" => (
            "pyth_wti > 80 && poly_hormuz_yes > 0.50".to_string(),
            std::collections::BTreeMap::from([
                ("pyth_wti".to_string(), 81.25),
                ("poly_hormuz_yes".to_string(), 0.57),
            ]),
            "Oil shock risk and Strait of Hormuz escalation probability rose.".to_string(),
        ),
        _ => (
            "sentinel_test_value > 0.5".to_string(),
            std::collections::BTreeMap::from([("sentinel_test_value".to_string(), 1.0)]),
            "Synthetic sentinel wiring test packet.".to_string(),
        ),
    };

    let eval = eli_core::sentinel::evaluator::Evaluation {
        triggered: true,
        observed_vars: vars.clone(),
        observations: std::collections::BTreeMap::from([(
            vars.keys().next().cloned().unwrap_or_else(|| "sentinel_test_value".to_string()),
            eli_core::sentinel::evaluator::VariableObservation {
                value: *vars.values().next().unwrap_or(&1.0),
                source: "test".to_string(),
                instrument: args.scenario.clone(),
                endpoint: "sentinel_test".to_string(),
                symbol_or_id: args.scenario.clone(),
            },
        )]),
    };
    let sub = eli_core::sentinel::SubscriptionSpec {
        id: "test_subscription".to_string(),
        name: format!("test-{}", args.scenario),
        expr,
        vars: std::collections::BTreeMap::new(),
        source_set: Vec::new(),
        cooldown_secs: 1,
        severity: eli_core::sentinel::Severity::High,
        why_template: why,
        prompt_template:
            "SYSTEM OVERRIDE: Sentinel test fired. Re-run macro risk pack and update projector."
                .to_string(),
        enabled: true,
        last_triggered_at: None,
    };
    let packet = eli_core::sentinel::packets::build_alert_packet(&paths, &sub, &eval, 0)
        .map_err(|e| anyhow::anyhow!(e))
        .context("build test packet")?;
    eli_core::sentinel::io::append_alert_packet(&paths, &packet).map_err(|e| anyhow::anyhow!(e))?;
    let out = serde_json::json!({
        "ok": true,
        "packet_id": packet.packet_id,
        "queue_file": paths.queue_file.display().to_string(),
        "packets_file": paths.packets_file.display().to_string(),
        "playbook_path": packet.playbook_path,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn cmd_sentinel_replay(args: SentinelReplayArgs) -> Result<()> {
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    let paths = eli_core::sentinel::resolve_paths(sentinel_dir, queue_file, packets_file)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve sentinel paths")?;
    if !paths.queue_file.exists() {
        let out = serde_json::json!({
            "ok": true,
            "count": 0,
            "lines": [],
            "queue_file": paths.queue_file.display().to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let raw = std::fs::read_to_string(&paths.queue_file).context("read queue file")?;
    let mut lines: Vec<&str> = raw.lines().collect();
    if lines.len() > args.max_lines {
        lines = lines.split_off(lines.len() - args.max_lines);
    }
    let json_lines: Vec<serde_json::Value> = lines
        .into_iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect();
    let out = serde_json::json!({
        "ok": true,
        "count": json_lines.len(),
        "lines": json_lines,
        "queue_file": paths.queue_file.display().to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn cmd_sentinel_daemon_run(args: SentinelDaemonRunArgs) -> Result<()> {
    let (sentinel_dir, queue_file, packets_file) = sentinel_paths_tuple(&args.paths);
    eli_core::sentinel::daemon::run_daemon(eli_core::sentinel::daemon::DaemonOptions {
        sentinel_dir,
        queue_file,
        packets_file,
        interval_secs: args.interval_secs.max(1),
    })
    .await
    .map_err(|e| anyhow::anyhow!(e))
    .context("run sentinel daemon")
}
