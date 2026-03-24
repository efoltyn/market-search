/// Run chat in TUI mode (alternate screen, no ghost issues)
async fn run_chat_tui(
    cfg: &mut ConfigFile,
    adapter: Arc<dyn LlmAdapter>,
    diff_engine: &DiffEngine,
    command_runner: &CommandRunner,
    store: &SessionStore,
    paths: &Paths,
    session_id: &str,
    project_root: &Path,
    memory: &mut eli_core::memory::Memory,
    undo_stack: &mut Vec<Vec<DiffResult>>,
) -> Result<()> {
    use chat_ui::{ChatTerminal, ChatUi, PromptMode as TuiPromptMode};
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let mut ui = ChatUi::new();
    ui.prompt_mode = TuiPromptMode::Auto;
    ui.scrollback_max_lines = cfg.chat.scrollback_max_lines;
    let mut terminal = ChatTerminal::new().context("create TUI terminal")?;

    // Map TUI prompt mode to config
    cfg.chat.approvals = ApprovalMode::Auto;
    cfg.chat.auto_mode = AutoMode::Autonomous;
    let apply_tui_mode = |_mode: TuiPromptMode, cfg: &mut ConfigFile| {
        cfg.chat.approvals = ApprovalMode::Auto;
        cfg.chat.auto_mode = AutoMode::Autonomous;
    };

    loop {
        // Update spinner and per-request elapsed time
        ui.tick_spinner();
        if ui.is_processing {
            if let Some(start) = ui.request_start {
                ui.elapsed_secs = start.elapsed().as_secs();
            }
        }

        // Render
        terminal.draw(&mut ui)?;

        // Poll for events
        if let Some(event) = terminal.poll_event(Duration::from_millis(50))? {
            match event {
                Event::Paste(text) => {
                    ui.handle_paste(&text);
                    continue;
                }
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        if key.code == KeyCode::Char('o')
                            && key.modifiers.contains(KeyModifiers::ALT)
                        {
                            let enabled = ui.toggle_tool_output();
                            ui.add_message(
                                "System",
                                if enabled {
                                    "Tool output: shown."
                                } else {
                                    "Tool output: hidden (commands still shown)."
                                },
                            );
                            continue;
                        }
                        if let Some(input) = ui.handle_key(key.code, key.modifiers) {
                            let trimmed = input.trim();

                            // Handle slash commands
                            if trimmed == "/exit" || trimmed == "/quit" {
                                break;
                            }
                            if trimmed == "/help" {
                                ui.add_message(
                                    "System",
                                    "Commands: /exit, /help, /model, /compact, /clear, /copy, /status, /output\n/copy [scope] [> file] - Copy session: all, last, user, tools, N, -data\n/output - Toggle full tool stdout/stderr\nKeys: Esc interrupt, ↑↓ history, PgUp/PgDn scroll, Opt+O output toggle",
                                );
                                continue;
                            }
                            if trimmed == "/output" {
                                let enabled = ui.toggle_tool_output();
                                ui.add_message(
                                    "System",
                                    if enabled {
                                        "Tool output: shown."
                                    } else {
                                        "Tool output: hidden (commands still shown)."
                                    },
                                );
                                continue;
                            }
                            if trimmed == "/model" || trimmed.starts_with("/model ") {
                                let model = trimmed.strip_prefix("/model").unwrap_or("").trim();
                                if model.is_empty() {
                                    ui.add_message("System", &format!("model: {}", cfg.chat.model));
                                } else {
                                    cfg.chat.model = model.to_string();
                                    ui.add_message(
                                        "System",
                                        &format!("(model: {})", cfg.chat.model),
                                    );
                                }
                                continue;
                            }
                            if trimmed == "/models" {
                                ui.add_message(
                                    "System",
                                    &format!("model: {}\nset with: /model <name>", cfg.chat.model),
                                );
                                continue;
                            }
                            if trimmed == "/compact" || trimmed == "/memory compact" {
                                // Streaming compact: show progress as the model summarizes.
                                if let Some(plan) = eli_core::orchestrator::plan_compact(&cfg.chat, memory) {
                                    // Show spinner immediately
                                    ui.request_start = Some(Instant::now());
                                    ui.elapsed_secs = 0;
                                    ui.is_processing = true;
                                    terminal.draw(&mut ui)?;

                                    // Build compact request inline (streaming)
                                    let mut content = String::new();
                                    if let Some(s) = &plan.existing_summary {
                                        content.push_str("Existing summary:\n");
                                        content.push_str(s);
                                        content.push_str("\n\n");
                                    }
                                    content.push_str("Transcript (most recent last):\n");
                                    for msg in &plan.older {
                                        let role = match msg.role {
                                            eli_core::types::Role::System => "system",
                                            eli_core::types::Role::User => "user",
                                            eli_core::types::Role::Assistant => "assistant",
                                            eli_core::types::Role::Tool => "tool",
                                        };
                                        content.push_str(&format!("{role}: {}\n", msg.content));
                                    }
                                    // Truncate to max input chars
                                    let max = eli_core::orchestrator::SUMMARY_INPUT_MAX_CHARS;
                                    if content.len() > max {
                                        let start = content.char_indices()
                                            .find(|(i, _)| *i >= content.len().saturating_sub(max))
                                            .map(|(i, _)| i).unwrap_or(0);
                                        content = content[start..].to_string();
                                    }

                                    let compact_req = ChatRequest {
                                        model: cfg.chat.resolved_summary_model().to_string(),
                                        messages: vec![
                                            ChatMessage::system(eli_core::orchestrator::SUMMARY_SYSTEM_PROMPT),
                                            ChatMessage::user(content),
                                        ],
                                        temperature: Some(0.2),
                                        max_tokens: Some(eli_core::orchestrator::SUMMARY_MAX_TOKENS),
                                        response_format: None,
                                        stream: true,
                                    };

                                    let mut summary = String::new();
                                    match adapter.chat_stream(compact_req).await {
                                        Err(e) => {
                                            ui.add_message("Error", &format!("compact failed: {e}"));
                                        }
                                        Ok(mut stream) => {
                                            use eli_core::types::ChatStreamEvent;
                                            loop {
                                                tokio::select! {
                                                    ev = stream.next() => {
                                                        match ev {
                                                            Some(Ok(ChatStreamEvent::Delta(d))) => summary.push_str(&d),
                                                            Some(Ok(ChatStreamEvent::Done)) | None => break,
                                                            Some(Ok(ChatStreamEvent::Usage(_))) => {}
                                                            Some(Err(e)) => {
                                                                ui.add_message("Error", &format!("compact stream: {e}"));
                                                                break;
                                                            }
                                                        }
                                                    }
                                                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                                                }
                                                ui.tick_spinner();
                                                terminal.draw(&mut ui)?;
                                            }

                                            if !summary.trim().is_empty() {
                                                eli_core::orchestrator::apply_compact(memory, &plan, summary.trim().to_string());
                                                let note = format!(
                                                    "memory_compaction: dropped {} messages\n{}",
                                                    plan.dropped, summary.trim()
                                                );
                                                let brain_entry = format!(
                                                    "\n### {} (session {})\n{}\n",
                                                    chrono::Utc::now().to_rfc3339(),
                                                    session_id,
                                                    note
                                                );
                                                if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                                                    ui.add_message("System", &format!("(compacted, but brain write failed: {e})"));
                                                } else {
                                                    ui.add_message("System", &format!("memory: compacted ({} msgs)", plan.dropped));
                                                }
                                                store.append(session_id, &SessionEvent {
                                                    ts: chrono::Utc::now(),
                                                    kind: EventKind::Note { content: note },
                                                }).await.ok();
                                            }
                                        }
                                    }
                                    ui.is_processing = false;
                                } else {
                                    ui.add_message("System", "(nothing to compact)");
                                }
                                continue;
                            }
                            if trimmed == "/tip" {
                                ui.show_tips = !ui.show_tips;
                                ui.add_message(
                                    "System",
                                    if ui.show_tips {
                                        "Tips shown."
                                    } else {
                                        "Tips hidden."
                                    },
                                );
                                continue;
                            }
                            if trimmed == "/brain" || trimmed == "/debug" || trimmed == "/raw" {
                                // Can't switch rendering modes mid-session
                                ui.add_message("System", &format!(
                                    "Can't switch to {} mode mid-session. Exit and run: eli chat --display {}",
                                    trimmed.trim_start_matches('/'),
                                    trimmed.trim_start_matches('/')
                                ));
                                continue;
                            }
                            if trimmed == "/standard" {
                                ui.add_message("System", "Already in standard (TUI) mode.");
                                continue;
                            }
                            if trimmed == "/status" || trimmed == "/s" {
                                ui.add_message(
                                    "System",
                                    &format!(
                                        "Mode: AUTO | Tokens: {} | Time: {}s",
                                        ui.total_tokens, ui.elapsed_secs
                                    ),
                                );
                                continue;
                            }
                            if trimmed == "/copy" || trimmed.starts_with("/copy ") {
                                let args = trimmed.strip_prefix("/copy").unwrap_or("").trim();
                                let result = execute_copy_command(args, memory, project_root).await;
                                match result {
                                    Ok(msg) => ui.add_message("System", &msg),
                                    Err(e) => ui.add_message("Error", &format!("copy failed: {e}")),
                                }
                                continue;
                            }
                            if trimmed == "/clear"
                                || trimmed == "/reset"
                                || trimmed == "/new"
                                || trimmed == "/memory clear"
                            {
                                ui.messages.clear();
                                ui.add_message("System", "Conversation cleared.");
                                // Reset memory by creating fresh one with same system prompt
                                *memory = eli_core::memory::Memory::new(cfg.chat.mem_steps);
                                memory.set_system(eli_core::contract::system_prompt());
                                ui.total_tokens = 0;
                                ui.last_request_tokens = 0;
                                ui.elapsed_secs = 0;
                                ui.clear_sources();
                                continue;
                            }
                            if trimmed.is_empty() {
                                continue;
                            }

                            // Regular input - run agent
                            apply_tui_mode(ui.prompt_mode, cfg);
                            ui.add_message("You", trimmed);
                            store
                                .append(
                                    session_id,
                                    &SessionEvent {
                                        ts: chrono::Utc::now(),
                                        kind: EventKind::UserMessage {
                                            content: trimmed.to_string(),
                                        },
                                    },
                                )
                                .await
                                .ok();
                            ui.request_start = Some(Instant::now());
                            ui.elapsed_secs = 0;
                            ui.last_request_tokens = 0;
                            ui.is_processing = true;
                            ui.clear_sources();

                            // Render processing state
                            terminal.draw(&mut ui)?;

                            let (clean_prompt, images) = process_input_for_images(trimmed);

                            // Run the agent (single unified persona)
                            let result = run_agent_tui(
                                &cfg.chat,
                                adapter.clone(),
                                diff_engine,
                                command_runner,
                                store,
                                &paths.data_dir,
                                session_id,
                                project_root,
                                memory,
                                undo_stack,
                                &mut ui,
                                &mut terminal,
                                AgentProfile::Coding,
                                clean_prompt,
                                images,
                            )
                            .await;

                            ui.is_processing = false;

                            if let Err(e) = result {
                                let msg = format!("{:?}", e);
                                ui.add_message("Error", &msg);
                                store
                                    .append(
                                        session_id,
                                        &SessionEvent {
                                            ts: chrono::Utc::now(),
                                            kind: EventKind::Note { content: msg },
                                        },
                                    )
                                    .await
                                    .ok();
                            }

                            while let Some(queued) = ui.pop_queued() {
                                let trimmed = queued.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                apply_tui_mode(ui.prompt_mode, cfg);
                                ui.add_message("You", trimmed);
                                store
                                    .append(
                                        session_id,
                                        &SessionEvent {
                                            ts: chrono::Utc::now(),
                                            kind: EventKind::UserMessage {
                                                content: trimmed.to_string(),
                                            },
                                        },
                                    )
                                    .await
                                    .ok();
                                ui.request_start = Some(Instant::now());
                                ui.elapsed_secs = 0;
                                ui.last_request_tokens = 0;
                                ui.is_processing = true;
                                ui.clear_sources();
                                terminal.draw(&mut ui)?;

                                let (clean_prompt, images) = process_input_for_images(trimmed);
                                let queued_result = run_agent_tui(
                                    &cfg.chat,
                                    adapter.clone(),
                                    diff_engine,
                                    command_runner,
                                    store,
                                    &paths.data_dir,
                                    session_id,
                                    project_root,
                                    memory,
                                    undo_stack,
                                    &mut ui,
                                    &mut terminal,
                                    AgentProfile::Coding,
                                    clean_prompt,
                                    images,
                                )
                                .await;

                                ui.is_processing = false;
                                if let Err(e) = queued_result {
                                    let msg = format!("{:?}", e);
                                    ui.add_message("Error", &msg);
                                    store
                                        .append(
                                            session_id,
                                            &SessionEvent {
                                                ts: chrono::Utc::now(),
                                                kind: EventKind::Note { content: msg },
                                            },
                                        )
                                        .await
                                        .ok();
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if ui.should_quit {
            break;
        }
    }

    Ok(())
}

/// Run agent steps with TUI output
async fn run_agent_tui(
    chat: &eli_core::config::ChatConfig,
    adapter: Arc<dyn LlmAdapter>,
    _diff_engine: &DiffEngine,
    command_runner: &CommandRunner,
    store: &SessionStore,
    _data_dir: &Path,
    session_id: &str,
    _project_root: &Path,
    memory: &mut eli_core::memory::Memory,
    _undo_stack: &mut Vec<Vec<DiffResult>>,
    ui: &mut chat_ui::ChatUi,
    terminal: &mut chat_ui::ChatTerminal,
    _profile: AgentProfile,
    initial_message: String,
    images: Vec<String>,
) -> Result<()> {
    use crossterm::event as ct_event;
    use crossterm::event::{
        Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind as CtKeyEventKind,
        KeyModifiers as CtKeyModifiers,
    };
    use eli_core::types::ChatStreamEvent;
    use futures::StreamExt;

    let max_iters = if chat.auto { chat.max_auto.max(1) } else { 1 };
    let mut current_message = initial_message.clone();
    let mut current_images = images;
    let mut invalid_format_retries: u8 = 0;

    for step in 1..=max_iters {
        // Update UI
        ui.tick_spinner();
        terminal.draw(ui)?;

        // Add message to memory
        if !current_images.is_empty() {
            memory.push(ChatMessage::user_with_images(
                current_message.clone(),
                current_images.clone(),
            ));
            current_images.clear();
        } else {
            memory.push(ChatMessage::user(current_message.clone()));
        }

        // Build request — enforce JSON schema on OpenRouter so models don't go rogue.
        // context_compressed: last 20 messages verbatim, older ones get diffs/large
        // fields stripped — eliminates megabytes of stale file content from old turns.
        let response_format = if adapter.provider() == ProviderKind::OpenRouter {
            Some(ResponseFormat::EliContractJsonSchema)
        } else {
            None
        };
        let req = ChatRequest {
            messages: memory.context_compressed(20),
            model: chat.model.clone(),
            max_tokens: chat.max_tokens,
            temperature: chat.temperature,
            response_format,
            stream: true,
        };

        // Stream response (spinner in title shows we're working)
        terminal.draw(ui)?;

        // Some OpenRouter providers (e.g. Google) don't support json_schema response_format.
        // On 404, retry without it — system-prompt JSON enforcement is still in place.
        let mut stream = match adapter.chat_stream(req).await {
            Ok(s) => s,
            Err(e) if e.to_string().contains("404") && adapter.provider() == ProviderKind::OpenRouter => {
                let req_plain = ChatRequest {
                    messages: memory.context_compressed(20),
                    model: chat.model.clone(),
                    max_tokens: chat.max_tokens,
                    temperature: chat.temperature,
                    response_format: None,
                    stream: true,
                };
                adapter.chat_stream(req_plain).await.map_err(|e| anyhow::anyhow!("{}", e).context("start stream (no schema fallback)"))?
            }
            Err(e) => return Err(anyhow::anyhow!("{}", e).context("start stream")),
        };
        let mut full_response = String::new();
        let mut interrupted = false;

        let check_interrupt = |ui: &mut chat_ui::ChatUi| -> bool {
            if ui.interrupt_requested {
                ui.interrupt_requested = false;
                return true;
            }
            while ct_event::poll(Duration::from_millis(0)).unwrap_or(false) {
                let Ok(ev) = ct_event::read() else {
                    continue;
                };
                match ev {
                    CtEvent::Key(key) => {
                        if key.kind != CtKeyEventKind::Press {
                            continue;
                        }
                        if key.code == CtKeyCode::Esc {
                            return true;
                        }
                        if key.code == CtKeyCode::Char('o')
                            && key.modifiers.contains(CtKeyModifiers::ALT)
                        {
                            let enabled = ui.toggle_tool_output();
                            ui.add_message(
                                "System",
                                if enabled {
                                    "Tool output: shown."
                                } else {
                                    "Tool output: hidden (commands still shown)."
                                },
                            );
                            continue;
                        }
                        if let Some(input) = ui.handle_key(key.code, key.modifiers) {
                            let trimmed = input.trim();
                            if trimmed.eq_ignore_ascii_case("/exit")
                                || trimmed.eq_ignore_ascii_case("/quit")
                            {
                                ui.should_quit = true;
                                return true;
                            }
                            if !trimmed.is_empty() {
                                ui.queue_prompt(trimmed.to_string());
                            }
                        }
                    }
                    CtEvent::Paste(text) => {
                        ui.handle_paste(&text);
                    }
                    _ => {}
                }
            }
            false
        };

        loop {
            tokio::select! {
                maybe_ev = stream.next() => {
                    match maybe_ev {
                        Some(Ok(ChatStreamEvent::Delta(text))) => {
                            full_response.push_str(&text);
                        }
                        Some(Ok(ChatStreamEvent::Usage(usage))) => {
                            ui.total_tokens = ui.total_tokens.saturating_add(usage.total_tokens);
                            ui.last_request_tokens = ui.last_request_tokens.saturating_add(usage.total_tokens);
                        }
                        Some(Ok(ChatStreamEvent::Done)) => break,
                        Some(Err(e)) => {
                            let msg = format!("Stream error: {:?}", e);
                            ui.add_message("Error", &msg);
                            store
                                .append(
                                    session_id,
                                    &SessionEvent {
                                        ts: chrono::Utc::now(),
                                        kind: EventKind::Note { content: msg },
                                    },
                                )
                                .await
                                .ok();
                            break;
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }

            if check_interrupt(ui) {
                interrupted = true;
                break;
            }

            // Update UI periodically
            ui.tick_spinner();
            terminal.draw(ui)?;
        }

        if interrupted {
            ui.add_message("System", "(interrupted)");
            store
                .append(
                    session_id,
                    &SessionEvent {
                        ts: chrono::Utc::now(),
                        kind: EventKind::Note {
                            content: "(interrupted)".to_string(),
                        },
                    },
                )
                .await
                .ok();
            return Ok(());
        }

        // Parse response
        let model = match contract::validate_model_response(&full_response) {
            Ok(m) => {
                invalid_format_retries = 0;
                m
            }
            Err(e) => {
                // If there's no JSON object at all, the model responded in plain text.
                // Show it directly rather than retrying — this happens for greetings,
                // refusals, or models that ignore the schema.
                if contract::extract_first_json_value(&full_response).is_none() {
                    let text = full_response.trim();
                    if !text.is_empty() {
                        ui.add_message("Eli", text);
                        let text = text.to_string();
                        memory.push(ChatMessage::assistant(text.clone()));
                        store
                            .append(
                                session_id,
                                &SessionEvent {
                                    ts: chrono::Utc::now(),
                                    kind: EventKind::AssistantMessage {
                                        content: text,
                                    },
                                },
                            )
                            .await
                            .ok();
                    }
                    break;
                }
                // JSON found but failed schema validation — retry with correction.
                let msg = format!("Invalid response: {}", e);
                ui.add_message("Error", &msg);
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note { content: msg },
                        },
                    )
                    .await
                    .ok();
                invalid_format_retries = invalid_format_retries.saturating_add(1);
                if invalid_format_retries >= 3 {
                    ui.add_message(
                        "Error",
                        &format!(
                            "Too many invalid JSON responses ({}). Stopping this run.",
                            invalid_format_retries
                        ),
                    );
                    break;
                }
                let format_error = format!(
                    "FORMAT ERROR: Your previous response was invalid ({e}). Return ONLY one strict JSON object matching the Eli contract. No prose, no markdown, no <tool_call> tags."
                );
                memory.push(ChatMessage::tool(format_error.clone(), "eli.format"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note {
                                content: format_error.clone(),
                            },
                        },
                    )
                    .await
                    .ok();
                current_message = format_error;
                continue;
            }
        };
        let canonical = serde_json::to_string_pretty(&model).unwrap_or(full_response.clone());

        // Add response to memory
        memory.push(ChatMessage::assistant(canonical.clone()));
        store
            .append(
                session_id,
                &SessionEvent {
                    ts: chrono::Utc::now(),
                    kind: EventKind::AssistantMessage { content: canonical },
                },
            )
            .await
            .ok();

        // Show progress while working; only show final answer on DONE.
        let display_text: Option<String> = match model.status {
            StepStatus::Done => {
                if let Some(synthesis) = &model.synthesis {
                    if !synthesis.answer.trim().is_empty() {
                        Some(synthesis.answer.trim().to_string())
                    } else if !model.notes.trim().is_empty() {
                        Some(model.notes.trim().to_string())
                    } else {
                        None
                    }
                } else if !model.notes.trim().is_empty() {
                    Some(model.notes.trim().to_string())
                } else {
                    None
                }
            }
            StepStatus::KeepWorking => {
                let summary = model
                    .synthesis
                    .as_ref()
                    .map(|s| {
                        s.summary
                            .iter()
                            .map(|x| x.trim())
                            .filter(|x| !x.is_empty())
                            .take(3)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .filter(|s| !s.trim().is_empty());
                summary.or_else(|| {
                    if !model.notes.trim().is_empty() {
                        Some(model.notes.trim().to_string())
                    } else {
                        None
                    }
                })
            }
        };
        if matches!(model.status, StepStatus::Done) {
            ui.clear_tool_messages();
        }
        if let Some(text) = &display_text {
            let role = if matches!(model.status, StepStatus::KeepWorking) {
                "Progress"
            } else {
                "Eli"
            };
            ui.add_message(role, text);
        }

        // Execute commands if any
        if !model.commands.is_empty() && !matches!(chat.mode, RunMode::Read) {
            let mut command_results: Vec<CommandResult> = Vec::new();

            for cmd in &model.commands {
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note {
                                content: format!("$ {}", cmd),
                            },
                        },
                    )
                    .await
                    .ok();
                ui.last_tool_ok = None;
                ui.last_tool_summary = Some(format!("$ {}", truncate_line(cmd, 110)));
                ui.add_message("Tool", &format!("$ {}", cmd));
                terminal.draw(ui)?;

                let results = command_runner.run_commands(&[cmd.clone()]).await;

                for r in &results {
                    let line = tui_tool_status_summary(r);
                    ui.last_tool_ok = Some(r.returncode == 0);
                    ui.last_tool_summary = Some(line.clone());
                    if ui.show_tool_output {
                        ui.add_message("Tool", &format_tui_tool_output(r));
                    }
                    store
                        .append(
                            session_id,
                            &SessionEvent {
                                ts: chrono::Utc::now(),
                                kind: EventKind::Note { content: line },
                            },
                        )
                        .await
                        .ok();

                    // Infer sources (never invent a generic "eli finance" source)
                    for source in infer_sources(cmd, &r.stdout) {
                        ui.add_source(source);
                    }
                }
                command_results.extend(results);
                terminal.draw(ui)?;
            }

            if !command_results.is_empty() {
                command_results = augment_tool_errors(command_results);
                let command_results_for_llm =
                    shadow_large_tool_outputs(_project_root, session_id, step, &command_results);
                let observation =
                    build_observation(false, false, false, &[], &command_results_for_llm);
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
            }
        }

        // Check if done
        if matches!(model.status, StepStatus::Done) {
            ui.last_tool_ok = None;
            ui.last_tool_summary = None;
            ui.clear_tool_messages();
            break;
        }

        // Continue with KEEP WORKING
        current_message = "KEEP WORKING".to_string();
    }

    Ok(())
}

fn tui_tool_status_summary(result: &CommandResult) -> String {
    let cmd = truncate_line(result.command.trim(), 68);
    if result.returncode == 0 {
        let digest = truncate_line(&build_command_digest(result), 120);
        return format!("{cmd} · {digest}");
    }

    let mut reason = result
        .stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("returncode={}", result.returncode));
    reason = truncate_line(&reason, 120);
    format!("{cmd} · {reason}")
}

fn format_tui_tool_output(result: &CommandResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("exit={}\n", result.returncode));

    if !result.stdout.trim().is_empty() {
        out.push_str("stdout:\n");
        out.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            out.push('\n');
        }
    }

    if !result.stderr.trim().is_empty() {
        out.push_str("stderr:\n");
        out.push_str(&result.stderr);
        if !result.stderr.ends_with('\n') {
            out.push('\n');
        }
    }

    if out.trim().is_empty() {
        out.push_str("(no output)");
    }

    out
}
