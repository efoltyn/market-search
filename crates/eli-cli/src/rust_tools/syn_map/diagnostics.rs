fn augment_tool_errors(results: Vec<CommandResult>) -> Vec<CommandResult> {
    results
        .into_iter()
        .map(|mut r| {
            if !r.allowed || r.returncode == 0 {
                return r;
            }

            if !looks_like_clap_error(&r.stderr) {
                return r;
            }

            let path = match extract_eli_tool_path(&r.command) {
                Some(path) => path,
                None => return r,
            };

            if path.first().map(|p| p.as_str()) == Some("tool-info") {
                return r;
            }

            let info = build_tool_info(&path);
            let info_hint = compact_tool_info_hint(&info);
            let sep = if r.stderr.trim().is_empty() { "" } else { "\n" };
            r.stderr = format!("{}{}[TOOL INFO] {}", r.stderr.trim_end(), sep, info_hint);
            r
        })
        .collect()
}

fn compact_tool_info_hint(info: &ToolInfoResponse) -> String {
    let mut flags: Vec<String> = info
        .args
        .iter()
        .filter_map(|a| a.long.as_ref().map(|v| format!("--{v}")))
        .collect();
    flags.sort();
    flags.dedup();
    if flags.len() > 12 {
        flags.truncate(12);
        flags.push("...".to_string());
    }

    let mut subcommands: Vec<String> = info.subcommands.iter().map(|s| s.name.clone()).collect();
    subcommands.sort();
    subcommands.dedup();
    if subcommands.len() > 8 {
        subcommands.truncate(8);
        subcommands.push("...".to_string());
    }

    let flags_text = if flags.is_empty() {
        "none".to_string()
    } else {
        flags.join(", ")
    };
    let subs_text = if subcommands.is_empty() {
        "none".to_string()
    } else {
        subcommands.join(", ")
    };

    format!(
        "command=`{}` flags=[{}] subcommands=[{}]",
        info.command, flags_text, subs_text
    )
}

fn looks_like_clap_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("error:") && (lower.contains("usage:") || lower.contains("try '--help'"))
}

fn extract_eli_tool_path(command: &str) -> Option<Vec<String>> {
    let mut parts = command.split_whitespace();
    let first = parts.next()?;
    let is_eli = first == "eli" || first.ends_with("/eli") || first.ends_with("\\eli");
    if !is_eli {
        return None;
    }

    let mut path = Vec::new();
    for tok in parts {
        if tok.starts_with('-') {
            break;
        }
        path.push(tok.to_string());
    }

    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

fn is_suppression_exempt(command: &str) -> bool {
    let trimmed = command.trim_start();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    let mut parts = lower.split_whitespace();
    let Some(bin) = parts.next() else {
        return false;
    };

    let is_eli = bin == "eli" || bin.ends_with("/eli") || bin.ends_with("\\eli");
    if !is_eli {
        return false;
    }

    let Some(domain) = parts.next() else {
        return false;
    };
    if domain != "finance" {
        return false;
    }

    let Some(tool) = parts.next() else {
        return false;
    };

    match tool {
        "search" => true,
        "odds" => {
            let rest = parts.collect::<Vec<_>>();
            rest.iter()
                .any(|t| *t == "--list-events" || *t == "--list-series")
        }
        "options" => {
            let rest = parts.collect::<Vec<_>>();
            rest.iter().any(|t| *t == "--expirations")
        }
        _ => false,
    }
}

fn infer_sources(command: &str, stdout: &str) -> Vec<&'static str> {
    let cmd_lower = command.to_ascii_lowercase();
    let mut out: Vec<&'static str> = Vec::new();

    if cmd_lower.contains("eli finance odds") {
        let out_lower = stdout.to_ascii_lowercase();
        if out_lower.contains("kalshi") {
            out.push("Kalshi");
        }
        if out_lower.contains("polymarket") {
            out.push("Polymarket");
        }
        return dedupe_sources(out);
    }

    if cmd_lower.contains("eli finance prices") {
        out.push("Pyth");
        return out;
    }

    if cmd_lower.contains("eli finance") {
        if let Some(source) = infer_sources_from_json(stdout) {
            out.extend(source);
            return dedupe_sources(out);
        }
        if cmd_lower.contains("--provider fred") {
            out.push("FRED");
        } else if cmd_lower.contains("--provider yahoo") {
            out.push("Yahoo Finance");
        } else if cmd_lower.contains("--provider mock") {
            out.push("Mock");
        }
    }

    dedupe_sources(out)
}

fn infer_sources_from_json(stdout: &str) -> Option<Vec<&'static str>> {
    let value: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let mut out: Vec<&'static str> = Vec::new();

    if let Some(provider) = value.get("provider").and_then(|v| v.as_str()) {
        match provider {
            "yahoo" => out.push("Yahoo Finance"),
            "fred" => out.push("FRED"),
            "mock" => out.push("Mock"),
            _ => {}
        }
    }

    if let Some(source) = value.get("source").and_then(|v| v.as_str()) {
        match source {
            "pyth" => out.push("Pyth"),
            "kalshi" => out.push("Kalshi"),
            "polymarket" => out.push("Polymarket"),
            _ => {}
        }
    }

    if let Some(sources) = value.get("sources").and_then(|v| v.as_array()) {
        for s in sources {
            if let Some(name) = s.get("source").and_then(|v| v.as_str()) {
                match name {
                    "kalshi" => out.push("Kalshi"),
                    "polymarket" => out.push("Polymarket"),
                    "pyth" => out.push("Pyth"),
                    "fred" => out.push("FRED"),
                    "yahoo" => out.push("Yahoo Finance"),
                    "mock" => out.push("Mock"),
                    _ => {}
                }
            }
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(dedupe_sources(out))
    }
}

fn dedupe_sources(mut sources: Vec<&'static str>) -> Vec<&'static str> {
    sources.sort_unstable();
    sources.dedup();
    sources
}

fn count_data_points(value: &serde_json::Value) -> usize {
    fn array_len(v: Option<&serde_json::Value>) -> Option<usize> {
        v.and_then(|vv| vv.as_array().map(|a| a.len()))
    }

    match value {
        serde_json::Value::Array(arr) => arr.len(),
        serde_json::Value::Object(map) => {
            if let Some(series) = map.get("series").and_then(|v| v.as_array()) {
                let mut total = 0usize;
                for s in series {
                    total += s
                        .get("candles")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                }
                if total > 0 {
                    return total;
                }
            }

            if let Some(n) = array_len(map.get("snapshots")) {
                return n;
            }
            if let Some(n) = array_len(map.get("prices")) {
                return n;
            }
            if let Some(n) = array_len(map.get("available_events")) {
                return n;
            }
            if let Some(n) = array_len(map.get("available_tags")) {
                return n;
            }
            if let Some(n) = array_len(map.get("events")) {
                return n;
            }
            if let Some(n) = array_len(map.get("markets")) {
                return n;
            }
            if let Some(n) = array_len(map.get("results")) {
                return n;
            }

            map.len()
        }
        _ => 1,
    }
}

fn build_observation(
    read_mode: bool,
    approvals_ask_commands: bool,
    approvals_ask_diffs: bool,
    diffs: &[DiffResult],
    commands: &[CommandResult],
) -> String {
    let mode = if read_mode { "read" } else { "work" };
    let approvals_cmds = if approvals_ask_commands {
        "ask"
    } else {
        "auto"
    };
    let approvals_diffs = if approvals_ask_diffs { "ask" } else { "auto" };

    let mut out = String::new();
    out.push_str(&format!(
        "mode={mode}, approvals_cmds={approvals_cmds}, approvals_diffs={approvals_diffs}\n"
    ));

    if !diffs.is_empty() {
        out.push_str("diffs:\n");
        for r in diffs {
            out.push_str(&format!(
                "- {op} {path}: {status} {msg}\n",
                op = r.op,
                path = r.path,
                status = if r.success { "OK" } else { "ERR" },
                msg = r.message
            ));
        }
    }

    if !commands.is_empty() {
        out.push_str("commands:\n");
        for r in commands {
            out.push_str(&format!(
                "- `{cmd}` => {code} ({ms}ms)\n",
                cmd = r.command,
                code = r.returncode,
                ms = r.duration_ms
            ));
            let digest = build_command_digest(r);
            if !digest.trim().is_empty() {
                out.push_str(&format!("  digest: {digest}\n"));
            }
            if !r.stdout.trim().is_empty() {
                out.push_str(&format!("  stdout:\n{}\n", truncate(&r.stdout, 8000)));
            }
            if !r.stderr.trim().is_empty() {
                out.push_str(&format!("  stderr:\n{}\n", truncate(&r.stderr, 4000)));
            }
        }
    }

    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in s.char_indices() {
        if idx >= max {
            break;
        }
        out.push(ch);
    }
    out
}

fn insert_system_context_before_conversation(messages: &mut Vec<ChatMessage>, extra: ChatMessage) {
    // Keep the contract/system prompt first, but insert this near the top
    // (after any initial system messages like date/summary/brain).
    let mut idx = 0usize;
    while idx < messages.len() {
        if !matches!(messages[idx].role, eli_core::types::Role::System) {
            break;
        }
        idx += 1;
    }
    messages.insert(idx, extra);
}

fn discover_recent_research(project_root: &Path, max_items: usize) -> Vec<ResearchArtifact> {
    if max_items == 0 {
        return Vec::new();
    }

    let dir = project_root.join("eli_research");
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };

    #[derive(Clone)]
    struct Candidate {
        path: PathBuf,
        modified: std::time::SystemTime,
    }

    let mut files: Vec<Candidate> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.file_name().and_then(|s| s.to_str()) == Some("ELI.md") {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        files.push(Candidate { path, modified });
    }

    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    files.truncate(max_items);

    let mut out = Vec::new();
    for cand in files {
        let rel = cand
            .path
            .strip_prefix(project_root)
            .unwrap_or(&cand.path)
            .to_string_lossy()
            .to_string();

        let title = read_markdown_title(&cand.path).unwrap_or_else(|| {
            cand.path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("research")
                .to_string()
        });

        let created_utc = chrono::DateTime::<chrono::Utc>::from(cand.modified).to_rfc3339();

        out.push(ResearchArtifact {
            rel_path: rel,
            title,
            status: String::new(),
            created_utc,
            answer_hint: None,
        });
    }

    out
}

fn read_markdown_title(path: &Path) -> Option<String> {
    use std::io::Read;

    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    // Titles are at the top for Eli reports; keep this cheap.
    let mut reader = f.take(2048);
    reader.read_to_end(&mut buf).ok()?;
    let s = String::from_utf8_lossy(&buf);
    let first = s.lines().next()?.trim();
    let title = first.strip_prefix('#')?.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

fn is_slash_command_context(line: &str, pos: usize) -> bool {
    if pos != line.len() {
        return false;
    }
    if !line.starts_with('/') {
        return false;
    }
    if line.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let tail = line.get(1..).unwrap_or("");
    if tail.contains('/') {
        return false;
    }
    true
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    print!(
        "{}?{} {} {}(y/n):{} ",
        style::YELLOW,
        style::RESET,
        prompt,
        style::GRAY,
        style::RESET
    );
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("read confirm input")?;
    let v = input.trim().to_lowercase();
    Ok(v == "y" || v == "yes")
}

fn prompt_user(prompt: &str) -> Result<(String, Vec<String>)> {
    use std::io::Write;
    println!("\n{}?{} {}", style::CYAN, style::RESET, prompt);
    print!("{}›{} ", style::CYAN, style::RESET);
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("read input")?;
    Ok(process_input_for_images(input.trim()))
}

fn colorize_diff(diff: &str) -> String {
    use style::*;

    let mut out = String::new();
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            out.push_str(&format!("{}    {}{}\n", GREEN, line, RESET));
        } else if line.starts_with('-') && !line.starts_with("---") {
            out.push_str(&format!("{}    {}{}\n", RED, line, RESET));
        } else if line.starts_with("@@") {
            out.push_str(&format!("{}    {}{}\n", CYAN, line, RESET));
        } else if line.starts_with("+++") || line.starts_with("---") {
            out.push_str(&format!("{}    {}{}\n", GRAY, line, RESET));
        } else {
            out.push_str(&format!("    {}\n", line));
        }
    }
    out
}

fn diff_line_counts(diff: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut deleted = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deleted += 1;
        }
    }
    (added, deleted)
}

fn print_diff_results(results: &[DiffResult], preview: bool, brief: bool) {
    use style::*;

    if results.is_empty() {
        return;
    }
    if brief {
        let created = results.iter().filter(|r| r.op == "create").count();
        let modified = results
            .iter()
            .filter(|r| r.op == "replace" || r.op == "patch")
            .count();
        let deleted = results.iter().filter(|r| r.op == "delete").count();

        let mut parts = Vec::new();
        if created > 0 {
            parts.push(format!("{}+{} created{}", GREEN, created, RESET));
        }
        if modified > 0 {
            parts.push(format!("{}~{} modified{}", YELLOW, modified, RESET));
        }
        if deleted > 0 {
            parts.push(format!("{}-{} deleted{}", RED, deleted, RESET));
        }

        let status = if preview {
            format!("{}preview{}", GRAY, RESET)
        } else {
            format!("{}applied{}", GREEN, RESET)
        };
        let count = created + modified + deleted;
        let noun = if count == 1 { "file" } else { "files" };
        print_history_line(format!("edited {count} {noun} ({})", status));
        return;
    }

    let status = if preview { "preview" } else { "applied" };
    println!("{}◆{} diffs: {} ({})", PURPLE, RESET, results.len(), status);
    for r in results {
        let (icon, color) = if r.success {
            ("✓", GREEN)
        } else {
            ("✗", RED)
        };
        println!(
            "  {}{}{} {}{} {}{}{}: {}",
            color, icon, RESET, BLUE, r.op, RESET, WHITE, r.path, RESET,
        );
        if !r.message.is_empty() && r.message != "ok" {
            println!("    {}{}{}", GRAY, r.message, RESET);
        }
        if let Some(d) = &r.diff {
            let (added, deleted) = diff_line_counts(d);
            println!(
                "    LINE CODED ({}{}{} IN GREEN, {}{}{} IN RED)",
                GREEN, added, RESET, RED, deleted, RESET
            );
            println!("{}", colorize_diff(d));
        }
    }
}

fn print_command_results(results: &[CommandResult], brief: bool, full: bool) {
    use style::*;

    if results.is_empty() {
        return;
    }

    if brief {
        for r in results {
            let (icon, color) = if r.returncode == 0 {
                ("✓", GREEN)
            } else {
                ("✗", RED)
            };
            print_history_line(format!(
                "{}{}{} {}${} {}{}",
                color,
                icon,
                RESET,
                GRAY,
                RESET,
                truncate_line(&r.command, 70),
                RESET
            ));
            if r.returncode != 0 && !r.stderr.trim().is_empty() {
                print_history_line(format!(
                    "{}err:{} {}{}",
                    RED,
                    RESET,
                    truncate_line(&r.stderr.replace('\n', " "), 100),
                    RESET
                ));
            }
        }
        return;
    }

    println!("{}◆{} commands: {}", YELLOW, RESET, results.len());
    for r in results {
        let (icon, color) = if r.returncode == 0 {
            ("✓", GREEN)
        } else {
            ("✗", RED)
        };
        println!(
            "  {}{}{} {}${} {} {}{}ms{}",
            color, icon, RESET, GRAY, RESET, r.command, DARK_GRAY, r.duration_ms, RESET
        );
        if full {
            if !r.stdout.trim().is_empty() {
                println!("    {}stdout:{}{}", GRAY, RESET, RESET);
                for line in r.stdout.lines() {
                    println!("    {}{}{}", GRAY, line, RESET);
                }
            }
            if !r.stderr.trim().is_empty() {
                println!("    {}stderr:{}{}", RED, RESET, RESET);
                for line in r.stderr.lines() {
                    println!("    {}{}{}", RED, line, RESET);
                }
            }
        } else {
            if !r.stdout.trim().is_empty() {
                for line in r.stdout.lines().take(20) {
                    println!("    {}{}{}", GRAY, line, RESET);
                }
                if r.stdout.lines().count() > 20 {
                    println!(
                        "    {}... ({} more lines){}",
                        DARK_GRAY,
                        r.stdout.lines().count() - 20,
                        RESET
                    );
                }
            }
            if !r.stderr.trim().is_empty() {
                for line in r.stderr.lines().take(10) {
                    println!("    {}{}{}", RED, line, RESET);
                }
            }
        }
    }
}

fn print_tool_results_debug(results: &[CommandResult]) {
    if results.is_empty() {
        return;
    }

    println!("\n=== TOOL CALL RESULT ===");
    for (idx, r) in results.iter().enumerate() {
        if idx > 0 {
            println!("\n---");
        }
        println!("command: {}", r.command);
        println!("returncode: {}", r.returncode);
        if let Some(reason) = &r.deny_reason {
            println!("deny_reason: {}", reason);
        }
        println!("stdout:");
        print!("{}", r.stdout);
        if !r.stdout.ends_with('\n') {
            println!();
        }
        println!("stderr:");
        print!("{}", r.stderr);
        if !r.stderr.ends_with('\n') {
            println!();
        }
    }
    println!("=== END TOOL CALL RESULT ===");
}

async fn print_screen_results(actions: &[serde_json::Value]) {
    for action in actions {
        let Some(obj) = action.as_object() else {
            continue;
        };
        let Some(kind) = obj.get("action").and_then(|v| v.as_str()) else {
            continue;
        };
        match kind {
            "clipboard" => {
                if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                    let _ = eli_screen::run_action(eli_screen::ScreenAction::Clipboard {
                        text: text.to_string(),
                    })
                    .await;
                    println!("screen: clipboard ({} chars)", text.len());
                }
            }
            "focus_app" => {
                if let Some(name) = obj.get("app").and_then(|v| v.as_str()) {
                    let _ = eli_screen::run_action(eli_screen::ScreenAction::FocusApp {
                        name: name.to_string(),
                    })
                    .await;
                    println!("screen: focus_app {name}");
                }
            }
            other => println!("screen: skipped action {other}"),
        }
    }
}

fn parse_plan_controls(plan: &str) -> (Option<RunMode>, Option<ApprovalMode>) {
    let line = plan.lines().next().unwrap_or("");
    let mut mode = None;
    let mut approvals = None;

    for part in line.split('|').map(|p| p.trim()) {
        let lower = part.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("mode:") {
            let v = rest.trim();
            mode = match v {
                "read" => Some(RunMode::Read),
                "work" => Some(RunMode::Work),
                _ => None,
            };
        } else if let Some(rest) = lower.strip_prefix("approvals:") {
            let v = rest.trim();
            approvals = match v {
                "ask" => Some(ApprovalMode::Ask),
                "auto" => Some(ApprovalMode::Auto),
                _ => None,
            };
        }
    }

    (mode, approvals)
}

fn print_cost_stats(state: &SessionState, chat: &eli_core::config::ChatConfig) {
    use style::*;

    let usage = &state.total_usage;
    let cost = estimate_cost(usage, &chat.model);

    let lines = vec![
        format!("{}{}Cost & Usage{}", BOLD, CYAN, RESET),
        String::new(),
        format!(
            "{}total{} {} tokens  {}│{}  {}${}  {:.4}{}",
            GRAY, RESET, usage.total_tokens, DARK_GRAY, RESET, GREEN, RESET, cost, RESET
        ),
        format!(
            "{}      {} in         {} out",
            GRAY, usage.prompt_tokens, usage.completion_tokens
        ),
    ];

    if let Some(last) = &state.last_usage {
        let last_cost = estimate_cost(last, &chat.model);
        let mut extended = lines;
        extended.push(String::new());
        extended.push(format!(
            "{}last{}  {} tokens     {}${:.4}{}",
            GRAY, RESET, last.total_tokens, YELLOW, last_cost, RESET
        ));
        let out = format_indented_block(&extended);
        println!("{}", out);
    } else {
        let out = format_indented_block(&lines);
        println!("{}", out);
    }
}

fn estimate_cost(usage: &eli_core::types::Usage, model: &str) -> f64 {
    // Very rough estimation based on common OpenRouter/Anthropic pricing
    // Normalize model name
    let m = model.to_lowercase();
    let (input_rate, output_rate) = if m.contains("claude-3-5-sonnet") {
        (3.0, 15.0)
    } else if m.contains("claude-3-5-haiku") {
        (0.8, 4.0)
    } else if m.contains("claude-3-haiku") || m.contains("haiku") {
        (0.25, 1.25)
    } else if m.contains("claude-3-opus") || m.contains("opus") {
        (15.0, 75.0)
    } else if m.contains("gpt-4o-mini") {
        (0.15, 0.60)
    } else if m.contains("gpt-4o") {
        (2.5, 10.0)
    } else if m.contains("o1-mini") {
        (1.1, 4.4)
    } else if m.contains("o1") {
        (15.0, 60.0)
    } else if m.contains("o3-mini") {
        (1.1, 4.4)
    } else if m.contains("gpt-4-turbo") || m.contains("gpt-4") {
        (10.0, 30.0)
    } else if m.contains("deepseek") {
        (0.14, 0.28)
    } else if m.contains("gemini-1.5-flash") {
        (0.075, 0.3)
    } else if m.contains("gemini-1.5-pro") {
        (1.25, 5.0)
    } else if m.contains("llama-3.1-405b") || m.contains("llama-3.3-70b") {
        (1.0, 1.0) // Approx OpenRouter pricing for huge models
    } else if m.contains("llama") || m.contains("mistral") {
        (0.1, 0.1)
    } else if m.contains("devstral") || m.contains("moe") {
        (0.05, 0.22) // $0.22 per 1M output tokens as requested
    } else {
        (3.0, 15.0) // Default to Sonnet
    };

    let input_cost = (usage.prompt_tokens as f64 / 1_000_000.0) * input_rate;
    let output_cost = (usage.completion_tokens as f64 / 1_000_000.0) * output_rate;
    input_cost + output_cost
}
