// `eli mcp share` — boot local HTTP MCP + spawn a public-URL tunnel.
//
// Three free providers:
//   tunnelmole  → temporary URL via `npx tunnelmole`        (default)
//   cloudflare  → temporary URL via `cloudflared`
//   ngrok       → permanent URL via `ngrok` (account required)
//
// Plus a placeholder for `self-host` (sovereign) mode that points at
// SELFHOST.md until the Rust gateway/laptop-ACME stack ships.

const SHARE_PID_FILE: &str = "/tmp/eli-share-children.pid";

async fn cmd_mcp_share(args: ShareArgs) -> Result<()> {
    let provider = args.provider.to_ascii_lowercase();
    let port = args.port;

    eprintln!("[eli mcp share] provider={} port={}", provider, port);

    // Reap any tunnel children orphaned by a previous SIGTERM/SIGKILL run.
    cleanup_orphan_children();

    // Ensure local HTTP MCP is reachable on `port`. If not, boot one.
    let _local_handle = ensure_local_mcp(port).await?;

    match provider.as_str() {
        "tunnelmole" | "tm" => provider_tunnelmole(port).await,
        "cloudflare" | "cloudflared" | "cf" => provider_cloudflare(port).await,
        "ngrok" => provider_ngrok(args).await,
        "self-host" | "selfhost" | "self_host" | "sovereign" => provider_selfhost(),
        other => anyhow::bail!(
            "unknown provider '{}'. Pick one of: tunnelmole, cloudflare, ngrok, self-host",
            other
        ),
    }
}

// ── local HTTP MCP bootstrap ────────────────────────────────────────────────

/// Returns a handle that, when dropped, signals the spawned HTTP MCP task
/// to shut down. If the port is already serving our MCP, returns None and
/// reuses the existing process.
async fn ensure_local_mcp(port: u16) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let url = format!("http://127.0.0.1:{}/mcp", port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .context("build probe http client")?;

    if let Ok(resp) = client.get(&url).send().await {
        if let Ok(body) = resp.text().await {
            if body.contains("eli-mcp") || body.contains("streamable-http") {
                eprintln!("[eli mcp share] reusing existing local MCP on :{}", port);
                return Ok(None);
            }
        }
        anyhow::bail!(
            "port {} is in use but doesn't look like an eli MCP server. \
             Pick a different --port or stop whatever is on that port.",
            port
        );
    }

    eprintln!("[eli mcp share] booting local HTTP MCP on :{}", port);
    let handle = tokio::spawn(async move {
        if let Err(e) = cmd_mcp_http(port).await {
            eprintln!("[eli mcp share] local MCP exited: {e}");
        }
    });

    // Wait for the local server to become reachable.
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                return Ok(Some(handle));
            }
        }
    }
    anyhow::bail!("local MCP didn't come up on :{} within 6 seconds", port);
}

// ── tunnel providers ────────────────────────────────────────────────────────

async fn provider_tunnelmole(port: u16) -> Result<()> {
    if which("npx").is_none() {
        anyhow::bail!(
            "tunnelmole needs `npx` (Node.js). Install Node from https://nodejs.org \
             or pick a different provider:\n  \
             eli mcp share --provider cloudflare    # temp, no Node needed\n  \
             eli mcp share --provider ngrok --domain <your>.ngrok-free.dev"
        );
    }

    let mut child = tokio::process::Command::new("npx")
        .args(["-y", "tunnelmole", &port.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn npx tunnelmole")?;
    record_child_pid(&child);

    let pattern =
        regex::Regex::new(r"https://[A-Za-z0-9.-]+\.tunnelmole\.net").expect("valid regex");
    let url = read_url_from_child(&mut child, &pattern, 60).await?;

    print_share_block("tunnelmole (temporary)", &url, "URL stays up while this process runs. Ctrl-C to stop.");
    wait_for_signal_or_exit(child).await
}

async fn provider_cloudflare(port: u16) -> Result<()> {
    if which("cloudflared").is_none() {
        anyhow::bail!(
            "cloudflared not found. Install it:\n  \
             macOS: brew install cloudflared\n  \
             Linux: curl -L https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/') -o /usr/local/bin/cloudflared && chmod +x /usr/local/bin/cloudflared\n  \
             Or pick another provider: eli mcp share --provider tunnelmole"
        );
    }

    let mut child = tokio::process::Command::new("cloudflared")
        .args([
            "tunnel",
            "--url",
            &format!("http://localhost:{}", port),
            "--no-autoupdate",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn cloudflared")?;
    record_child_pid(&child);

    let pattern =
        regex::Regex::new(r"https://[a-z0-9-]+\.trycloudflare\.com").expect("valid regex");
    let url = read_url_from_child(&mut child, &pattern, 30).await?;

    print_share_block(
        "cloudflare quick tunnel (temporary)",
        &url,
        "URL stays up while this process runs. Dies on Ctrl-C or restart.",
    );
    wait_for_signal_or_exit(child).await
}

async fn provider_ngrok(args: ShareArgs) -> Result<()> {
    if which("ngrok").is_none() {
        anyhow::bail!(
            "ngrok not found. Install it:\n  \
             macOS: brew install ngrok/ngrok/ngrok\n  \
             Linux/Windows: https://download.ngrok.com\n\
             After install, sign up at https://dashboard.ngrok.com/signup, \
             then re-run with: eli mcp share --provider ngrok --authtoken <token> --domain <your>.ngrok-free.dev"
        );
    }

    if let Some(token) = args.authtoken.as_deref() {
        let status = tokio::process::Command::new("ngrok")
            .args(["config", "add-authtoken", token])
            .status()
            .await
            .context("ngrok config add-authtoken")?;
        if !status.success() {
            anyhow::bail!("`ngrok config add-authtoken` failed (exit {})", status);
        }
        eprintln!("[eli mcp share] ngrok authtoken stored.");
    }

    // Normalize domain: accept "mysub" or "mysub.ngrok-free.dev" or full https URL.
    let domain = args.domain.as_deref().map(|d| {
        let d = d.trim().trim_start_matches("https://").trim_end_matches('/');
        if d.contains('.') {
            d.to_string()
        } else {
            format!("{}.ngrok-free.dev", d)
        }
    });

    let mut cmd_args: Vec<String> = vec!["http".into(), args.port.to_string()];
    if let Some(d) = &domain {
        cmd_args.push(format!("--url={}", d));
    }
    cmd_args.push("--log=stdout".into());
    cmd_args.push("--log-format=logfmt".into());

    let mut child = tokio::process::Command::new("ngrok")
        .args(&cmd_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ngrok")?;
    record_child_pid(&child);

    let pattern = regex::Regex::new(
        r"https://[A-Za-z0-9-]+\.(?:ngrok-free\.dev|ngrok-free\.app|ngrok\.io|ngrok\.app)",
    )
    .expect("valid regex");
    let url = read_url_from_child(&mut child, &pattern, 30).await?;

    let note = if domain.is_some() {
        "Permanent URL — pasted in your reserved subdomain. Persists across restarts as long as your ngrok account holds it."
    } else {
        "Random URL — assigned by ngrok. Will rotate on next run. Pass --domain to use your reserved subdomain."
    };
    print_share_block("ngrok (permanent free)", &url, note);
    wait_for_signal_or_exit(child).await
}

fn provider_selfhost() -> Result<()> {
    println!();
    println!("┌─ Self-host mode ─────────────────────────────────────────────");
    println!("│");
    println!("│  Most secure option. Your data never leaves your machine.");
    println!("│");
    println!("│  Architecture: SNI-routing gateway on your VPS,");
    println!("│  TLS terminates on your laptop (gateway can't decrypt traffic).");
    println!("│");
    println!("│  Status: design phase — see SELFHOST.md");
    println!("│  Requires: a domain (~$10/yr) + a small VPS (~$5/mo)");
    println!("│");
    println!("│  For now, use --provider ngrok (permanent free) or");
    println!("│  --provider tunnelmole (instant temporary).");
    println!("└──────────────────────────────────────────────────────────────");
    println!();
    Ok(())
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Read both stdout and stderr of `child`, return the first match of
/// `pattern` from either. `timeout_secs` is the total budget.
async fn read_url_from_child(
    child: &mut tokio::process::Child,
    pattern: &regex::Regex,
    timeout_secs: u64,
) -> Result<String> {
    use tokio::io::AsyncBufReadExt as _;

    let stdout = child.stdout.take().context("no stdout pipe")?;
    let stderr = child.stderr.take().context("no stderr pipe")?;
    let mut out = tokio::io::BufReader::new(stdout).lines();
    let mut err = tokio::io::BufReader::new(stderr).lines();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let pattern = pattern.clone();
    let mut found: Option<String> = None;

    while found.is_none() {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                anyhow::bail!("tunnel did not print a URL within {}s", timeout_secs);
            }
            line = out.next_line() => {
                if let Ok(Some(line)) = line {
                    if let Some(m) = pattern.find(&line) {
                        found = Some(m.as_str().to_string());
                    }
                }
            }
            line = err.next_line() => {
                if let Ok(Some(line)) = line {
                    if let Some(m) = pattern.find(&line) {
                        found = Some(m.as_str().to_string());
                    }
                }
            }
        }
    }

    // Spawn background drainers so the child doesn't SIGPIPE on its
    // next write to stdout/stderr after we've moved on.
    tokio::spawn(async move {
        while let Ok(Some(_)) = out.next_line().await {}
    });
    tokio::spawn(async move {
        while let Ok(Some(_)) = err.next_line().await {}
    });

    Ok(found.expect("loop only exits with Some"))
}

fn print_share_block(label: &str, base_url: &str, note: &str) {
    let mcp_url = format!("{}/mcp", base_url);
    println!();
    println!("┌─ Market Search public URL ──────────────────────────────────");
    println!("│");
    println!("│  Provider: {}", label);
    println!("│");
    println!("│  Paste this into claude.ai (Settings → Connectors → Add)");
    println!("│  or ChatGPT (Settings → Apps & Connectors → Create):");
    println!("│");
    println!("│  {}", mcp_url);
    println!("│");
    println!("│  {}", note);
    println!("└──────────────────────────────────────────────────────────────");
    println!();
}

async fn wait_for_signal_or_exit(mut child: tokio::process::Child) -> Result<()> {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\n[eli mcp share] shutting down tunnel...");
            let _ = child.kill().await;
            let _ = std::fs::remove_file(SHARE_PID_FILE);
            Ok(())
        }
        status = child.wait() => {
            let _ = std::fs::remove_file(SHARE_PID_FILE);
            let status = status.context("tunnel child wait")?;
            anyhow::bail!("tunnel process exited unexpectedly: {}", status);
        }
    }
}

// ── orphan reaper ───────────────────────────────────────────────────────────
//
// When the user kills `eli mcp share` with SIGTERM (`kill <pid>`) or SIGKILL
// (`kill -9`), our tokio Ctrl-C handler doesn't run, so the spawned tunnel
// binary becomes an orphan. To recover, every share invocation writes the
// child's PID to /tmp/eli-share-children.pid and reaps any prior recorded
// PIDs at startup before spawning a new one.

fn record_child_pid(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        let _ = std::fs::write(SHARE_PID_FILE, format!("{}\n", pid));
    }
}

fn cleanup_orphan_children() {
    let Ok(content) = std::fs::read_to_string(SHARE_PID_FILE) else {
        return;
    };
    for line in content.lines() {
        if let Ok(pid) = line.trim().parse::<u32>() {
            // SIGTERM the previous tunnel binary if it's still alive. Errors are
            // expected (process may already be dead) — silently ignore.
            let _ = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            eprintln!("[eli mcp share] reaped orphan tunnel PID {}", pid);
        }
    }
    let _ = std::fs::remove_file(SHARE_PID_FILE);
}
