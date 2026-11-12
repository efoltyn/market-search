// Feedback to the Market Search developer through tell.dev.
//
// Agents use Market Search far more than people do, and they hit the edge
// cases first. The MCP server gives them a `report_issue` tool: when a tool
// errors, returns a wrong or stale number, or cannot do what was needed, the
// agent can send a note straight to the developer. A person can run
// `market-search feedback "..."` or open the hosted form.
//
// Nothing is sent unless an agent calls the tool or a person runs the command.
// Each note is also written to the local activity trail (finance_log), so the
// person can read exactly what left the machine.
//
// The key below is a publishable key, public by design: it can only add a note
// to the Market Search inbox on tell.dev. It cannot read, list or reply.
//
// Env:
//   MARKET_SEARCH_FEEDBACK=off        hide the tool and disable the command
//   MARKET_SEARCH_FEEDBACK_KEY=...    send to another tell.dev project (forks)
//   MARKET_SEARCH_FEEDBACK_URL=...    override the endpoint (testing)

const FEEDBACK_PUBLISHABLE_KEY: &str = "tell_pk_sndQpXc7X_l7y9gOQNEZDocO";
const FEEDBACK_ENDPOINT: &str = "https://tell.dev/api/v1/feedback";
const FEEDBACK_KINDS: &[&str] = &["bug", "idea", "question", "praise", "other"];

// tell.dev's field limits in bytes. It refuses a longer value, so cut first.
const FEEDBACK_MESSAGE_MAX: usize = 8000;
const FEEDBACK_ERROR_MAX: usize = 4000;
const FEEDBACK_ARGUMENTS_MAX: usize = 500;
const FEEDBACK_CONTACT_MAX: usize = 200;
const FEEDBACK_TOOL_MAX: usize = 120;
const FEEDBACK_AGENT_MAX: usize = 80;

/// Name of the MCP client that last sent `initialize`, such as "claude-code".
/// A hint for the developer, not an identity: over HTTP several clients can
/// share one server.
static FEEDBACK_MCP_CLIENT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn feedback_note_client(name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    if let Ok(mut slot) = FEEDBACK_MCP_CLIENT.lock() {
        *slot = Some(name.to_string());
    }
}

fn feedback_enabled() -> bool {
    !matches!(
        std::env::var("MARKET_SEARCH_FEEDBACK")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "off" | "0" | "false" | "no" | "disabled"
    )
}

fn feedback_key() -> String {
    std::env::var("MARKET_SEARCH_FEEDBACK_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| FEEDBACK_PUBLISHABLE_KEY.to_string())
}

fn feedback_endpoint() -> String {
    std::env::var("MARKET_SEARCH_FEEDBACK_URL")
        .ok()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| FEEDBACK_ENDPOINT.to_string())
}

/// The hosted form for the same inbox, for when the call cannot get through.
fn feedback_form_url() -> String {
    format!("https://tell.dev/f/{}", feedback_key())
}

/// Cut `s` to at most `max` bytes on a char boundary, marking the cut.
fn feedback_truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    const MARK: &str = "...[truncated]";
    let mut end = max.saturating_sub(MARK.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARK}", &s[..end])
}

#[derive(Debug, Clone, Default)]
struct FeedbackReport {
    message: String,
    kind: String,
    tool: Option<String>,
    arguments: Option<String>,
    error: Option<String>,
    /// How to reach the sender, if they want an answer. Only set on request.
    contact: Option<String>,
    /// True when an agent sent it; false for a person at a terminal.
    from_agent: bool,
    /// The agent's or MCP client's name, such as "claude-code".
    agent: Option<String>,
}

impl FeedbackReport {
    /// Parse the `report_issue` MCP arguments.
    fn from_mcp_args(args: &serde_json::Value) -> Result<Self> {
        let get = |k: &str| -> Option<String> {
            match args.get(k) {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => {
                    let s = s.trim();
                    (!s.is_empty()).then(|| s.to_string())
                }
                // Agents sometimes pass `arguments` as an object; keep it as JSON text.
                Some(other) => Some(other.to_string()),
            }
        };
        let message = get("message").ok_or_else(|| anyhow::anyhow!("`message` is required"))?;
        let kind = get("kind").unwrap_or_else(|| "bug".to_string()).to_ascii_lowercase();
        if !FEEDBACK_KINDS.contains(&kind.as_str()) {
            anyhow::bail!("`kind` must be one of: {}", FEEDBACK_KINDS.join(", "));
        }
        Ok(Self {
            message,
            kind,
            tool: get("tool"),
            arguments: get("arguments"),
            error: get("error"),
            contact: None,
            from_agent: true,
            agent: FEEDBACK_MCP_CLIENT.lock().ok().and_then(|slot| slot.clone()),
        })
    }

    /// The JSON body for POST /api/v1/feedback. tell.dev refuses unknown fields,
    /// so this sends only the ones it documents.
    fn to_body(&self) -> serde_json::Value {
        let mut from = serde_json::Map::new();
        from.insert("type".into(), json!(if self.from_agent { "agent" } else { "person" }));
        if let Some(agent) = &self.agent {
            from.insert("agent".into(), json!(feedback_truncate(agent, FEEDBACK_AGENT_MAX)));
        }
        if let Some(contact) = &self.contact {
            from.insert("contact".into(), json!(contact));
        }

        let mut context = serde_json::Map::new();
        context.insert("app_version".into(), json!(env!("CARGO_PKG_VERSION")));
        context.insert(
            "platform".into(),
            json!(format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)),
        );
        if let Some(tool) = &self.tool {
            context.insert("tool".into(), json!(feedback_truncate(tool, FEEDBACK_TOOL_MAX)));
        }
        if let Some(error) = &self.error {
            context.insert("error".into(), json!(feedback_truncate(error, FEEDBACK_ERROR_MAX)));
        }
        if let Some(arguments) = &self.arguments {
            context.insert(
                "metadata".into(),
                json!({ "arguments": feedback_truncate(arguments, FEEDBACK_ARGUMENTS_MAX) }),
            );
        }

        json!({
            "message": feedback_truncate(&self.message, FEEDBACK_MESSAGE_MAX),
            "kind": self.kind,
            "from": from,
            "context": context,
        })
    }
}

/// POST the report to tell.dev and return the note id (msg_...). With `dry_run`
/// tell.dev checks the key and every field, stores nothing, and the id is empty.
async fn feedback_send(report: &FeedbackReport, dry_run: bool) -> Result<String> {
    let body = report.to_body();
    let started = std::time::Instant::now();
    let outcome = feedback_post(&body, dry_run).await;
    if !dry_run {
        feedback_audit(report, &body, started, &outcome);
    }
    outcome
}

async fn feedback_post(body: &serde_json::Value, dry_run: bool) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!("market-search/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;

    let mut request = client
        .post(feedback_endpoint())
        .bearer_auth(feedback_key())
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(body);
    if dry_run {
        request = request.header("Tell-Dry-Run", "true");
    }
    let resp = request.send().await.map_err(|e| {
        if e.is_timeout() {
            anyhow::anyhow!("tell.dev did not answer within 10 s")
        } else {
            anyhow::anyhow!("could not reach tell.dev: {e}")
        }
    })?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let parsed: Option<serde_json::Value> = serde_json::from_str(&text).ok();

    if status.is_success() {
        return Ok(parsed
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string());
    }

    let err = parsed.as_ref().and_then(|v| v.get("error"));
    let code = err.and_then(|e| e.get("code")).and_then(|c| c.as_str());
    let message = err.and_then(|e| e.get("message")).and_then(|m| m.as_str());
    match (code, message) {
        (Some(code), Some(message)) => {
            anyhow::bail!("tell.dev refused the note ({} {code}): {message}", status.as_u16())
        }
        _ => anyhow::bail!(
            "tell.dev returned HTTP {}: {}",
            status.as_u16(),
            feedback_truncate(text.trim(), 300)
        ),
    }
}

/// Write the note to the local activity trail: what was sent, where, and
/// whether tell.dev took it. Best-effort, like every other trail write.
fn feedback_audit(
    report: &FeedbackReport,
    body: &serde_json::Value,
    started: std::time::Instant,
    outcome: &Result<String>,
) {
    if !audit_enabled() {
        return;
    }
    let via = if report.from_agent { "report_issue" } else { "send" };
    let mut record = base_record(chrono::Utc::now(), vec!["feedback".to_string(), via.to_string()]);
    record["duration_ms"] = json!(started.elapsed().as_millis() as u64);
    let mut summary = json!({ "sent_to": feedback_endpoint(), "body": body });
    match outcome {
        Ok(note_id) => {
            record["status"] = json!("ok");
            summary["note_id"] = json!(note_id);
        }
        Err(e) => {
            record["status"] = json!("error");
            record["error"] = json!(format!("{e:#}"));
        }
    }
    record["summary"] = summary;
    append_chained_record(record);
}

/// tools/call for report_issue. Failures are tool errors (isError), not
/// JSON-RPC errors, so the agent reads tell.dev's reason and can fix the note.
async fn mcp_report_issue(id: serde_json::Value, args: &serde_json::Value) -> serde_json::Value {
    let result = |text: String, is_error: bool| {
        let mut result = json!({ "content": [{ "type": "text", "text": text }] });
        if is_error {
            result["isError"] = json!(true);
        }
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    };
    if !feedback_enabled() {
        return result(
            "Not sent: feedback is turned off on this machine (MARKET_SEARCH_FEEDBACK=off).".to_string(),
            true,
        );
    }
    let report = match FeedbackReport::from_mcp_args(args) {
        Ok(r) => r,
        Err(e) => return result(format!("Not sent: {e}"), true),
    };
    match feedback_send(&report, false).await {
        Ok(note_id) => result(format!("Sent to the Market Search developer (id {note_id})."), false),
        Err(e) => result(format!("Not sent: {e}"), true),
    }
}

async fn cmd_feedback(args: FeedbackArgs) -> Result<()> {
    if !feedback_enabled() {
        anyhow::bail!("feedback is turned off (MARKET_SEARCH_FEEDBACK=off)");
    }
    let message = args.message.join(" ");
    let message = message.trim();
    if message.is_empty() {
        anyhow::bail!("the message is empty. Usage: market-search feedback \"what happened\" [--kind bug]");
    }
    let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let contact = clean(args.contact);
    if contact.as_ref().is_some_and(|c| c.len() > FEEDBACK_CONTACT_MAX) {
        anyhow::bail!("--contact is longer than {FEEDBACK_CONTACT_MAX} bytes");
    }
    let agent = clean(args.agent);
    let report = FeedbackReport {
        message: message.to_string(),
        kind: args.kind.as_str().to_string(),
        tool: clean(args.tool),
        arguments: clean(args.arguments),
        error: clean(args.error),
        contact,
        from_agent: agent.is_some(),
        agent,
    };
    let note_id = feedback_send(&report, args.dry_run)
        .await
        .with_context(|| format!("not sent; you can also use {}", feedback_form_url()))?;
    if args.dry_run {
        println!("tell.dev checked the note and would accept it. Nothing was sent.");
    } else {
        println!("Sent to the Market Search developer (id {note_id}). Thanks.");
    }
    Ok(())
}
