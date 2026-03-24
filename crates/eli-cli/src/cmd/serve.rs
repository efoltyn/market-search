use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::net::SocketAddr;
use tokio::sync::broadcast;
// Duration already in scope from context.rs include

// Embedded fallback (always compiled in, used in release builds and as fallback)
const SERVE_INDEX_HTML: &str = include_str!("../serve_ui.html");

// In debug builds: read serve_ui.html from disk on every request so UI edits
// require zero recompilation. Falls back to embedded if file not found.
fn serve_ui_html() -> String {
    #[cfg(debug_assertions)]
    {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/serve_ui.html");
        if let Ok(s) = std::fs::read_to_string(&path) {
            return s;
        }
    }
    SERVE_INDEX_HTML.to_string()
}

// ── state ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ServeAppState {
    reports_dir: PathBuf,
    sentinel_paths: eli_core::sentinel::SentinelPaths,
    ws_tx: broadcast::Sender<String>,
}

#[derive(Debug, serde::Deserialize)]
struct SpawnSettingsUpdate {
    default_spawn_target: String,
    codex_max_spawns_per_hour: u32,
    claude_max_spawns_per_hour: u32,
    gemini_max_spawns_per_hour: u32,
}

// ── entry point ───────────────────────────────────────────────────────────────

async fn cmd_serve(args: ServeArgs) -> Result<()> {
    let reports_dir = picks_expand_path(&args.reports_dir);
    tokio::fs::create_dir_all(&reports_dir)
        .await
        .context("create reports dir")?;
    let sentinel_paths = eli_core::sentinel::resolve_paths(args.sentinel_dir, None, None)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve sentinel paths")?;
    sentinel_paths
        .ensure_dirs()
        .map_err(|e| anyhow::anyhow!(e))
        .context("create sentinel dirs")?;

    let (ws_tx, _) = broadcast::channel::<String>(64);
    let state = std::sync::Arc::new(ServeAppState {
        reports_dir,
        sentinel_paths: sentinel_paths.clone(),
        ws_tx: ws_tx.clone(),
    });

    // Push sentinel state updates to the monitor UI on a short polling loop.
    tokio::spawn(daemon_state_watcher(sentinel_paths, ws_tx));

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/reports/{filename}", get(serve_report_file))
        .route("/reports-md/{filename}", get(serve_md_report))
        .route("/api/reports", get(api_reports))
        .route("/api/monitor", get(api_monitor))
        .route("/api/picks", get(api_picks))
        .route("/api/daemons", get(api_daemons))
        .route(
            "/api/spawn-settings",
            get(api_spawn_settings).post(api_update_spawn_settings),
        )
        .route("/ws", get(ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let url = format!("http://localhost:{}", args.port);
    eprintln!("eli monitor → {url}");

    if args.open {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind port {}", args.port))?;

    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}

// ── websocket ─────────────────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<std::sync::Arc<ServeAppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state.ws_tx.subscribe()))
}

async fn handle_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    loop {
        tokio::select! {
            Ok(msg) = rx.recv() => {
                if socket.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                if msg.is_none() { break; }
            }
        }
    }
}

// ── daemon price watcher ──────────────────────────────────────────────────────

async fn daemon_state_watcher(
    sentinel_paths: eli_core::sentinel::SentinelPaths,
    tx: broadcast::Sender<String>,
) {
    loop {
        if let Some(payload) = daemon_monitor_payload(&sentinel_paths).await {
            let _ = tx.send(payload);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn daemon_monitor_payload(
    sentinel_paths: &eli_core::sentinel::SentinelPaths,
) -> Option<String> {
    let config_paths = eli_core::config::Paths::discover().ok()?;
    let cfg = eli_core::config::load_or_default(&config_paths).ok()?;
    let sentinel_cfg = cfg.sentinel;
    let daemon_state = eli_core::sentinel::io::load_daemon_state(sentinel_paths)
        .ok()
        .unwrap_or_default();
    let registry = eli_core::sentinel::io::load_registry(sentinel_paths)
        .ok()
        .unwrap_or_default();
    let pid = eli_core::sentinel::io::read_pid(sentinel_paths)
        .ok()
        .flatten();
    let daemon_running = pid.map(serve_process_alive).unwrap_or(false);
    let now = chrono::Utc::now();
    let codex_used = daemon_state
        .spawn_budget
        .codex_recent_spawns
        .iter()
        .filter(|ts| now.signed_duration_since(**ts).num_seconds() < 3600)
        .count() as u32;
    let claude_used = daemon_state
        .spawn_budget
        .claude_recent_spawns
        .iter()
        .filter(|ts| now.signed_duration_since(**ts).num_seconds() < 3600)
        .count() as u32;
    let gemini_used = daemon_state
        .spawn_budget
        .gemini_recent_spawns
        .iter()
        .filter(|ts| now.signed_duration_since(**ts).num_seconds() < 3600)
        .count() as u32;
    let mut daemons = Vec::new();
    for sub in registry.subscriptions.into_iter() {
        let (watch_summary, observed_values, condition_met) = match eli_core::sentinel::evaluator::evaluate_subscription(&sub).await {
            Ok(eval) => {
                let summary = serve_watch_summary(&eval);
                let triggered = eval.triggered;
                // Map var_name -> current observed value for gap computation in UI
                let obs_map: serde_json::Map<String, serde_json::Value> = eval
                    .observations
                    .iter()
                    .map(|(var_name, obs)| (var_name.clone(), serde_json::json!(obs.value)))
                    .collect();
                (summary, serde_json::Value::Object(obs_map), triggered)
            }
            Err(_) => (None, serde_json::Value::Object(serde_json::Map::new()), false),
        };
        daemons.push(
            serde_json::json!({
                "id": sub.id,
                "name": sub.name,
                "title": sub.title,
                "sourceReportTitle": sub.source_report_title,
                "sourceReportDate": sub.source_report_date,
                "sourceReportFile": sub.source_report_file,
                "sourceEvidence": sub.source_evidence,
                "condition": sub.expr,
                "severity": format!("{:?}", sub.severity).to_lowercase(),
                "status": if !sub.enabled {
                    "paused"
                } else if sub.prediction_resolved {
                    match sub.prediction_result.as_deref() {
                        Some("HIT") => "hit",
                        Some("MISS") => "miss",
                        _ => "resolved",
                    }
                } else {
                    "pending"
                },
                "enabled": sub.enabled,
                "spawnAgent": sub.spawn_agent,
                "spawnTarget": match sub.spawn_target {
                    eli_core::sentinel::SpawnTarget::Default => {
                        format!("{:?}", sentinel_cfg.default_spawn_target).to_ascii_lowercase()
                    }
                    _ => format!("{:?}", sub.spawn_target).to_ascii_lowercase(),
                },
                "cooldownSecs": sub.cooldown_secs,
                "lastTriggeredAt": sub.last_triggered_at.map(|ts| ts.to_rfc3339()),
                "lastSpawnedAt": sub.last_spawned_at.map(|ts| ts.to_rfc3339()),
                "triggerReady": sub
                    .last_triggered_at
                    .map(|ts| now.signed_duration_since(ts).num_seconds() >= sub.cooldown_secs as i64)
                    .unwrap_or(true),
                "vars": sub.vars,
                "watchSummary": watch_summary,
                "fireAt": sub.fire_at.map(|ts| ts.to_rfc3339()),
                "predictionResolved": sub.prediction_resolved,
                "predictionResult": &sub.prediction_result,
                "predictionText": &sub.prediction,
                "resolvedActual": sub.resolved_actual,
                "resolvedAt": sub.resolved_at.map(|ts| ts.to_rfc3339()),
                "deadline": sub.deadline.map(|ts| ts.to_rfc3339()),
                "createdAt": sub.created_at.map(|ts| ts.to_rfc3339()),
                "daemonType": if sub.fire_at.is_some() { "scheduled" } else { "breakthrough" },
                "targetVar": &sub.target_var,
                "targetValue": sub.target_value,
                "observedValues": observed_values,
                "conditionMet": condition_met,
                "daemonKind": if sub.fire_at.is_some() { "scheduled" } else if sub.prediction.is_some() { "prediction" } else { "watch" },
            }),
        );
    }

    let payload = serde_json::json!({
        "type": "daemon_update",
        "daemons": daemons,
        "daemon_running": daemon_running,
        "spawnSettings": {
            "defaultSpawnTarget": format!("{:?}", sentinel_cfg.default_spawn_target).to_ascii_lowercase(),
            "codexMaxSpawnsPerHour": sentinel_cfg.codex_max_spawns_per_hour,
            "claudeMaxSpawnsPerHour": sentinel_cfg.claude_max_spawns_per_hour,
            "geminiMaxSpawnsPerHour": sentinel_cfg.gemini_max_spawns_per_hour,
        },
        "spawnBudget": {
            "codexUsedLastHour": codex_used,
            "codexRemaining": sentinel_cfg.codex_max_spawns_per_hour.saturating_sub(codex_used),
            "claudeUsedLastHour": claude_used,
            "claudeRemaining": sentinel_cfg.claude_max_spawns_per_hour.saturating_sub(claude_used),
            "geminiUsedLastHour": gemini_used,
            "geminiRemaining": sentinel_cfg.gemini_max_spawns_per_hour.saturating_sub(gemini_used),
        },
        "heartbeat_at": daemon_state.heartbeat_at.map(|ts| ts.to_rfc3339()),
        "ts": chrono::Utc::now().to_rfc3339()
    });
    Some(payload.to_string())
}

fn serve_watch_summary(eval: &eli_core::sentinel::evaluator::Evaluation) -> Option<String> {
    let mut parts = Vec::new();
    for (_, obs) in eval.observations.iter().take(2) {
        let value = if matches!(obs.source.as_str(), "kalshi" | "polymarket") {
            format!("{:.1}%", obs.value * 100.0)
        } else {
            format!("{:.2}", obs.value)
        };
        parts.push(format!("{} {}", obs.instrument, value));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

// ── handlers ──────────────────────────────────────────────────────────────────

async fn serve_index() -> Html<String> {
    Html(serve_ui_html())
}

// axum Path extractor — use fully qualified to avoid clash with std::path::Path
async fn serve_report_file(
    path_param: axum::extract::Path<String>,
    State(state): State<std::sync::Arc<ServeAppState>>,
) -> Response {
    let filename = path_param.0;
    if filename.contains("..") || filename.contains('/') {
        return (StatusCode::BAD_REQUEST, "invalid filename").into_response();
    }
    let path = state.reports_dir.join(&filename);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut resp = bytes.into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/html; charset=utf-8"),
            );
            resp
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Serve a Markdown report rendered to HTML via marked.js.
async fn serve_md_report(
    path_param: axum::extract::Path<String>,
    State(state): State<std::sync::Arc<ServeAppState>>,
) -> Response {
    let filename = path_param.0;
    if filename.contains("..") || filename.contains('/') {
        return (StatusCode::BAD_REQUEST, "invalid filename").into_response();
    }
    let path = state.reports_dir.join(&filename);
    match tokio::fs::read_to_string(&path).await {
        Ok(md_text) => {
            let escaped = |text: &str| {
                text.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            };
            let (yaml_front, body) = if let Some(rest) = md_text.strip_prefix("---\n") {
                if let Some(end) = rest.find("\n---\n") {
                    (&rest[..end], &rest[end + 5..])
                } else {
                    ("", md_text.as_str())
                }
            } else {
                ("", md_text.as_str())
            };
            let yaml_block = if yaml_front.trim().is_empty() {
                String::new()
            } else {
                format!(r#"<pre class="yaml-front">{}</pre>"#, escaped(yaml_front))
            };
            let body = escaped(body);
            let html = format!(
                r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{filename}</title>
<style>
  :root {{
    --bg: #f6f2ea;
    --paper: #fffdf8;
    --line: #d8cec0;
    --text: #1f2933;
    --muted: #6b7280;
    --ink: #0f1720;
  }}
  body {{
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font-family: "Iowan Old Style", "Palatino Linotype", Georgia, serif;
    padding: 32px;
  }}
  .shell {{
    max-width: 980px;
    margin: 0 auto;
    background: var(--paper);
    border: 1px solid var(--line);
    box-shadow: 0 24px 50px rgba(31, 41, 51, 0.08);
  }}
  .head {{
    padding: 16px 20px;
    border-bottom: 1px solid var(--line);
    font: 600 12px/1.2 "SF Mono", "IBM Plex Mono", monospace;
    letter-spacing: 0.12em;
    color: var(--muted);
    text-transform: uppercase;
  }}
  .yaml-front {{
    margin: 20px;
    padding: 14px 16px;
    border: 1px solid var(--line);
    background: #f3eee4;
    color: var(--muted);
    font: 12px/1.7 "SF Mono", "IBM Plex Mono", monospace;
    white-space: pre-wrap;
  }}
  .md-body {{
    margin: 20px;
    padding: 0 0 12px;
    white-space: pre-wrap;
    font-size: 16px;
    line-height: 1.75;
  }}
</style>
</head>
<body>
<div class="shell">
  <div class="head">Markdown Report</div>
  {yaml_block}
  <pre class="md-body">{body}</pre>
</div>
</body>
</html>"#
            );
            let mut resp = html.into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/html; charset=utf-8"),
            );
            resp
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn api_reports(State(state): State<std::sync::Arc<ServeAppState>>) -> Response {
    #[derive(Default)]
    struct ReportAggregate {
        name: String,
        title: Option<String>,
        author: Option<String>,
        researcher: Option<String>,
        modified: u64,
        preferred_file: String,
        preferred_url: String,
        preferred_kind: String,
        preferred_modified: u64,
        formats: Vec<String>,
    }

    let dir = &state.reports_dir;
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Json(serde_json::json!([])).into_response();
    };

    let mut by_name: std::collections::HashMap<String, ReportAggregate> =
        std::collections::HashMap::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "html" && ext != "md" {
            continue;
        }
        let fname = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if fname.is_empty() {
            continue;
        }
        let modified = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs())
            })
            .unwrap_or(0);

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // Extract metadata differently for .md (YAML front matter) vs .html (meta tags)
        let (title_tag, author, researcher) = if ext == "md" {
            let text = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            let title = serve_yaml_field(&text, "title");
            let res = serve_yaml_field(&text, "researcher");
            let auth = serve_yaml_field(&text, "author");
            (title, auth, res)
        } else {
            let head_bytes = {
                let mut buf = vec![0u8; 3072];
                if let Ok(mut f) = tokio::fs::File::open(&path).await {
                    use tokio::io::AsyncReadExt;
                    let n = f.read(&mut buf).await.unwrap_or(0);
                    buf.truncate(n);
                    buf
                } else {
                    vec![]
                }
            };
            let author = serve_meta(&head_bytes, "eli:author");
            let researcher = serve_meta(&head_bytes, "eli:researcher");
            let title_tag =
                serve_meta(&head_bytes, "eli:title").or_else(|| serve_html_title(&head_bytes));
            (title_tag, author, researcher)
        };

        // For .md files, the frontend should open /reports-md/{fname}, not /reports/{fname}
        let report_url = if ext == "md" {
            format!("/reports-md/{fname}")
        } else {
            format!("/reports/{fname}")
        };

        let aggregate = by_name
            .entry(name.clone())
            .or_insert_with(|| ReportAggregate {
                name: name.clone(),
                ..Default::default()
            });
        aggregate.modified = aggregate.modified.max(modified);
        if !aggregate.formats.iter().any(|fmt| fmt == ext) {
            aggregate.formats.push(ext.to_string());
            aggregate.formats.sort();
        }
        if title_tag.is_some() && (aggregate.title.is_none() || ext == "html") {
            aggregate.title = title_tag;
        }
        if author.is_some() && (aggregate.author.is_none() || ext == "html") {
            aggregate.author = author;
        }
        if researcher.is_some() && (aggregate.researcher.is_none() || ext == "html") {
            aggregate.researcher = researcher;
        }

        let should_prefer = aggregate.preferred_file.is_empty()
            || modified > aggregate.preferred_modified
            || (modified == aggregate.preferred_modified
                && ext == "html"
                && aggregate.preferred_kind != "html");
        if should_prefer {
            aggregate.preferred_file = fname;
            aggregate.preferred_url = report_url;
            aggregate.preferred_kind = ext.to_string();
            aggregate.preferred_modified = modified;
        }
    }

    let mut reports: Vec<serde_json::Value> = by_name
        .into_values()
        .map(|report| {
            serde_json::json!({
                "file": report.preferred_file,
                "name": report.name,
                "title": report.title,
                "author": report.author,
                "researcher": report.researcher,
                "date": serve_format_date(report.modified),
                "date_iso": serve_format_iso(report.modified),
                "modified": report.modified,
                "url": report.preferred_url,
                "kind": report.preferred_kind,
                "formats": report.formats,
            })
        })
        .collect();

    reports.sort_by(|a, b| {
        b["modified"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["modified"].as_u64().unwrap_or(0))
    });
    reports.truncate(50);
    Json(reports).into_response()
}

async fn api_picks(
    Query(params): Query<std::collections::HashMap<String, String>>,
    State(state): State<std::sync::Arc<ServeAppState>>,
) -> Response {
    let file = params.get("file").cloned().unwrap_or_default();
    let refresh = params.get("refresh").map(|v| v == "true").unwrap_or(false);
    if file.contains("..") || file.contains('/') {
        return (StatusCode::BAD_REQUEST, "invalid filename").into_response();
    }
    let mut candidates = vec![state.reports_dir.join(format!("{}.picks.json", file))];
    if let Some(stem) = std::path::Path::new(&file)
        .file_stem()
        .and_then(|s| s.to_str())
    {
        candidates.push(state.reports_dir.join(format!("{}.picks.json", stem)));
    }
    for sidecar in candidates {
        if let Some(picks) = picks_load_with_refresh(&sidecar, refresh).await {
            return Json(serde_json::to_value(picks).unwrap_or_default()).into_response();
        }
    }
    Json(serde_json::json!({"picks": [], "logged_at": null})).into_response()
}

async fn api_monitor(State(state): State<std::sync::Arc<ServeAppState>>) -> Response {
    match daemon_monitor_payload(&state.sentinel_paths).await {
        Some(payload) => Json(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap_or_else(|_| {
                serde_json::json!({
                    "type": "daemon_update",
                    "daemons": [],
                    "daemon_running": false,
                    "spawnSettings": null,
                    "spawnBudget": null,
                })
            }),
        )
        .into_response(),
        None => Json(serde_json::json!({
            "type": "daemon_update",
            "daemons": [],
            "daemon_running": false,
            "spawnSettings": null,
            "spawnBudget": null,
        }))
        .into_response(),
    }
}

async fn api_daemons(State(state): State<std::sync::Arc<ServeAppState>>) -> Response {
    match daemon_monitor_payload(&state.sentinel_paths).await {
        Some(payload) => {
            let value: serde_json::Value =
                serde_json::from_str(&payload).unwrap_or(serde_json::json!({"daemons": []}));
            Json(value["daemons"].clone()).into_response()
        }
        None => Json(serde_json::json!([])).into_response(),
    }
}

async fn api_spawn_settings() -> Response {
    let paths = match eli_core::config::Paths::discover() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "config paths unavailable",
            )
                .into_response()
        }
    };
    let cfg = match eli_core::config::load_or_create(&paths) {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "config unavailable").into_response(),
    };
    let out = serde_json::json!({
        "defaultSpawnTarget": format!("{:?}", cfg.sentinel.default_spawn_target).to_ascii_lowercase(),
        "codexMaxSpawnsPerHour": cfg.sentinel.codex_max_spawns_per_hour,
        "claudeMaxSpawnsPerHour": cfg.sentinel.claude_max_spawns_per_hour,
        "geminiMaxSpawnsPerHour": cfg.sentinel.gemini_max_spawns_per_hour,
        "codexCommand": cfg.sentinel.codex_agent_cmd,
        "claudeCommand": cfg.sentinel.claude_agent_cmd,
        "geminiCommand": cfg.sentinel.gemini_agent_cmd,
    });
    Json(out).into_response()
}

async fn api_update_spawn_settings(Json(body): Json<SpawnSettingsUpdate>) -> Response {
    let paths = match eli_core::config::Paths::discover() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "config paths unavailable",
            )
                .into_response()
        }
    };
    let mut cfg = match eli_core::config::load_or_create(&paths) {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "config unavailable").into_response(),
    };
    let target = match body
        .default_spawn_target
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "codex" => eli_core::sentinel::SpawnTarget::Codex,
        "claude" => eli_core::sentinel::SpawnTarget::Claude,
        "gemini" => eli_core::sentinel::SpawnTarget::Gemini,
        "both" => eli_core::sentinel::SpawnTarget::Both,
        "all" => eli_core::sentinel::SpawnTarget::All,
        _ => eli_core::sentinel::SpawnTarget::All,
    };
    cfg.sentinel.default_spawn_target = target;
    cfg.sentinel.codex_max_spawns_per_hour = body.codex_max_spawns_per_hour.min(60);
    cfg.sentinel.claude_max_spawns_per_hour = body.claude_max_spawns_per_hour.min(60);
    cfg.sentinel.gemini_max_spawns_per_hour = body.gemini_max_spawns_per_hour.min(60);
    if let Err(_) = eli_core::config::save(&paths, &cfg) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to save settings").into_response();
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn serve_process_alive(pid: u32) -> bool {
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

fn serve_format_date(secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(secs);
    let diff = now.saturating_sub(secs);
    if diff < 86_400 {
        "today".to_string()
    } else if diff < 172_800 {
        "yesterday".to_string()
    } else {
        format!("{}d ago", diff / 86_400)
    }
}

fn serve_format_iso(secs: u64) -> String {
    use chrono::{DateTime, Utc};
    let dt = DateTime::<Utc>::from_timestamp(secs as i64, 0).unwrap_or_else(chrono::Utc::now);
    dt.format("%Y-%m-%d").to_string()
}

/// Extract a field from YAML front matter in a Markdown string.
/// Handles: key: value, key: "value", key: 'value'
fn serve_yaml_field(text: &str, key: &str) -> Option<String> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after = &trimmed[3..];
    let end = after.find("\n---")?;
    let yaml = &after[..end];
    for line in yaml.lines() {
        if let Some(rest) = line.trim().strip_prefix(&format!("{key}:")) {
            let val = rest
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string();
            if !val.is_empty() && val != "[]" {
                return Some(val);
            }
        }
    }
    None
}

/// Extract `<meta name="KEY" content="VALUE">` from raw HTML bytes.
fn serve_meta(html: &[u8], key: &str) -> Option<String> {
    let text = std::str::from_utf8(html).ok()?;
    let search = format!("name=\"{}\"", key);
    let alt = format!("name='{}'", key);
    let pos = text.find(&search).or_else(|| text.find(&alt))?;
    // find content= after the meta name
    let after = &text[pos..];
    let content_pos = after
        .find("content=\"")
        .map(|p| (p + 9, '"'))
        .or_else(|| after.find("content='").map(|p| (p + 9, '\'')));
    let (start, delim) = content_pos?;
    let value_str = &after[start..];
    let end = value_str.find(delim)?;
    let v = value_str[..end].trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Extract the HTML <title> tag content.
fn serve_html_title(html: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(html).ok()?;
    let start = text.find("<title>")? + 7;
    let end = text[start..].find("</title>")?;
    let v = text[start..start + end].trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}
