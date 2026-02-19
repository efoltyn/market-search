fn flush_buffer(out: &mut std::io::Stdout, buf: &Buffer, rect: Rect, top: u16) {
    let mut current_style = Style::default();
    for y in 0..rect.height {
        queue!(out, cursor::MoveTo(0, top + y)).ok();
        for x in 0..rect.width {
            let cell = buf.get(x, y);
            let cell_style = cell.style();
            if cell_style != current_style {
                apply_style(out, cell_style);
                current_style = cell_style;
            }
            queue!(out, crossterm::style::Print(cell.symbol())).ok();
        }
        queue!(out, SetAttribute(Attribute::Reset), ResetColor).ok();
        current_style = Style::default();
    }
}

fn apply_style(out: &mut std::io::Stdout, style: Style) {
    queue!(out, SetAttribute(Attribute::Reset), ResetColor).ok();
    if let Some(fg) = style.fg {
        queue!(out, SetForegroundColor(map_color(fg))).ok();
    }
    if let Some(bg) = style.bg {
        queue!(out, SetBackgroundColor(map_color(bg))).ok();
    }
    let mods = style.add_modifier;
    if mods.contains(Modifier::BOLD) {
        queue!(out, SetAttribute(Attribute::Bold)).ok();
    }
    if mods.contains(Modifier::DIM) {
        queue!(out, SetAttribute(Attribute::Dim)).ok();
    }
    if mods.contains(Modifier::ITALIC) {
        queue!(out, SetAttribute(Attribute::Italic)).ok();
    }
    if mods.contains(Modifier::UNDERLINED) {
        queue!(out, SetAttribute(Attribute::Underlined)).ok();
    }
    if mods.contains(Modifier::REVERSED) {
        queue!(out, SetAttribute(Attribute::Reverse)).ok();
    }
    if mods.contains(Modifier::HIDDEN) {
        queue!(out, SetAttribute(Attribute::Hidden)).ok();
    }
    if mods.contains(Modifier::CROSSED_OUT) {
        queue!(out, SetAttribute(Attribute::CrossedOut)).ok();
    }
    if mods.contains(Modifier::SLOW_BLINK) {
        queue!(out, SetAttribute(Attribute::SlowBlink)).ok();
    }
    if mods.contains(Modifier::RAPID_BLINK) {
        queue!(out, SetAttribute(Attribute::RapidBlink)).ok();
    }
}

fn map_color(color: Color) -> crossterm::style::Color {
    match color {
        Color::Reset => crossterm::style::Color::Reset,
        Color::Black => crossterm::style::Color::Black,
        Color::Red => crossterm::style::Color::DarkRed,
        Color::Green => crossterm::style::Color::DarkGreen,
        Color::Yellow => crossterm::style::Color::DarkYellow,
        Color::Blue => crossterm::style::Color::DarkBlue,
        Color::Magenta => crossterm::style::Color::DarkMagenta,
        Color::Cyan => crossterm::style::Color::DarkCyan,
        Color::Gray => crossterm::style::Color::Grey,
        Color::DarkGray => crossterm::style::Color::DarkGrey,
        Color::LightRed => crossterm::style::Color::Red,
        Color::LightGreen => crossterm::style::Color::Green,
        Color::LightYellow => crossterm::style::Color::Yellow,
        Color::LightBlue => crossterm::style::Color::Blue,
        Color::LightMagenta => crossterm::style::Color::Magenta,
        Color::LightCyan => crossterm::style::Color::Cyan,
        Color::White => crossterm::style::Color::White,
        Color::Indexed(idx) => crossterm::style::Color::AnsiValue(idx),
        Color::Rgb(r, g, b) => crossterm::style::Color::Rgb { r, g, b },
    }
}

fn footer_title(
    phase: &str,
    spinner_idx: usize,
    queue_len: usize,
    elapsed: Duration,
    total_tokens: u32,
    mode: Option<PromptMode>,
) -> String {
    let spinner = FOOTER_SPINNER[spinner_idx % FOOTER_SPINNER.len()];
    let queue_indicator = if queue_len > 0 {
        format!(" [{}Q]", queue_len)
    } else {
        String::new()
    };
    let mode_chip = match mode {
        Some(PromptMode::Ask) => " [ASK]",
        Some(PromptMode::Plan) => " [PLAN]",
        Some(PromptMode::Auto) => " [AUTO]",
        None => "",
    };
    format!(
        "{spinner} {phase}{queue_indicator}{mode_chip} [{}s] {total_tokens} tokens",
        elapsed.as_secs()
    )
}

fn render_footer(
    footer: &mut Option<FooterUi>,
    phase: &str,
    spinner_idx: usize,
    elapsed: Duration,
    state: &SessionState,
    mode: Option<PromptMode>,
) {
    if footer.is_none() {
        *footer = Some(FooterUi::enable());
    }
    if let Some(footer) = footer.as_mut() {
        let title = footer_title(
            phase,
            spinner_idx,
            state.queue_len(),
            elapsed,
            state.total_usage.total_tokens,
            mode,
        );
        footer.render(&title, &state.input_buffer, state.cursor_pos);
    }
}

fn drain_run_key_events(
    state: &mut SessionState,
    interrupted: &mut bool,
    interrupted_by_esc: &mut bool,
) -> bool {
    let mut changed = false;
    while ct_event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let Ok(ev) = ct_event::read() else {
            continue;
        };
        match ev {
            CtEvent::Resize(_, _) => {
                changed = true;
            }
            CtEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    CtKeyCode::Char(c) => {
                        state.input_buffer.insert(state.cursor_pos, c);
                        state.cursor_pos += 1;
                        changed = true;
                    }
                    CtKeyCode::Backspace => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            state.input_buffer.remove(state.cursor_pos);
                            changed = true;
                        }
                    }
                    CtKeyCode::Delete => {
                        if state.cursor_pos < state.input_buffer.len() {
                            state.input_buffer.remove(state.cursor_pos);
                            changed = true;
                        }
                    }
                    CtKeyCode::Left => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            changed = true;
                        }
                    }
                    CtKeyCode::Right => {
                        if state.cursor_pos < state.input_buffer.len() {
                            state.cursor_pos += 1;
                            changed = true;
                        }
                    }
                    CtKeyCode::Home => {
                        state.cursor_pos = 0;
                        changed = true;
                    }
                    CtKeyCode::End => {
                        state.cursor_pos = state.input_buffer.len();
                        changed = true;
                    }
                    CtKeyCode::Enter => {
                        let trimmed = state.input_buffer.trim().to_string();
                        if !trimmed.is_empty() {
                            if trimmed == "/stop" || trimmed == "/interrupt" {
                                *interrupted = true;
                                state.input_buffer.clear();
                                state.cursor_pos = 0;
                                changed = true;
                                break;
                            }
                            print_history_line(format!(
                                "{}›{} {}",
                                style::CYAN,
                                style::RESET,
                                trimmed
                            ));
                            state.queue_prompt(trimmed.clone());
                            state.prompt_history.push(trimmed);
                            state.input_buffer.clear();
                            state.cursor_pos = 0;
                            changed = true;
                        }
                    }
                    CtKeyCode::Esc => {
                        *interrupted = true;
                        *interrupted_by_esc = true;
                        changed = true;
                        break;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    changed
}

fn drain_run_key_events_queue_only(state: &mut SessionState) -> bool {
    let mut changed = false;
    while ct_event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let Ok(ev) = ct_event::read() else {
            continue;
        };
        match ev {
            CtEvent::Resize(_, _) => {
                changed = true;
            }
            CtEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    CtKeyCode::Char(c) => {
                        state.input_buffer.insert(state.cursor_pos, c);
                        state.cursor_pos += 1;
                        changed = true;
                    }
                    CtKeyCode::Backspace => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            state.input_buffer.remove(state.cursor_pos);
                            changed = true;
                        }
                    }
                    CtKeyCode::Delete => {
                        if state.cursor_pos < state.input_buffer.len() {
                            state.input_buffer.remove(state.cursor_pos);
                            changed = true;
                        }
                    }
                    CtKeyCode::Left => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            changed = true;
                        }
                    }
                    CtKeyCode::Right => {
                        if state.cursor_pos < state.input_buffer.len() {
                            state.cursor_pos += 1;
                            changed = true;
                        }
                    }
                    CtKeyCode::Home => {
                        state.cursor_pos = 0;
                        changed = true;
                    }
                    CtKeyCode::End => {
                        state.cursor_pos = state.input_buffer.len();
                        changed = true;
                    }
                    CtKeyCode::Enter => {
                        let trimmed = state.input_buffer.trim().to_string();
                        if !trimmed.is_empty() {
                            print_history_line(format!(
                                "{}›{} {}",
                                style::CYAN,
                                style::RESET,
                                trimmed
                            ));
                            state.queue_prompt(trimmed.clone());
                            state.prompt_history.push(trimmed);
                            state.input_buffer.clear();
                            state.cursor_pos = 0;
                            changed = true;
                        }
                    }
                    CtKeyCode::Esc => {
                        state.input_buffer.clear();
                        state.cursor_pos = 0;
                        changed = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    changed
}

fn render_ratatui_panel(title: &str, body: &str) -> String {
    let (width, _) = terminal_size();
    let width = width.min(140).max(20);
    let inner_width = width.saturating_sub(2).max(1);
    let wrapped = wrap(body, WrapOptions::new(inner_width));
    let height = wrapped.len().saturating_add(2).max(3);
    let rect = Rect::new(0, 0, width as u16, height as u16);
    let mut buf = Buffer::empty(rect);
    let paragraph = Paragraph::new(wrapped.join("\n"))
        .block(Block::default().title(title).borders(Borders::ALL));
    paragraph.render(rect, &mut buf);
    buffer_to_lines(buf, rect).join("\n")
}

fn buffer_to_lines(buf: Buffer, rect: Rect) -> Vec<String> {
    let mut lines = Vec::new();
    for y in 0..rect.height {
        let mut line = String::new();
        for x in 0..rect.width {
            let cell = buf.get(x, y);
            line.push_str(cell.symbol());
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Strip ANSI escape sequences for length calculation
fn strip_ansi(s: &str) -> String {
    let mut result = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            result.push(c);
            continue;
        }

        // Escape sequence.
        match it.next() {
            Some('[') => {
                // CSI: ESC [ ... <final byte>
                while let Some(ch) = it.next() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: ESC ] ... BEL | ESC \
                while let Some(ch) = it.next() {
                    if ch == '\x07' {
                        break;
                    }
                    if ch == '\x1b' {
                        if let Some('\\') = it.peek().copied() {
                            let _ = it.next();
                            break;
                        }
                    }
                }
            }
            Some(_) | None => {}
        }
    }
    result
}

#[allow(dead_code)]
fn print_box(lines: &[String]) {
    let out = format_box_string(lines);
    if !out.is_empty() {
        println!("{out}");
    }
}

fn truncate_line(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    input.chars().take(max).collect()
}

fn truncate_middle(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    let total = max;
    let head_len = total / 2;
    let tail_len = total - head_len;
    let head: String = input.chars().take(head_len).collect();
    let tail: String = input
        .chars()
        .rev()
        .take(tail_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{}{}", head, tail)
}

fn format_root_path(path: &Path) -> String {
    let mut out = path.display().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if out.starts_with(&home) {
            out = out.replacen(&home, "~", 1);
        }
    }
    truncate_middle(&out, 70)
}

fn terminal_size() -> (usize, usize) {
    let term = ConsoleTerm::stdout();
    let (rows, cols) = term.size();
    let width = cols.max(1) as usize;
    let height = rows.max(1) as usize;
    (width, height)
}

fn format_mode(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Read => "read",
        RunMode::Work => "work",
    }
}

fn format_approvals(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
    }
}

fn format_approvals_display(chat: &eli_core::config::ChatConfig) -> String {
    let cmds = chat.resolved_command_approvals();
    let diffs = chat.resolved_diff_approvals();
    if cmds == diffs {
        return format_approvals(cmds).to_string();
    }
    format!(
        "cmd:{} diff:{}",
        format_approvals(cmds),
        format_approvals(diffs)
    )
}

fn parse_bool(val: &str) -> Result<bool> {
    match val.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => anyhow::bail!("invalid boolean value: {other}"),
    }
}

async fn run_commands_with_policy(
    profile: AgentProfile,
    command_runner: &CommandRunner,
    commands: &[String],
    parallelism: usize,
) -> Vec<CommandResult> {
    let _ = profile;
    command_runner
        .run_commands_with_parallelism(commands, parallelism)
        .await
}

fn shadow_large_tool_outputs(
    project_root: &Path,
    session_id: &str,
    step: u32,
    results: &[CommandResult],
) -> Vec<CommandResult> {
    const MAX_INLINE_JSON_BYTES: usize = 2 * 1024;
    const MAX_TOTAL_BYTES: u64 = 1_000_000_000;
    const MAX_AGE_DAYS: i64 = 45;
    const MAX_FILES_PER_SESSION: usize = 200;

    let last_output_path = project_root
        .join("eli_research")
        .join("data")
        .join(".last_tool_output.json");
    let rel_last_output_path = last_output_path
        .strip_prefix(project_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| last_output_path.display().to_string());
    let tool_output_root = project_root
        .join("eli_research")
        .join("data")
        .join("tool_outputs");
    let index_path = tool_output_root.join("index.jsonl");

    let mut out = Vec::with_capacity(results.len());
    for (idx, r) in results.iter().enumerate() {
        let mut rr = r.clone();

        let cmd0 = rr
            .command
            .trim_start()
            .split_whitespace()
            .next()
            .unwrap_or("");
        let is_eli = cmd0 == "eli" || cmd0.ends_with("/eli") || cmd0.ends_with("\\eli");
        if !is_eli || !rr.allowed || rr.returncode != 0 {
            out.push(rr);
            continue;
        }

        if is_suppression_exempt(&rr.command) {
            out.push(rr);
            continue;
        }

        let stdout = rr.stdout.trim();
        if stdout.as_bytes().len() <= MAX_INLINE_JSON_BYTES {
            out.push(rr);
            continue;
        }
        let Some(value) = extract_json_from_stdout(stdout) else {
            out.push(rr);
            continue;
        };

        if let Some(parent) = last_output_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                rr.stderr = format!(
                    "{}\n(data shadowing: failed to create dir '{}': {e})",
                    rr.stderr.trim_end(),
                    parent.display()
                )
                .trim()
                .to_string();
                out.push(rr);
                continue;
            }
        }

        if let Err(e) = std::fs::create_dir_all(&tool_output_root) {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to create dir '{}': {e})",
                rr.stderr.trim_end(),
                tool_output_root.display()
            )
            .trim()
            .to_string();
            out.push(rr);
            continue;
        }

        let json = serde_json::to_string_pretty(&value).unwrap_or_else(|_| stdout.to_string());
        let session_dir = tool_output_root.join(session_id);
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to create dir '{}': {e})",
                rr.stderr.trim_end(),
                session_dir.display()
            )
            .trim()
            .to_string();
            out.push(rr);
            continue;
        }

        let (tool_name, parsed_args) = parse_tool_name_and_arg_pairs_from_command(&rr.command);
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
        let sha256 = sha256_hex(json.as_bytes());
        let stem = build_programmatic_dataset_stem(&tool_name, &value, &parsed_args, &stamp);
        let mut archive_path = session_dir.join(format!("{stem}.json"));
        if archive_path.exists() {
            archive_path = session_dir.join(format!("{stem}_S{step:03}_I{idx:02}.json"));
        }
        let rel_archive_path = archive_path
            .strip_prefix(project_root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| archive_path.display().to_string());

        let archive_ok = match std::fs::write(&archive_path, &json) {
            Ok(()) => true,
            Err(e) => {
                rr.stderr = format!(
                    "{}\n(data shadowing: failed to write '{}': {e})",
                    rr.stderr.trim_end(),
                    rel_archive_path
                )
                .trim()
                .to_string();
                false
            }
        };
        if archive_ok {
            if let Err(e) =
                write_shadow_meta_for_value(&archive_path, &value, "tool.shadow", &rr.command)
            {
                rr.stderr = format!(
                    "{}\n(data shadowing: failed to write meta for '{}': {e})",
                    rr.stderr.trim_end(),
                    rel_archive_path
                )
                .trim()
                .to_string();
            }
        }

        if let Err(e) = std::fs::write(&last_output_path, &json) {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to write '{}': {e})",
                rr.stderr.trim_end(),
                rel_last_output_path
            )
            .trim()
            .to_string();
        } else if let Err(e) =
            write_shadow_meta_for_value(&last_output_path, &value, "tool.shadow", &rr.command)
        {
            rr.stderr = format!(
                "{}\n(data shadowing: failed to write meta for '{}': {e})",
                rr.stderr.trim_end(),
                rel_last_output_path
            )
            .trim()
            .to_string();
        }

        let bytes = json.as_bytes().len();
        if archive_ok {
            let meta = serde_json::json!({
                "created_at": chrono::Utc::now().to_rfc3339(),
                "session_id": session_id,
                "step": step,
                "command_index": idx,
                "command": rr.command,
                "path": rel_archive_path,
                "latest_path": rel_last_output_path,
                "bytes": bytes,
                "sha256": sha256,
            });
            if let Ok(line) = serde_json::to_string(&meta) {
                if let Err(e) = append_line(&index_path, &line) {
                    rr.stderr = format!(
                        "{}\n(data shadowing: failed to append index '{}': {e})",
                        rr.stderr.trim_end(),
                        index_path.display()
                    )
                    .trim()
                    .to_string();
                }
            }
            if let Err(e) = prune_tool_outputs(
                &tool_output_root,
                MAX_TOTAL_BYTES,
                MAX_AGE_DAYS,
                MAX_FILES_PER_SESSION,
            ) {
                rr.stderr = format!(
                    "{}\n(data shadowing: prune failed: {e})",
                    rr.stderr.trim_end(),
                )
                .trim()
                .to_string()
            }
        }

        let points = count_data_points(&value);
        let summary = format_suppressed_summary(&rr.command, &value, 8, 160);
        let hint =
            "More detail is available in the saved file; inspect with local tools if needed.";
        let archive_fragment = if archive_ok {
            format!("; saved_copy={rel_archive_path}")
        } else {
            String::new()
        };
        rr.stdout = format!(
            "[OUTPUT SUPPRESSED] saved_to={rel_last_output_path} ({bytes} bytes){archive_fragment}. Data points: {points}.\n[SUMMARY]\n{summary}\n{hint}"
        );
        out.push(rr);
    }

    out
}

fn extract_json_from_stdout(stdout: &str) -> Option<serde_json::Value> {
    if stdout.starts_with('{') || stdout.starts_with('[') {
        return serde_json::from_str(stdout).ok();
    }

    let first_obj = stdout.find('{');
    let first_arr = stdout.find('[');
    let start = match (first_obj, first_arr) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    serde_json::from_str(&stdout[start..]).ok()
}

fn parse_tool_name_and_arg_pairs_from_command(command: &str) -> (String, Vec<String>) {
    let toks = command.split_whitespace().collect::<Vec<_>>();
    if toks.is_empty() {
        return ("tool.shadow".to_string(), Vec::new());
    }
    let first = toks[0];
    let is_eli = first == "eli" || first.ends_with("/eli") || first.ends_with("\\eli");
    let tool_name =
        if is_eli && toks.len() >= 3 && !toks[1].starts_with('-') && !toks[2].starts_with('-') {
            format!("{}.{}", toks[1], toks[2])
        } else {
            "tool.shadow".to_string()
        };

    let mut args = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let tok = toks[i];
        if let Some(rest) = tok.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                let key = normalize_name_token(k, false, 48).to_ascii_lowercase();
                if !key.is_empty() && !v.trim().is_empty() {
                    args.push(format!("{key}={v}"));
                }
            } else {
                let key = normalize_name_token(rest, false, 48).to_ascii_lowercase();
                let next = toks.get(i + 1).copied();
                if let Some(v) = next {
                    if !v.starts_with('-') {
                        args.push(format!("{key}={v}"));
                        i += 1;
                    } else {
                        args.push(format!("{key}=true"));
                    }
                } else {
                    args.push(format!("{key}=true"));
                }
            }
        }
        i += 1;
    }
    (tool_name, args)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

fn prune_tool_outputs(
    tool_output_root: &Path,
    max_total_bytes: u64,
    max_age_days: i64,
    max_files_per_session: usize,
) -> Result<()> {
    #[derive(Clone)]
    struct Entry {
        path: PathBuf,
        modified: std::time::SystemTime,
        size: u64,
        session: String,
    }

    if !tool_output_root.exists() {
        return Ok(());
    }

    let now = chrono::Utc::now();
    let expiry = now - chrono::Duration::days(max_age_days);
    let mut entries: Vec<Entry> = Vec::new();

    let sessions = match std::fs::read_dir(tool_output_root) {
        Ok(rd) => rd,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "read_dir {}: {e}",
                tool_output_root.display()
            ));
        }
    };

    for session_entry in sessions {
        let session_entry = match session_entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let session_path = session_entry.path();
        if !session_path.is_dir() {
            continue;
        }
        let session = session_entry.file_name().to_string_lossy().to_string();
        let files = match std::fs::read_dir(&session_path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for file in files {
            let Ok(file) = file else { continue };
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let modified_dt: chrono::DateTime<chrono::Utc> = modified.into();
            if modified_dt < expiry {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            entries.push(Entry {
                path,
                modified,
                size: meta.len(),
                session: session.clone(),
            });
        }
    }

    // Per-session cap: keep newest N.
    let mut by_session: std::collections::HashMap<String, Vec<Entry>> =
        std::collections::HashMap::new();
    for entry in entries {
        by_session
            .entry(entry.session.clone())
            .or_default()
            .push(entry);
    }

    let mut remaining: Vec<Entry> = Vec::new();
    for (_session, mut files) in by_session {
        files.sort_by_key(|e| std::cmp::Reverse(e.modified));
        for (idx, entry) in files.into_iter().enumerate() {
            if idx < max_files_per_session {
                remaining.push(entry);
            } else {
                let _ = std::fs::remove_file(&entry.path);
            }
        }
    }

    // Global cap: delete oldest until size <= max_total_bytes.
    let mut total: u64 = remaining.iter().map(|e| e.size).sum();
    if total > max_total_bytes {
        remaining.sort_by_key(|e| e.modified);
        for entry in remaining {
            if total <= max_total_bytes {
                break;
            }
            if std::fs::remove_file(&entry.path).is_ok() {
                total = total.saturating_sub(entry.size);
            }
        }
    }

    // Remove empty session dirs.
    if let Ok(sessions) = std::fs::read_dir(tool_output_root) {
        for session in sessions.flatten() {
            let p = session.path();
            if p.is_dir() {
                let is_empty = std::fs::read_dir(&p)
                    .ok()
                    .and_then(|mut rd| rd.next())
                    .is_none();
                if is_empty {
                    let _ = std::fs::remove_dir(&p);
                }
            }
        }
    }

    Ok(())
}

fn format_suppressed_summary(
    command: &str,
    value: &serde_json::Value,
    max_lines: usize,
    max_field_len: usize,
) -> String {
    fn trunc(s: String, max_len: usize) -> String {
        if s.chars().count() <= max_len {
            return s;
        }
        let mut out: String = s.chars().take(max_len).collect();
        out.push('…');
        out
    }

    fn list_sample(items: Vec<String>, max_items: usize, max_len: usize) -> String {
        let mut out = items
            .into_iter()
            .filter(|s| !s.is_empty())
            .take(max_items)
            .map(|s| trunc(s, max_len))
            .collect::<Vec<_>>()
            .join(", ");
        if out.is_empty() {
            out = "n/a".to_string();
        }
        out
    }

    let mut lines: Vec<String> = Vec::new();

    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
            keys.sort();
            lines.push(format!("top_level_keys: {}", keys.join(", ")));

            if let Some(provider) = map.get("provider").and_then(|v| v.as_str()) {
                lines.push(format!("provider: {provider}"));
            }

            if let Some(tickers) = map.get("tickers").and_then(|v| v.as_array()) {
                let tickers = tickers
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>();
                if !tickers.is_empty() {
                    lines.push(format!(
                        "tickers: {}",
                        list_sample(tickers, 10, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("available_events").and_then(|v| v.as_array()) {
                lines.push(format!("available_events: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let title = v.get("title").and_then(|s| s.as_str());
                        let ticker = v.get("ticker").and_then(|s| s.as_str());
                        match (ticker, title) {
                            (Some(t), Some(tt)) => Some(format!("{t}: {tt}")),
                            (None, Some(tt)) => Some(tt.to_string()),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "event_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("available_tags").and_then(|v| v.as_array()) {
                lines.push(format!("available_tags: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let label = v.get("label").and_then(|s| s.as_str());
                        let slug = v.get("slug").and_then(|s| s.as_str());
                        let id = v.get("id").and_then(|s| s.as_str());
                        match (label, slug, id) {
                            (Some(l), _, _) => Some(l.to_string()),
                            (None, Some(s), _) => Some(s.to_string()),
                            (None, None, Some(i)) => Some(i.to_string()),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "tag_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("markets").and_then(|v| v.as_array()) {
                lines.push(format!("markets: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let title = v.get("title").and_then(|s| s.as_str());
                        let ticker = v.get("ticker").and_then(|s| s.as_str());
                        match (ticker, title) {
                            (Some(t), Some(tt)) => Some(format!("{t}: {tt}")),
                            (None, Some(tt)) => Some(tt.to_string()),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "market_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("series").and_then(|v| v.as_array()) {
                let mut tickers = Vec::new();
                let mut total_points = 0usize;
                for s in arr {
                    if let Some(t) = s.get("ticker").and_then(|v| v.as_str()) {
                        tickers.push(t.to_string());
                    }
                    if let Some(candles) = s.get("candles").and_then(|v| v.as_array()) {
                        total_points += candles.len();
                    }
                }
                lines.push(format!("series: {}", arr.len()));
                if !tickers.is_empty() {
                    lines.push(format!(
                        "series_tickers: {}",
                        list_sample(tickers, 10, max_field_len)
                    ));
                }
                if total_points > 0 {
                    lines.push(format!("series_points: {total_points}"));
                }
            }

            if let Some(arr) = map.get("snapshots").and_then(|v| v.as_array()) {
                lines.push(format!("snapshots: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let t = v.get("ticker").and_then(|s| s.as_str())?;
                        let p = v.get("current_price").and_then(|s| s.as_f64());
                        Some(match p {
                            Some(px) => format!("{t}={px:.2}"),
                            None => t.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "snapshot_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("prices").and_then(|v| v.as_array()) {
                lines.push(format!("prices: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let sym = v.get("symbol").and_then(|s| s.as_str())?;
                        let val = v.get("value").and_then(|s| s.as_f64());
                        Some(match val {
                            Some(px) => format!("{sym}={px:.4}"),
                            None => sym.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "price_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("filings").and_then(|v| v.as_array()) {
                lines.push(format!("filings: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let form = v.get("form").and_then(|s| s.as_str())?;
                        let date = v.get("filing_date").and_then(|s| s.as_str());
                        Some(match date {
                            Some(d) => format!("{form} ({d})"),
                            None => form.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "filing_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(arr) = map.get("indicators").and_then(|v| v.as_array()) {
                lines.push(format!("indicators: {}", arr.len()));
                let sample = arr
                    .iter()
                    .take(3)
                    .filter_map(|v| {
                        let sym = v.get("symbol").and_then(|s| s.as_str())?;
                        let val = v.get("current_value").and_then(|s| s.as_f64());
                        Some(match val {
                            Some(px) => format!("{sym}={px:.3}"),
                            None => sym.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                if !sample.is_empty() {
                    lines.push(format!(
                        "indicator_samples: {}",
                        list_sample(sample, 3, max_field_len)
                    ));
                }
            }

            if let Some(data) = map.get("data") {
                if let serde_json::Value::Object(data_obj) = data {
                    let mut child_keys: Vec<&str> = data_obj.keys().map(|k| k.as_str()).collect();
                    child_keys.sort();
                    lines.push(format!("data_keys: {}", child_keys.join(", ")));
                    for key in child_keys.iter().take(4) {
                        if let Some(arr) = data_obj.get(*key).and_then(|v| v.as_array()) {
                            lines.push(format!("data.{key}: {}", arr.len()));
                        }
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            lines.push(format!("top_level: array (len={})", arr.len()));
        }
        _ => {
            lines.push("top_level: scalar".to_string());
        }
    }

    let mut merged = command_summary_lines(command, value, 4);
    merged.extend(schema_pattern_summary_parts(value));
    merged.extend(lines);
    let trimmed = merged.into_iter().take(max_lines).collect::<Vec<_>>();
    trimmed
        .into_iter()
        .map(|l| format!("- {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
