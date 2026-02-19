async fn cmd_chat(
    provider: Option<String>,
    model: Option<String>,
    display_override: Option<DisplayMode>,
) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let mut cfg = config::load_or_create(&paths).context("load/create config")?;
    apply_overrides(&mut cfg, provider, model)?;
    ensure_tui_default_model(&mut cfg.chat);
    cfg.chat.approvals = ApprovalMode::Auto;
    cfg.chat.approvals_commands = None;
    cfg.chat.approvals_diffs = None;
    cfg.chat.auto_mode = AutoMode::Autonomous;
    if let Some(mode) = display_override {
        cfg.chat.display_mode = mode;
    }

    let adapter = eli_adapters::build_from_chat_config(&cfg.chat).context("build adapter")?;
    let mut adapter: Arc<dyn LlmAdapter> = Arc::from(adapter);

    let cwd = std::env::current_dir().context("get cwd")?;
    let project_root = cfg
        .chat
        .resolved_project_root(&cwd)
        .map_err(|e| anyhow::anyhow!(e))
        .context("resolve project root")?;

    let diff_engine = DiffEngine::new(project_root.clone()).context("init diff engine")?;
    let command_runner = CommandRunner::new(
        cfg.chat.timeout_secs,
        cfg.chat.max_cmds,
        cfg.chat.parallel_commands,
        project_root.clone(),
    );

    let store = SessionStore::new(&paths);
    let session_id = uuid::Uuid::new_v4().to_string();
    info!(session_id = %session_id, provider = %cfg.chat.provider, model = %cfg.chat.model, "starting chat");

    let rl_config = Config::builder()
        .completion_type(CompletionType::Circular)
        .build();
    let shared_input_tokens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut editor: Editor<SlashHelper, DefaultHistory> =
        Editor::with_config(rl_config).context("init readline")?;
    editor.set_helper(Some(SlashHelper {
        last_input_tokens: shared_input_tokens.clone(),
    }));
    let slash_menu = SlashMenu::new();
    editor.bind_sequence(
        KeyEvent::from('/'),
        EventHandler::Conditional(Box::new(SlashMenuHandler::new(slash_menu.clone()))),
    );
    editor.bind_sequence(
        KeyEvent(KeyCode::Down, Modifiers::NONE),
        EventHandler::Conditional(Box::new(SlashNavHandler::new(
            slash_menu.clone(),
            SlashNav::Next,
        ))),
    );
    editor.bind_sequence(
        KeyEvent(KeyCode::Up, Modifiers::NONE),
        EventHandler::Conditional(Box::new(SlashNavHandler::new(
            slash_menu.clone(),
            SlashNav::Prev,
        ))),
    );
    let mut memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
    memory.set_system(eli_core::contract::system_prompt());
    ensure_eli_research_brain(&project_root).context("ensure eli_research/ELI.md")?;
    let mut undo_stack: Vec<Vec<DiffResult>> = Vec::new();
    let mut state = SessionState::new(&cfg.chat);
    state.load_recent_research(&project_root, 12);
    let force_plain_prompt = matches!(cfg.chat.display_mode, DisplayMode::Debug);

    // Use TUI mode for Standard display mode (alternate screen, no ghost issues)
    if matches!(cfg.chat.display_mode, DisplayMode::Standard) {
        return run_chat_tui(
            &mut cfg,
            adapter,
            &diff_engine,
            &command_runner,
            &store,
            &paths,
            &session_id,
            &project_root,
            &mut memory,
            &mut undo_stack,
        )
        .await;
    }

    // Non-TUI modes (Brain/Debug/Raw) use the old CLI approach
    if has_interactive_terminal() {
        print_banner(&cfg.chat, &project_root, &state);
    }

    loop {
        // Show queue status if there are queued prompts
        let queue_len = state.prompt_queue.len();

        // Update token hint for the upcoming prompt
        if let Some(usage) = &state.last_usage {
            shared_input_tokens.store(
                usage.prompt_tokens as usize,
                std::sync::atomic::Ordering::Relaxed,
            );
        }

        let (line, from_boxed_prompt) = if let Some(queued) = state.next_prompt() {
            print_history_line(format!("{}›{} {}", style::CYAN, style::RESET, queued));
            (queued, false)
        } else if matches!(state.display_mode, DisplayMode::Standard) && !force_plain_prompt {
            let Some(line) =
                read_line_boxed(&mut state, &mut cfg.chat, queue_len).context("boxed prompt")?
            else {
                break;
            };
            (line, true)
        } else {
            let prompt_prefix = if force_plain_prompt {
                "› ".to_string()
            } else if queue_len > 0 {
                format!("[{}Q] › ", queue_len)
            } else {
                "› ".to_string()
            };

            slash_menu.reset();

            let res = editor.readline_with_initial(&prompt_prefix, (&state.input_buffer, ""));
            state.input_buffer.clear();

            let line = match res {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    println!();
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e).context("readline failed"),
            };
            (line, false)
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if from_boxed_prompt {
            print_history_line(format!("{}›{} {}", style::CYAN, style::RESET, trimmed));
        }
        state.prompt_history.push(trimmed.to_string());
        editor.add_history_entry(trimmed).ok();

        // Slash commands
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }
        if trimmed == "/queue" || trimmed == "/q" {
            if state.prompt_queue.is_empty() {
                println!("(queue empty)");
            } else {
                println!("Queue:");
                for (i, p) in state.prompt_queue.iter().enumerate() {
                    println!("  {}. {}", i + 1, p);
                }
            }
            continue;
        }
        if trimmed.starts_with("/q ") || trimmed.starts_with("/queue ") {
            let rest = trimmed.splitn(2, ' ').nth(1).unwrap_or("");
            if !rest.is_empty() {
                state.queue_prompt(rest.to_string());
                println!("(added to queue: position {})", state.queue_len());
            }
            continue;
        }
        if trimmed == "/clear-queue" || trimmed == "/cq" {
            state.prompt_queue.clear();
            println!("(queue cleared)");
            continue;
        }
        if trimmed == "/compact" || trimmed == "/memory compact" {
            match compact_memory_now(adapter.clone(), &cfg.chat, &mut memory).await {
                Ok(Some(compaction)) => {
                    let note = format!(
                        "memory_compaction: dropped {} messages\n{}",
                        compaction.dropped, compaction.summary
                    );
                    let brain_entry = format!(
                        "\n### {} (session {})\n{}\n",
                        chrono::Utc::now().to_rfc3339(),
                        session_id,
                        note
                    );
                    if let Err(e) = append_eli_brain(&project_root, &brain_entry) {
                        println!("(compacted, but failed to write brain: {e})");
                    } else {
                        println!("memory: compacted ({} msgs)", compaction.dropped);
                    }
                    store
                        .append(
                            &session_id,
                            &SessionEvent {
                                ts: chrono::Utc::now(),
                                kind: EventKind::Note { content: note },
                            },
                        )
                        .await
                        .ok();
                }
                Ok(None) => println!("(nothing to compact)"),
                Err(e) => println!("(compact failed: {e})"),
            }
            continue;
        }
        if trimmed == "/tip" {
            println!("(tips are only shown in standard TUI mode)");
            continue;
        }
        if trimmed == "/clear"
            || trimmed == "/reset"
            || trimmed == "/new"
            || trimmed == "/memory clear"
        {
            memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
            memory.set_system(eli_core::contract::system_prompt());
            ensure_eli_research_brain(&project_root).ok();
            state.total_work_time = Duration::ZERO;
            state.step_count = 0;
            state.total_usage = eli_core::types::Usage::default();
            state.last_usage = None;
            println!("(cleared)");
            continue;
        }
        if trimmed == "/brain" {
            state.display_mode = DisplayMode::Brain;
            println!("(brain mode: full output)");
            continue;
        }
        if trimmed == "/debug" {
            state.display_mode = DisplayMode::Debug;
            println!("(debug mode: raw request/response + tool output + observation)");
            continue;
        }
        if trimmed == "/standard" || trimmed == "/brief" {
            state.display_mode = DisplayMode::Standard;
            println!("(standard mode: brief output)");
            continue;
        }
        if trimmed == "/read" {
            cfg.chat.mode = RunMode::Read;
            println!("(exec mode: read)");
            continue;
        }
        if trimmed == "/work" {
            cfg.chat.mode = RunMode::Work;
            println!("(exec mode: work)");
            continue;
        }
        if trimmed == "/bot" {
            cfg.chat.mode = RunMode::Work;
            cfg.chat.approvals = ApprovalMode::Auto;
            cfg.chat.approvals_commands = Some(ApprovalMode::Auto);
            cfg.chat.approvals_diffs = Some(ApprovalMode::Ask);
            println!(
                "(bot: exec=work, approvals={})",
                format_approvals_display(&cfg.chat)
            );
            continue;
        }
        if trimmed == "/yolo" {
            cfg.chat.mode = RunMode::Work;
            cfg.chat.approvals = ApprovalMode::Auto;
            cfg.chat.approvals_commands = None;
            cfg.chat.approvals_diffs = None;
            println!(
                "(yolo: exec=work, approvals={})",
                format_approvals_display(&cfg.chat)
            );
            continue;
        }
        if trimmed == "/mode" || trimmed.starts_with("/mode ") {
            let mode = trimmed
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_ascii_lowercase();
            if mode.is_empty() {
                println!("exec mode: {}", format_mode(cfg.chat.mode));
            } else if mode == "read" {
                cfg.chat.mode = RunMode::Read;
                println!("(exec mode: read)");
            } else if mode == "work" {
                cfg.chat.mode = RunMode::Work;
                println!("(exec mode: work)");
            } else {
                println!("(mode must be read or work)");
            }
            continue;
        }
        if trimmed == "/model" || trimmed.starts_with("/model ") {
            let model = trimmed.strip_prefix("/model").unwrap_or("").trim();
            if model.is_empty() {
                print_history_block(vec![format!("model: {}", cfg.chat.model)]);
            } else {
                cfg.chat.model = model.to_string();
                print_history_block(vec![format!("(model: {})", cfg.chat.model)]);
            }
            continue;
        }
        if trimmed == "/models" {
            print_history_block(vec![
                format!("model: {}", cfg.chat.model),
                "set with: /model <name>".to_string(),
            ]);
            continue;
        }
        if trimmed == "/key" || trimmed.starts_with("/key ") {
            let key = trimmed.strip_prefix("/key").unwrap_or("").trim();
            if key.is_empty() {
                println!("usage: /key <api-key>");
                continue;
            }
            match cfg.chat.provider {
                ProviderKind::Anthropic => cfg.chat.anthropic_api_key = Some(key.to_string()),
                ProviderKind::OpenAI => cfg.chat.openai_api_key = Some(key.to_string()),
                ProviderKind::OpenRouter => cfg.chat.openrouter_api_key = Some(key.to_string()),
                ProviderKind::Ollama | ProviderKind::Mock => {
                    println!("(no API key needed for {})", cfg.chat.provider);
                    continue;
                }
            }
            adapter = Arc::from(
                eli_adapters::build_from_chat_config(&cfg.chat).context("build adapter")?,
            );
            println!("(api key set for {} - session only)", cfg.chat.provider);
            continue;
        }
        if trimmed == "/status" || trimmed == "/s" {
            print_mode_status(&state, &cfg.chat);
            print_cost_stats(&state, &cfg.chat);
            continue;
        }
        if trimmed == "/$" {
            print_cost_stats(&state, &cfg.chat);
            continue;
        }
        if trimmed == "/help" || trimmed == "/?" {
            print_help();
            continue;
        }
        if trimmed == "/undo" {
            perform_undo(&mut undo_stack, &mut memory, &store, &session_id).await?;
            continue;
        }

        // Queue prompt with + prefix (e.g., "+fix the bug" queues it)
        if trimmed.starts_with('+') {
            let queued = trimmed[1..].trim().to_string();
            if !queued.is_empty() {
                state.queue_prompt(queued);
                println!("(queued, {} in queue)", state.queue_len());
            }
            continue;
        }

        // Process images
        let (clean_prompt, images) = process_input_for_images(trimmed);
        // Run agent for this prompt (single unified persona)
        run_agent_steps(
            &cfg.chat,
            adapter.clone(),
            &diff_engine,
            &command_runner,
            &store,
            &paths.data_dir,
            &session_id,
            &project_root,
            &mut memory,
            &mut undo_stack,
            &mut state,
            AgentProfile::Coding,
            clean_prompt,
            images,
        )
        .await?;

        // Process queue automatically
        while let Some(queued_prompt) = state.next_prompt() {
            print_history_line(format!(
                "{}›{} {}",
                style::CYAN,
                style::RESET,
                queued_prompt
            ));
            // Queue currently supports text only (no image dragging into queue command yet,
            // though one could theoretically type the path, but process_input_for_images handles paths in string)
            let (q_clean, q_images) = process_input_for_images(&queued_prompt);

            run_agent_steps(
                &cfg.chat,
                adapter.clone(),
                &diff_engine,
                &command_runner,
                &store,
                &paths.data_dir,
                &session_id,
                &project_root,
                &mut memory,
                &mut undo_stack,
                &mut state,
                AgentProfile::Coding,
                q_clean,
                q_images,
            )
            .await?;
        }
    }

    Ok(())
}

fn read_line_boxed(
    state: &mut SessionState,
    chat: &mut eli_core::config::ChatConfig,
    queue_len: usize,
) -> Result<Option<String>> {
    let mut input_buffer = std::mem::take(&mut state.input_buffer);
    let mut cursor_pos = state.cursor_pos.min(input_buffer.len());
    let mut history_cursor = state.history_cursor;

    let start = Instant::now();
    let mut spinner_idx = 0usize;
    let mut last_anim = Instant::now();
    let mut footer = FooterUi::enable();
    let mut esc_armed = false;
    let mut esc_deadline = Instant::now();

    let render = |footer: &mut FooterUi,
                  spinner_idx: usize,
                  input_buffer: &str,
                  cursor_pos: usize,
                  state: &SessionState,
                  chat: &eli_core::config::ChatConfig| {
        let title = footer_title(
            "ready",
            spinner_idx,
            queue_len,
            start.elapsed(),
            state.total_usage.total_tokens,
            Some(prompt_mode(state, chat)),
        );
        footer.render(&title, input_buffer, cursor_pos);
    };

    render(
        &mut footer,
        spinner_idx,
        &input_buffer,
        cursor_pos,
        state,
        chat,
    );

    let maybe_line = loop {
        if esc_armed && Instant::now() > esc_deadline {
            esc_armed = false;
        }
        if last_anim.elapsed() > Duration::from_millis(120) {
            spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
            render(
                &mut footer,
                spinner_idx,
                &input_buffer,
                cursor_pos,
                state,
                chat,
            );
            last_anim = Instant::now();
        }

        if !ct_event::poll(Duration::from_millis(40)).unwrap_or(false) {
            continue;
        }

        let event = match ct_event::read() {
            Ok(ev) => ev,
            Err(_) => continue,
        };

        match event {
            CtEvent::Resize(_, _) => {
                render(
                    &mut footer,
                    spinner_idx,
                    &input_buffer,
                    cursor_pos,
                    state,
                    chat,
                );
                continue;
            }
            CtEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if key.modifiers.contains(CtKeyModifiers::CONTROL) {
                    match key.code {
                        CtKeyCode::Char('c') => {
                            input_buffer.clear();
                            cursor_pos = 0;
                            history_cursor = None;
                            esc_armed = false;
                            break Some(String::new());
                        }
                        CtKeyCode::Char('d') => {
                            break None;
                        }
                        _ => {}
                    }
                }

                match key.code {
                    CtKeyCode::Char(c) => {
                        history_cursor = None;
                        input_buffer.insert(cursor_pos, c);
                        cursor_pos += 1;
                        esc_armed = false;
                    }
                    CtKeyCode::Backspace => {
                        history_cursor = None;
                        if cursor_pos > 0 {
                            cursor_pos -= 1;
                            input_buffer.remove(cursor_pos);
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Delete => {
                        history_cursor = None;
                        if cursor_pos < input_buffer.len() {
                            input_buffer.remove(cursor_pos);
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Left => {
                        if cursor_pos > 0 {
                            cursor_pos -= 1;
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Right => {
                        if cursor_pos < input_buffer.len() {
                            cursor_pos += 1;
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Home => {
                        cursor_pos = 0;
                        esc_armed = false;
                    }
                    CtKeyCode::End => {
                        cursor_pos = input_buffer.len();
                        esc_armed = false;
                    }
                    CtKeyCode::Up => {
                        let Some(last_idx) = state.prompt_history.len().checked_sub(1) else {
                            continue;
                        };
                        let next = match history_cursor {
                            None => Some(last_idx),
                            Some(idx) => idx.checked_sub(1),
                        };
                        if let Some(idx) = next {
                            history_cursor = Some(idx);
                            input_buffer = state.prompt_history[idx].clone();
                            cursor_pos = input_buffer.len(); // Cursor at end after history load
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Down => {
                        let Some(idx) = history_cursor else {
                            continue;
                        };
                        let next = idx.saturating_add(1);
                        if next >= state.prompt_history.len() {
                            history_cursor = None;
                            input_buffer.clear();
                            cursor_pos = 0;
                        } else {
                            history_cursor = Some(next);
                            input_buffer = state.prompt_history[next].clone();
                            cursor_pos = input_buffer.len(); // Cursor at end after history load
                        }
                        esc_armed = false;
                    }
                    CtKeyCode::Esc => {
                        if !esc_armed {
                            esc_armed = true;
                            esc_deadline = Instant::now() + Duration::from_millis(800);
                        } else {
                            history_cursor = None;
                            input_buffer.clear();
                            cursor_pos = 0;
                            esc_armed = false;
                        }
                    }
                    CtKeyCode::Enter => {
                        let line = input_buffer.clone();
                        history_cursor = None;
                        input_buffer.clear();
                        cursor_pos = 0;
                        esc_armed = false;
                        break Some(line);
                    }
                    _ => {}
                }

                render(
                    &mut footer,
                    spinner_idx,
                    &input_buffer,
                    cursor_pos,
                    state,
                    chat,
                );
            }
            _ => {}
        }
    };

    state.input_buffer = input_buffer;
    state.cursor_pos = cursor_pos;
    state.history_cursor = history_cursor;

    Ok(maybe_line)
}

fn print_mode_status(state: &SessionState, chat: &eli_core::config::ChatConfig) {
    let display = match state.display_mode {
        DisplayMode::Standard => "standard",
        DisplayMode::Brain => "brain",
        DisplayMode::Debug => "debug",
        DisplayMode::Raw => "raw",
    };
    let agent = "autonomous (locked)";
    let exec = format_mode(chat.mode);
    let approvals = format_approvals_display(chat);
    let auto_run = if chat.auto { "on" } else { "off" };
    let time = format_duration(state.total_work_time);

    let body = format!(
        "display: {display}\nagent: {agent}\nexec: {exec}\napprovals: {approvals}\nauto-run: {auto_run}\nsteps: {}\ntime: {time}",
        state.step_count
    );
    println!("{}", render_ratatui_panel("status", &body));
}

fn print_help() {
    use style::*;

    let lines = vec![
        format!("{}{}Commands{}", BOLD, CYAN, RESET),
        String::new(),
        format!("{}Display{}", PURPLE, RESET),
        format!(
            "  {}/brain{}      full output (tools, history, details)",
            WHITE, RESET
        ),
        format!(
            "  {}/debug{}      debug output (raw request/response + tool output + observation)",
            WHITE, RESET
        ),
        format!(
            "  {}/standard{}   brief output (recent stream, summary)",
            WHITE, RESET
        ),
        String::new(),
        format!("{}Execution{}", PURPLE, RESET),
        format!("  {}/mode{}       set exec mode (read/work)", WHITE, RESET),
        format!("  {}/read{}       set exec mode to read", WHITE, RESET),
        format!("  {}/work{}       set exec mode to work", WHITE, RESET),
        format!("  {}/bot{}        work; cmds auto, diffs ask", WHITE, RESET),
        format!("  {}/yolo{}       work; auto approvals", WHITE, RESET),
        String::new(),
        format!("{}Configuration{}", PURPLE, RESET),
        format!(
            "  {}/model{}      set or show model for this session",
            WHITE, RESET
        ),
        format!(
            "  {}/key{}        set API key for current provider",
            WHITE, RESET
        ),
        String::new(),
        format!("{}Queue{}", PURPLE, RESET),
        format!("  {}/queue /q{}   show queued prompts", WHITE, RESET),
        format!("  {}/cq{}         clear queue", WHITE, RESET),
        format!("  {}+<prompt>{}   queue a prompt for later", WHITE, RESET),
        String::new(),
        format!("{}Keyboard{}", PURPLE, RESET),
        format!(
            "  {}Esc{}         interrupt current run (standard mode)",
            WHITE, RESET
        ),
        format!(
            "  {}Esc Esc{}     clear input (standard mode)",
            WHITE, RESET
        ),
        format!(
            "  {}Ctrl+C{}      clear input (standard mode)",
            WHITE, RESET
        ),
        format!("  {}Ctrl+D{}      quit (standard mode)", WHITE, RESET),
        String::new(),
        format!("{}Session{}", PURPLE, RESET),
        format!("  {}/status /s{}  show current mode/stats", WHITE, RESET),
        format!(
            "  {}/compact{}    summarize older context (reduce tokens)",
            WHITE, RESET
        ),
        format!("  {}/clear{}      clear conversation", WHITE, RESET),
        format!("  {}/reset{}      alias for /clear", WHITE, RESET),
        format!("  {}/new{}        alias for /clear", WHITE, RESET),
        format!(
            "  {}/tip{}        toggle tips (standard mode)",
            WHITE, RESET
        ),
        format!("  {}/undo{}       undo last edit", WHITE, RESET),
        format!("  {}/exit{}       quit", WHITE, RESET),
    ];

    let out = format_indented_block(&lines);
    println!("{}", out);
}

async fn perform_undo(
    undo_stack: &mut Vec<Vec<DiffResult>>,
    memory: &mut eli_core::memory::Memory,
    store: &SessionStore,
    session_id: &str,
) -> Result<()> {
    let Some(last) = undo_stack.pop() else {
        println!("(nothing to undo)");
        return Ok(());
    };

    let messages = UndoManager::undo_step(&last);
    if messages.is_empty() {
        println!("(nothing to undo)");
        return Ok(());
    }

    for msg in &messages {
        println!("{msg}");
    }

    let observation = format!("undo:\n{}", messages.join("\n"));
    memory.push(ChatMessage::tool(observation.clone(), "eli"));
    store
        .append(
            session_id,
            &SessionEvent {
                ts: chrono::Utc::now(),
                kind: EventKind::Note {
                    content: observation,
                },
            },
        )
        .await
        .ok();

    Ok(())
}

fn ensure_eli_research_brain(project_root: &Path) -> Result<PathBuf> {
    let dir = project_root.join("eli_research");
    std::fs::create_dir_all(&dir).context("create eli_research dir")?;

    let brain = dir.join("ELI.md");
    const PINNED_START: &str = "<!-- ELI_PINNED_START -->";
    const PINNED_END: &str = "<!-- ELI_PINNED_END -->";

    let pinned_block = format!(
        "{PINNED_START}\n\
## Default Research Flow\n\
- If ticker/company is ambiguous: `eli finance search --query <name>`\n\
- Start with price/volume: `eli finance timeseries` (zoom out, then zoom in). Identify key move dates.\n\
- Only then pull catalysts: `eli finance news --date YYYY-MM-DD` / `eli finance filings` for those key dates. News only matters if it moved price.\n\
- If the user mentions specific dates/days, include them (or ask 1 clarification).\n\
{PINNED_END}\n\
\n\
<!-- Append-only log below (eli writes here). -->\n"
    );

    if brain.exists() {
        // Ensure a pinned instructions section exists (like CLAUDE.md), without clobbering existing notes.
        let content = std::fs::read_to_string(&brain).unwrap_or_default();
        if content.contains(PINNED_START) && content.contains(PINNED_END) {
            return Ok(brain);
        }
        let mut out = String::new();
        out.push_str(&pinned_block);
        if !content.trim().is_empty() {
            out.push_str("\n");
            out.push_str(&content);
        }
        std::fs::write(&brain, out).context("seed eli_research/ELI.md")?;
        return Ok(brain);
    }

    std::fs::write(&brain, pinned_block).context("create eli_research/ELI.md")?;
    Ok(brain)
}

fn read_eli_brain_tail(project_root: &Path, max_chars: usize) -> Result<Option<String>> {
    const MAX_LOG_ENTRIES: usize = 5;
    const LOG_MARKER: &str = "<!-- Append-only log below (eli writes here). -->";

    let brain = ensure_eli_research_brain(project_root)?;
    let content = std::fs::read_to_string(&brain).context("read eli_research/ELI.md")?;
    if content.trim().is_empty() {
        return Ok(None);
    }

    let log_slice = if let Some(idx) = content.find(LOG_MARKER) {
        &content[idx + LOG_MARKER.len()..]
    } else {
        content.as_str()
    };

    let mut entries: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in log_slice.lines() {
        if line.starts_with("### ") {
            if !current.is_empty() {
                entries.push(current.join("\n"));
                current.clear();
            }
            current.push(line.to_string());
        } else if !current.is_empty() {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        entries.push(current.join("\n"));
    }

    if entries.is_empty() {
        return Ok(None);
    }

    let start = entries.len().saturating_sub(MAX_LOG_ENTRIES);
    let mut recent = entries[start..].join("\n\n");
    recent = recent.trim().to_string();
    if recent.is_empty() {
        return Ok(None);
    }

    if max_chars == 0 {
        return Ok(Some(recent));
    }

    let total = recent.chars().count();
    if total <= max_chars {
        return Ok(Some(recent));
    }

    let tail: String = recent.chars().skip(total - max_chars).collect();
    Ok(Some(format!("…\n{tail}")))
}

fn read_eli_brain_pinned(project_root: &Path, max_chars: usize) -> Result<Option<String>> {
    const PINNED_START: &str = "<!-- ELI_PINNED_START -->";
    const PINNED_END: &str = "<!-- ELI_PINNED_END -->";

    let brain = ensure_eli_research_brain(project_root)?;
    let content = std::fs::read_to_string(&brain).context("read eli_research/ELI.md")?;

    let Some(start) = content.find(PINNED_START) else {
        return Ok(None);
    };
    let after_start = &content[start + PINNED_START.len()..];
    let Some(end_rel) = after_start.find(PINNED_END) else {
        return Ok(None);
    };
    let pinned = after_start[..end_rel].trim();
    if pinned.is_empty() {
        return Ok(None);
    }

    if max_chars == 0 {
        return Ok(Some(pinned.to_string()));
    }

    let total = pinned.chars().count();
    if total <= max_chars {
        return Ok(Some(pinned.to_string()));
    }

    let truncated: String = pinned.chars().take(max_chars).collect();
    Ok(Some(format!("{truncated}…")))
}

fn read_eli_brain_context(
    project_root: &Path,
    pinned_max: usize,
    tail_max: usize,
) -> Result<Option<String>> {
    let pinned = match read_eli_brain_pinned(project_root, pinned_max) {
        Ok(v) => v,
        Err(e) => {
            warn!("eli brain: failed to read pinned (ignored): {e}");
            None
        }
    };
    let tail = match read_eli_brain_tail(project_root, tail_max) {
        Ok(v) => v,
        Err(e) => {
            warn!("eli brain: failed to read tail (ignored): {e}");
            None
        }
    };

    match (pinned, tail) {
        (None, None) => Ok(None),
        (Some(pinned), None) => Ok(Some(format!("ELI.md (pinned):\n{pinned}"))),
        (None, Some(tail)) => Ok(Some(format!("ELI.md (recent):\n{tail}"))),
        (Some(pinned), Some(tail)) => Ok(Some(format!(
            "ELI.md (pinned):\n{pinned}\n\nELI.md (recent):\n{tail}"
        ))),
    }
}

fn append_eli_brain(project_root: &Path, entry: &str) -> Result<()> {
    let brain = ensure_eli_research_brain(project_root)?;

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&brain)
        .context("open eli_research/ELI.md")?;

    use std::io::Write;
    f.write_all(entry.as_bytes())
        .context("append eli_research/ELI.md")?;
    if !entry.ends_with('\n') {
        f.write_all(b"\n")
            .context("append newline to eli_research/ELI.md")?;
    }
    Ok(())
}

/// Execute /copy command - query session state and copy to clipboard or file
async fn execute_copy_command(
    args: &str,
    memory: &eli_core::memory::Memory,
    project_root: &Path,
) -> Result<String> {
    use eli_core::types::Role;

    // Parse arguments
    let parts: Vec<&str> = args.split_whitespace().collect();

    // Check for file output: /copy all > file.md
    let (scope_parts, output_file) = if let Some(idx) = parts.iter().position(|&p| p == ">") {
        let (scope, rest) = parts.split_at(idx);
        let file = rest.get(1).map(|s| s.to_string());
        (scope.to_vec(), file)
    } else {
        (parts, None)
    };

    // Parse scope
    let scope = scope_parts.first().copied().unwrap_or("");

    // Check for filters (-data, -raw, -meta)
    let exclude_data = scope_parts.iter().any(|&p| p == "-data");
    let exclude_meta = scope_parts.iter().any(|&p| p == "-meta");

    // Get messages from memory
    let messages = memory.context();

    // Filter by scope
    let filtered: Vec<_> = match scope {
        "" | "last" => {
            // Last assistant response
            messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .into_iter()
                .collect()
        }
        "all" => {
            // All non-system messages
            messages.iter().filter(|m| m.role != Role::System).collect()
        }
        "user" => messages.iter().filter(|m| m.role == Role::User).collect(),
        "assistant" => messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .collect(),
        "tools" => messages.iter().filter(|m| m.role == Role::Tool).collect(),
        n if n.parse::<usize>().is_ok() => {
            // Last N turns (user + assistant pairs)
            let n: usize = n.parse().unwrap();
            let non_system: Vec<_> = messages.iter().filter(|m| m.role != Role::System).collect();
            non_system
                .into_iter()
                .rev()
                .take(n * 2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        }
        _ => {
            return Err(anyhow::anyhow!(
                "unknown scope '{}'. Use: all, last, user, assistant, tools, or N",
                scope
            ));
        }
    };

    if filtered.is_empty() {
        return Ok("Nothing to copy.".to_string());
    }

    // Format as markdown
    let mut output = String::new();
    for msg in filtered {
        let role_str = match msg.role {
            Role::User => "## User",
            Role::Assistant => "## Assistant",
            Role::Tool => &format!("### Tool: {}", msg.name.as_deref().unwrap_or("unknown")),
            Role::System => continue, // Skip system messages
        };

        output.push_str(role_str);
        output.push_str("\n\n");

        let content = if exclude_data && msg.content.len() > 2000 && msg.role == Role::Tool {
            format!(
                "[output: {} chars, omitted with -data]\n",
                msg.content.len()
            )
        } else {
            msg.content.clone()
        };

        output.push_str(&content);
        output.push_str("\n\n");
    }

    let char_count = output.len();

    // Output to file or clipboard
    if let Some(file_path) = output_file {
        let full_path = project_root.join(&file_path);
        std::fs::write(&full_path, &output)
            .with_context(|| format!("write to {}", full_path.display()))?;
        Ok(format!("Copied {} chars to {}", char_count, file_path))
    } else {
        // Copy to clipboard
        eli_screen::clipboard_set(&output)
            .await
            .map_err(|e| anyhow::anyhow!("clipboard: {}", e))?;
        Ok(format!("Copied {} chars to clipboard", char_count))
    }
}

fn slugify_for_filename(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut last_was_sep = false;

    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_was_sep = false;
        } else if matches!(
            c,
            ' ' | '-' | '_' | '.' | '/' | '\\' | ':' | ';' | ',' | '|'
        ) {
            if !out.is_empty() && !last_was_sep {
                out.push('_');
                last_was_sep = true;
            }
        }

        if max_len > 0 && out.len() >= max_len {
            break;
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    out
}

fn write_research_report_md(
    project_root: &Path,
    session_id: &str,
    chat: &eli_core::config::ChatConfig,
    prompt: &str,
    synthesis: Option<&eli_core::contract::Synthesis>,
    status: &str,
    partial_output: Option<&str>,
) -> Result<Option<PathBuf>> {
    let dir = project_root.join("eli_research");
    std::fs::create_dir_all(&dir).context("create eli_research dir")?;
    let _ = ensure_eli_research_brain(project_root)?;

    let now = chrono::Utc::now();
    let ts = now.format("%Y%m%d_%H%M%S").to_string();
    let session_short: String = session_id.chars().take(8).collect();

    let prompt_clean = strip_agent_context_block(prompt).trim();
    let title = if prompt_clean.is_empty() {
        prompt.trim()
    } else {
        prompt_clean
    };
    let title = if title.is_empty() { "Research" } else { title };
    let title_line = truncate(title, 120);

    let slug = slugify_for_filename(title, 60);
    let filename = if slug.is_empty() {
        format!("research_{ts}_{session_short}.md")
    } else {
        format!("research_{ts}_{slug}_{session_short}.md")
    };
    let path = dir.join(filename);

    let mut md = String::new();
    md.push_str(&format!("# {title_line}\n\n"));
    md.push_str(&format!("- Date (UTC): {}\n", now.to_rfc3339()));
    md.push_str(&format!("- Session: `{session_id}`\n"));
    md.push_str(&format!("- Provider: `{}`\n", chat.provider));
    md.push_str(&format!("- Model: `{}`\n", chat.model));
    md.push_str(&format!("- Status: {status}\n\n"));

    md.push_str("## Prompt\n");
    md.push_str("```\n");
    md.push_str(title.trim());
    md.push_str("\n```\n\n");

    if let Some(s) = synthesis {
        if !s.summary.is_empty() {
            md.push_str("## Summary\n");
            for item in &s.summary {
                let item = item.trim();
                if !item.is_empty() {
                    md.push_str("- ");
                    md.push_str(item);
                    md.push('\n');
                }
            }
            md.push('\n');
        }

        if !s.answer.trim().is_empty() {
            md.push_str("## Answer\n\n");
            md.push_str(s.answer.trim());
            md.push_str("\n\n");
        }

        if !s.next_steps.is_empty() {
            md.push_str("## Next Steps\n");
            for item in &s.next_steps {
                let item = item.trim();
                if !item.is_empty() {
                    md.push_str("- ");
                    md.push_str(item);
                    md.push('\n');
                }
            }
            md.push('\n');
        }
    }

    if let Some(partial) = partial_output {
        let partial = partial.trim();
        if !partial.is_empty() {
            md.push_str("## Partial Output\n");
            md.push_str("```\n");
            md.push_str(partial);
            md.push_str("\n```\n");
        }
    }

    std::fs::write(&path, md).context("write research report")?;
    Ok(Some(path))
}

fn strip_agent_context_block(prompt: &str) -> &str {
    let marker = "[ELI_AGENT_CONTEXT]";
    if let Some(idx) = prompt.find(marker) {
        return &prompt[..idx];
    }
    prompt
}

fn slash_menu_lines() -> Vec<String> {
    use style::*;

    let mut lines = Vec::new();
    lines.push(format!(
        "{}{}Slash Commands{} {}(↑/↓ to cycle){}",
        BOLD, CYAN, RESET, GRAY, RESET
    ));
    lines.push(String::new());
    for cmd in SLASH_COMMANDS {
        lines.push(format!(
            "{}{:<14}{} {}{}{}",
            WHITE, cmd.name, RESET, GRAY, cmd.desc, RESET
        ));
    }
    lines
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}
