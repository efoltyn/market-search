async fn run_agent_steps(
    chat: &eli_core::config::ChatConfig,
    adapter: Arc<dyn LlmAdapter>,
    diff_engine: &DiffEngine,
    command_runner: &CommandRunner,
    store: &SessionStore,
    data_dir: &Path,
    session_id: &str,
    project_root: &Path,
    memory: &mut eli_core::memory::Memory,
    undo_stack: &mut Vec<Vec<DiffResult>>,
    state: &mut SessionState,
    profile: AgentProfile,
    initial_user_message: String,
    initial_images: Vec<String>,
) -> Result<()> {
    let trajectory_logger = eli_core::trajectory::TrajectoryLogger::new(data_dir.to_path_buf());

    let max_iters = if chat.auto { chat.max_auto.max(1) } else { 1 };
    let task_start = Instant::now();
    let debug = matches!(state.display_mode, DisplayMode::Debug)
        || matches!(chat.display_mode, DisplayMode::Debug);
    let brief = matches!(state.display_mode, DisplayMode::Standard)
        && !matches!(chat.display_mode, DisplayMode::Debug)
        && has_interactive_terminal();
    let machine_stream = env_truthy("ELI_PLAIN_OUTPUT")
        || env_truthy("ELI_NO_FOOTER")
        || !has_interactive_terminal();
    let emit_stream_updates = machine_stream && env_truthy("ELI_STREAM_UPDATES");
    let emit_cli_chrome = !machine_stream || debug;
    let mut footer: Option<FooterUi> = None;
    let mut spinner_idx = 0usize;
    let mut last_anim = Instant::now();
    let synthesis_title = format_synthesis_title(&initial_user_message);
    let mut task_had_actions = false;
    let mut task_insights: Vec<String> = Vec::new();
    let mut plan_confirmed = !matches!(state.auto_mode, AutoMode::Plan);
    let mut current_message = initial_user_message;
    let mut current_images = initial_images;
    let root_prompt = current_message.clone();
    let mut invalid_format_retries: u8 = 0;
    let mut last_keep_working_signature: Option<String> = None;
    let mut repeated_keep_working_count: u32 = 0;
    let mut last_focus_notes_signature: Option<String> = None;
    let mut repeated_focus_notes_count: u32 = 0;
    let mut consecutive_all_command_failure_steps: u8 = 0;
    let mut consecutive_clap_failure_steps: u8 = 0;
    let quick_query_mode = profile == AgentProfile::Research && is_quick_market_query(&root_prompt);
    let mut forced_finalize_sent = false;

    for step in 1..=max_iters {
        let step_start = Instant::now();
        state.step_count += 1;
        let mut step_observation: Option<String> = None;

        // Sequence fix: only push "KEEP WORKING" if the last message wasn't a tool observation.
        // This avoids double-user messages which crash some providers.
        let skip_keep_working = step > 1
            && current_message == "KEEP WORKING"
            && memory.last_role() == Some(eli_core::types::Role::Tool);

        if !skip_keep_working {
            store
                .append(
                    session_id,
                    &SessionEvent {
                        ts: chrono::Utc::now(),
                        kind: EventKind::UserMessage {
                            content: current_message.clone(),
                        },
                    },
                )
                .await
                .ok();

            if !current_images.is_empty() {
                memory.push(ChatMessage::user_with_images(
                    current_message.clone(),
                    current_images.clone(),
                ));
                if !brief {
                    println!("(attached {} images)", current_images.len());
                }
                // Clear images after first use so we don't re-send them in loop unless intended
                current_images.clear();
            } else {
                memory.push(ChatMessage::user(current_message.clone()));
            }
        }

        if let Ok(Some(compaction)) = maybe_compact_memory(adapter.clone(), chat, memory).await {
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
            if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                warn!("eli brain: failed to persist compaction (ignored): {e}");
            }
            store
                .append(
                    session_id,
                    &SessionEvent {
                        ts: chrono::Utc::now(),
                        kind: EventKind::Note {
                            content: note.clone(),
                        },
                    },
                )
                .await
                .ok();
            if !brief {
                println!("memory: compacted ({} msgs)", compaction.dropped);
            }
        }

        let mut messages = memory.context();
        let disable_brain_context = env_truthy("ELI_DISABLE_BRAIN_CONTEXT");
        if !disable_brain_context {
            let min_chars = if env_truthy("ELI_AGENT_FAST") { 600 } else { 2_000 };
            let max_chars = if env_truthy("ELI_AGENT_FAST") {
                1_600
            } else {
                6_000
            };
            if let Ok(Some(ctx)) = read_eli_brain_context(project_root, min_chars, max_chars) {
                insert_system_context_before_conversation(&mut messages, ChatMessage::system(ctx));
            }
        }
        let trajectory_input = messages.clone();

        let req = ChatRequest {
            model: chat.model.clone(),
            messages,
            temperature: chat.temperature,
            max_tokens: chat.max_tokens,
            response_format: None,
            stream: true,
        };

        if debug {
            debug_print_request(&req);
        }

        use std::io::Write;
        let mut out = String::new();
        let mut interrupted = false;
        let mut interrupted_by_esc = false;

        let connect_start = Instant::now();
        if brief {
            if footer.is_none() {
                footer = Some(FooterUi::enable());
            }
            render_footer(
                &mut footer,
                "connecting",
                spinner_idx,
                connect_start.elapsed(),
                state,
                None,
            );
        }

        let stream_opt = if brief {
            let mut fut = Box::pin(adapter.chat_stream(req));
            loop {
                let changed =
                    drain_run_key_events(state, &mut interrupted, &mut interrupted_by_esc);
                if brief && (last_anim.elapsed() > Duration::from_millis(120) || changed) {
                    if last_anim.elapsed() > Duration::from_millis(120) {
                        spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                        last_anim = Instant::now();
                    }
                    render_footer(
                        &mut footer,
                        "connecting",
                        spinner_idx,
                        connect_start.elapsed(),
                        state,
                        None,
                    );
                }
                if interrupted {
                    break None;
                }
                tokio::select! {
                    res = &mut fut => break Some(res.context("chat_stream")?),
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        } else {
            Some(adapter.chat_stream(req).await.context("chat_stream")?)
        };

        if let Some(mut stream) = stream_opt {
            let thinking_start = Instant::now();
            if brief {
                render_footer(
                    &mut footer,
                    "thinking",
                    spinner_idx,
                    thinking_start.elapsed(),
                    state,
                    None,
                );
            }

            loop {
                tokio::select! {
                    maybe_ev = stream.next() => {
                        let Some(ev) = maybe_ev else { break; };
                        match ev.context("stream event")? {
                            eli_core::types::ChatStreamEvent::Delta(delta) => {
                                out.push_str(&delta);
                            }
                            eli_core::types::ChatStreamEvent::Usage(usage) => {
                                state.last_usage = Some(usage.clone());
                                state.total_usage.prompt_tokens += usage.prompt_tokens;
                                state.total_usage.completion_tokens += usage.completion_tokens;
                                state.total_usage.total_tokens += usage.total_tokens;
                            }
                            eli_core::types::ChatStreamEvent::Done => break,
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }

                let changed =
                    drain_run_key_events(state, &mut interrupted, &mut interrupted_by_esc);
                if interrupted {
                    break;
                }

                if brief && (last_anim.elapsed() > Duration::from_millis(120) || changed) {
                    if last_anim.elapsed() > Duration::from_millis(120) {
                        spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                        last_anim = Instant::now();
                    }
                    render_footer(
                        &mut footer,
                        "thinking",
                        spinner_idx,
                        thinking_start.elapsed(),
                        state,
                        None,
                    );
                }
            }
        }

        if brief && interrupted_by_esc {
            let mut armed = false;
            let mut deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if !ct_event::poll(Duration::from_millis(60)).unwrap_or(false) {
                    continue;
                }
                let Ok(CtEvent::Key(key)) = ct_event::read() else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match key.code {
                    CtKeyCode::Esc => {
                        if !armed {
                            armed = true;
                            print!(
                                "\r\x1b[K  {}!{} press {}Esc{} again to clear input",
                                style::YELLOW,
                                style::RESET,
                                style::WHITE,
                                style::RESET
                            );
                            std::io::stdout().flush().ok();
                            deadline = Instant::now() + Duration::from_secs(2);
                        } else {
                            state.input_buffer.clear();
                            state.cursor_pos = 0;
                            break;
                        }
                    }
                    CtKeyCode::Char(c) => {
                        state.input_buffer.insert(state.cursor_pos, c);
                        state.cursor_pos += 1;
                        break;
                    }
                    CtKeyCode::Backspace => {
                        if state.cursor_pos > 0 {
                            state.cursor_pos -= 1;
                            state.input_buffer.remove(state.cursor_pos);
                        }
                        break;
                    }
                    _ => break,
                }
            }
            print!("\r\x1b[K");
            std::io::stdout().flush().ok();
        }

        if interrupted {
            println!("(interrupted)");
            if profile == AgentProfile::Research {
                match write_research_report_md(
                    project_root,
                    session_id,
                    chat,
                    &root_prompt,
                    None,
                    "interrupted",
                    Some(&out),
                ) {
                    Ok(Some(path)) => {
                        let rel = path.strip_prefix(project_root).unwrap_or(&path);
                        if brief {
                            println!("  saved: {}", rel.display());
                        } else {
                            println!("(saved: {})", rel.display());
                        }

                        let note = format!(
                            "research_report_saved: {}\nstatus: interrupted\ntitle: {}",
                            rel.display(),
                            truncate(&root_prompt, 120)
                        );
                        memory.push(ChatMessage::tool(note.clone(), "eli.research"));
                        store
                            .append(
                                session_id,
                                &SessionEvent {
                                    ts: chrono::Utc::now(),
                                    kind: EventKind::Note { content: note },
                                },
                            )
                            .await
                            .ok();

                        state.record_research_report(
                            ResearchArtifact {
                                rel_path: rel.display().to_string(),
                                title: root_prompt.clone(),
                                status: "interrupted".to_string(),
                                created_utc: chrono::Utc::now().to_rfc3339(),
                                answer_hint: None,
                            },
                            24,
                        );

                        let brain_entry = format!(
                            "\n### {} (session {})\n- Research saved: {} (interrupted)\n",
                            chrono::Utc::now().to_rfc3339(),
                            session_id,
                            rel.display()
                        );
                        if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                            warn!("eli brain: failed to persist research pointer (ignored): {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warn!("failed to write interrupted research report (ignored): {e}"),
                }
            }
            break;
        }

        if out.trim().is_empty() {
            warn!("empty assistant message");
            break;
        }

        if debug {
            println!("\n=== RAW MODEL OUTPUT ===");
            print!("{}", out);
            if !out.ends_with('\n') {
                println!();
            }
            println!("=== END RAW MODEL OUTPUT ===");
        }

        let model = match contract::validate_model_response(&out) {
            Ok(m) => {
                invalid_format_retries = 0;
                m
            }
            Err(e) => {
                if emit_cli_chrome {
                    println!("eli: invalid response ({})", e);
                }
                if !brief && emit_cli_chrome {
                    println!("{}", out);
                }
                invalid_format_retries = invalid_format_retries.saturating_add(1);
                if invalid_format_retries >= 3 {
                    if emit_cli_chrome {
                        println!(
                            "eli: stopping after {} invalid-format responses",
                            invalid_format_retries
                        );
                    }
                    break;
                }

                current_message = format!(
                    "FORMAT ERROR: Your previous response was invalid ({e}). Return ONLY one strict JSON object matching the Eli contract. No prose, no markdown, no <tool_call> tags."
                );
                current_images.clear();
                continue;
            }
        };
        let canonical = serde_json::to_string_pretty(&model).unwrap_or(out.clone());

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

        // Track step time
        let step_elapsed = step_start.elapsed();

        // Print step summary (brief vs full)
        if brief {
            if step == 1 {
                // Force a scroll line so the first prompt is not overwritten.
                print_history_line(String::new());
            }
            print_step_summary_brief(step, step_elapsed, &model);
            render_footer(
                &mut footer,
                "ready",
                spinner_idx,
                Duration::ZERO,
                state,
                None,
            );
        } else if emit_cli_chrome {
            print_step_summary(step, &model);
        }

        let mut read_mode = matches!(chat.mode, RunMode::Read);
        let mut approvals_ask_commands =
            matches!(chat.resolved_command_approvals(), ApprovalMode::Ask);
        let mut approvals_ask_diffs = matches!(chat.resolved_diff_approvals(), ApprovalMode::Ask);
        let (plan_mode, plan_approvals) = parse_plan_controls(&model.plan);
        if matches!(plan_mode, Some(RunMode::Read)) {
            read_mode = true;
        }
        if matches!(plan_approvals, Some(ApprovalMode::Ask)) {
            approvals_ask_commands = true;
            approvals_ask_diffs = true;
        }

        let wants_user_input = model
            .ask_user
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        let has_actions =
            !model.commands.is_empty() || !model.diffs.is_empty() || !model.subagents.is_empty();
        if emit_stream_updates {
            let status_label = match model.status {
                StepStatus::Done => "DONE",
                StepStatus::KeepWorking => "KEEP_WORKING",
            };
            println!(
                "status: step {} {} | commands={} diffs={} subagents={}",
                step,
                status_label,
                model.commands.len(),
                model.diffs.len(),
                model.subagents.len()
            );
            let focus = model.focus.trim();
            if !focus.is_empty() {
                println!("step {} focus: {}", step, tail_chars(focus, 220));
            }
            for cmd in model.commands.iter().take(8) {
                println!("running: {}", tail_chars(cmd, 320));
            }
            if model.commands.len() > 8 {
                println!("running: +{} more command(s)", model.commands.len() - 8);
            }
        }

        // Anti-loop guard: if the model repeats identical KEEP_WORKING command sets,
        // force a synthesis pivot instead of running the same tools indefinitely.
        if matches!(model.status, StepStatus::KeepWorking)
            && !model.commands.is_empty()
            && model.diffs.is_empty()
            && model.subagents.is_empty()
        {
            let signature = model
                .commands
                .iter()
                .map(|c| c.trim())
                .collect::<Vec<_>>()
                .join("\n");
            if last_keep_working_signature
                .as_deref()
                .map(|s| s == signature)
                .unwrap_or(false)
            {
                repeated_keep_working_count = repeated_keep_working_count.saturating_add(1);
            } else {
                last_keep_working_signature = Some(signature);
                repeated_keep_working_count = 0;
            }

            if repeated_keep_working_count >= 2 {
                let loop_note = "loop_guard: repeated identical KEEP_WORKING commands detected; forcing synthesis without additional tool calls.".to_string();
                memory.push(ChatMessage::tool(loop_note.clone(), "eli.loop_guard"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note { content: loop_note },
                        },
                    )
                    .await
                    .ok();
                current_message = "LOOP_GUARD: You have repeated identical commands multiple times. Do not run more commands. Use existing tool outputs and return status DONE with a concise answer.".to_string();
                current_images.clear();
                continue;
            }
        } else {
            last_keep_working_signature = None;
            repeated_keep_working_count = 0;
        }

        if matches!(model.status, StepStatus::KeepWorking) {
            let focus_notes_sig = format!(
                "{}|{}",
                model.focus.trim().to_ascii_lowercase(),
                model.notes.trim().to_ascii_lowercase()
            );
            if last_focus_notes_signature
                .as_deref()
                .map(|s| s == focus_notes_sig)
                .unwrap_or(false)
            {
                repeated_focus_notes_count = repeated_focus_notes_count.saturating_add(1);
            } else {
                last_focus_notes_signature = Some(focus_notes_sig);
                repeated_focus_notes_count = 0;
            }

            if repeated_focus_notes_count >= 4 {
                let loop_note = "loop_guard: repeated KEEP_WORKING focus/notes detected; forcing final synthesis.".to_string();
                memory.push(ChatMessage::tool(loop_note.clone(), "eli.loop_guard"));
                store
                    .append(
                        session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note { content: loop_note },
                        },
                    )
                    .await
                    .ok();
                current_message = "LOOP_GUARD: You are repeating the same focus/notes. Stop running tools and return status DONE with the best concise answer from existing evidence.".to_string();
                current_images.clear();
                continue;
            }
        } else {
            last_focus_notes_signature = None;
            repeated_focus_notes_count = 0;
        }

        if debug {
            println!("\n=== TOOL CALL ATTEMPTED ===");
            if model.commands.is_empty()
                && model.diffs.is_empty()
                && model.subagents.is_empty()
                && model.screen.is_empty()
            {
                println!("(none)");
            } else {
                if !model.commands.is_empty() {
                    println!("commands:");
                    for cmd in &model.commands {
                        println!("  $ {}", cmd);
                    }
                }
                if !model.diffs.is_empty() {
                    println!("diffs: {}", model.diffs.len());
                    for diff in &model.diffs {
                        println!("  {:?} {}", diff.op, diff.path);
                    }
                }
                if !model.subagents.is_empty() {
                    println!("subagents: {}", model.subagents.len());
                    for agent in &model.subagents {
                        println!(
                            "  {} (model: {})",
                            agent.name,
                            agent.model.as_deref().unwrap_or("default")
                        );
                    }
                }
                if !model.screen.is_empty() {
                    println!("screen actions: {}", model.screen.len());
                }
            }
        }

        if matches!(state.auto_mode, AutoMode::Plan)
            && !plan_confirmed
            && !wants_user_input
            && !model.plan.trim().is_empty()
            && (has_actions || matches!(model.status, StepStatus::KeepWorking))
        {
            if brief {
                footer.take();
            }

            println!(
                "\n{}[PLAN]{} \n{}\n",
                style::BLUE,
                style::RESET,
                model.plan.trim_end()
            );

            use std::io::Write;
            print!(
                "{}?{} Confirm plan (Enter = proceed, type = critique): ",
                style::YELLOW,
                style::RESET
            );
            std::io::stdout().flush().ok();

            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("read plan confirmation input")?;
            let critique = input.trim();

            if !critique.is_empty() {
                current_message = critique.to_string();
                current_images.clear();
                continue;
            }

            plan_confirmed = true;

            if !has_actions {
                current_message = "Plan approved. Proceed with execution.".to_string();
                continue;
            }
        }

        let mut diff_results: Vec<DiffResult> = Vec::new();
        let mut command_results: Vec<CommandResult> = Vec::new();
        if !wants_user_input {
            if !model.diffs.is_empty() {
                if read_mode {
                    // READ mode: allow ONLY creation of NEW files.
                    for diff in &model.diffs {
                        let is_create = matches!(diff.op, contract::DiffOp::Create);
                        let res = diff_engine.apply_diff(diff, !is_create);
                        diff_results.push(res);
                    }
                    if emit_cli_chrome {
                        print_diff_results(&diff_results, true, brief);
                    }
                    let actual_changes: Vec<_> = diff_results
                        .iter()
                        .filter(|r| !r.preview && r.success)
                        .cloned()
                        .collect();
                    if !actual_changes.is_empty() {
                        undo_stack.push(actual_changes);
                    }
                } else {
                    let apply = if approvals_ask_diffs {
                        if brief {
                            footer.take();
                        }
                        let ans = confirm("Apply diffs?")?;
                        ans
                    } else {
                        true
                    };
                    diff_results = diff_engine.apply_diffs(&model.diffs, !apply);
                    if emit_cli_chrome {
                        print_diff_results(&diff_results, !apply, brief);
                    }
                    if apply {
                        undo_stack.push(diff_results.clone());
                    }
                }
            }

            if !model.commands.is_empty() {
                if read_mode {
                    // READ mode: allow all commands as requested by user
                    let parallelism = if model.commands_parallel {
                        chat.resolved_parallel_commands()
                    } else {
                        1
                    };
                    if brief {
                        let exec_start = Instant::now();
                        render_footer(
                            &mut footer,
                            "exec",
                            spinner_idx,
                            exec_start.elapsed(),
                            state,
                            None,
                        );

                        let mut fut = Box::pin(run_commands_with_policy(
                            profile,
                            command_runner,
                            &model.commands,
                            parallelism,
                        ));
                        loop {
                            tokio::select! {
                                res = &mut fut => {
                                    command_results = res;
                                    break;
                                }
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                            }
                            let changed = drain_run_key_events_queue_only(state);
                            if last_anim.elapsed() > Duration::from_millis(120) || changed {
                                if last_anim.elapsed() > Duration::from_millis(120) {
                                    spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                                    last_anim = Instant::now();
                                }
                                render_footer(
                                    &mut footer,
                                    "exec",
                                    spinner_idx,
                                    exec_start.elapsed(),
                                    state,
                                    None,
                                );
                            }
                        }
                    } else {
                        command_results = run_commands_with_policy(
                            profile,
                            command_runner,
                            &model.commands,
                            parallelism,
                        )
                        .await;
                    }
                } else {
                    let run = if approvals_ask_commands {
                        if brief {
                            footer.take();
                        }
                        let ans = confirm("Run commands?")?;
                        ans
                    } else {
                        true
                    };
                    if run {
                        let parallelism = if model.commands_parallel {
                            chat.resolved_parallel_commands()
                        } else {
                            1
                        };
                        if brief {
                            let exec_start = Instant::now();
                            render_footer(
                                &mut footer,
                                "exec",
                                spinner_idx,
                                exec_start.elapsed(),
                                state,
                                None,
                            );

                            let mut fut = Box::pin(run_commands_with_policy(
                                profile,
                                command_runner,
                                &model.commands,
                                parallelism,
                            ));
                            loop {
                                tokio::select! {
                                    res = &mut fut => {
                                        command_results = res;
                                        break;
                                    }
                                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                                }
                                let changed = drain_run_key_events_queue_only(state);
                                if last_anim.elapsed() > Duration::from_millis(120) || changed {
                                    if last_anim.elapsed() > Duration::from_millis(120) {
                                        spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                                        last_anim = Instant::now();
                                    }
                                    render_footer(
                                        &mut footer,
                                        "exec",
                                        spinner_idx,
                                        exec_start.elapsed(),
                                        state,
                                        None,
                                    );
                                }
                            }
                        } else {
                            command_results = run_commands_with_policy(
                                profile,
                                command_runner,
                                &model.commands,
                                parallelism,
                            )
                            .await;
                        }
                    } else {
                        command_results = model
                            .commands
                            .iter()
                            .map(|cmd| CommandResult {
                                command: cmd.clone(),
                                returncode: -1,
                                stdout: String::new(),
                                stderr: "Skipped (approvals_cmds=ask)".to_string(),
                                duration_ms: 0,
                                allowed: false,
                                deny_reason: Some("approvals_cmds=ask".to_string()),
                            })
                            .collect();
                    }
                }
            }

            if !command_results.is_empty() {
                command_results = augment_tool_errors(command_results);

                let attempted = command_results.iter().filter(|r| r.allowed).count();
                let failed = command_results
                    .iter()
                    .filter(|r| r.allowed && r.returncode != 0)
                    .count();
                let clap_like = command_results
                    .iter()
                    .filter(|r| r.allowed && r.returncode != 0 && looks_like_clap_error(&r.stderr))
                    .count();

                if attempted > 0 && failed == attempted {
                    consecutive_all_command_failure_steps =
                        consecutive_all_command_failure_steps.saturating_add(1);
                } else {
                    consecutive_all_command_failure_steps = 0;
                }

                if attempted > 0 && clap_like == attempted {
                    consecutive_clap_failure_steps =
                        consecutive_clap_failure_steps.saturating_add(1);
                } else {
                    consecutive_clap_failure_steps = 0;
                }
            } else {
                consecutive_all_command_failure_steps = 0;
                consecutive_clap_failure_steps = 0;
            }

            let insight = extract_insight(&command_results, &diff_results);
            if let Some(ref line) = insight {
                if task_insights.last().map(|s| s != line).unwrap_or(true) {
                    if task_insights.len() < 6 {
                        task_insights.push(line.to_string());
                    }
                }
            }

            if !command_results.is_empty() {
                if emit_stream_updates {
                    for r in command_results.iter().take(12) {
                        let cmd = tail_chars(&r.command, 220);
                        if r.allowed {
                            println!(
                                "tool: rc={} ms={} cmd={}",
                                r.returncode, r.duration_ms, cmd
                            );
                        } else {
                            let why = r
                                .deny_reason
                                .clone()
                                .unwrap_or_else(|| "policy blocked".to_string());
                            println!("tool: blocked cmd={} reason={}", cmd, tail_chars(&why, 160));
                        }
                    }
                    if command_results.len() > 12 {
                        println!("tool: +{} more result(s)", command_results.len() - 12);
                    }
                }
                if emit_cli_chrome {
                    if debug {
                        print_tool_results_debug(&command_results);
                    } else {
                        print_command_results(
                            &command_results,
                            brief,
                            matches!(state.display_mode, DisplayMode::Brain),
                        );
                    }
                }
                if brief && emit_cli_chrome {
                    render_footer(
                        &mut footer,
                        "ready",
                        spinner_idx,
                        Duration::ZERO,
                        state,
                        None,
                    );
                }
            }

            if !model.screen.is_empty() && !read_mode && !brief && emit_cli_chrome {
                print_screen_results(&model.screen).await;
            }

            let command_results_for_llm =
                shadow_large_tool_outputs(project_root, session_id, step, &command_results);

            if !diff_results.is_empty() || !command_results.is_empty() || !model.screen.is_empty() {
                task_had_actions = true;
                let observation = build_observation(
                    read_mode,
                    approvals_ask_commands,
                    approvals_ask_diffs,
                    &diff_results,
                    &command_results_for_llm,
                );
                if debug {
                    println!("\n=== OBSERVATION INJECTED (eli) ===");
                    print!("{}", observation);
                    if !observation.ends_with('\n') {
                        println!();
                    }
                    println!("=== END OBSERVATION INJECTED (eli) ===");
                }
                step_observation = Some(observation.clone());
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

                if consecutive_clap_failure_steps >= 2
                    || consecutive_all_command_failure_steps >= 3
                {
                    let loop_note = format!(
                        "fail_fast_guard: repeated command failures detected (all_failed_steps={}, clap_failed_steps={}); forcing synthesis without more tool calls.",
                        consecutive_all_command_failure_steps, consecutive_clap_failure_steps
                    );
                    memory.push(ChatMessage::tool(loop_note.clone(), "eli.fail_fast"));
                    store
                        .append(
                            session_id,
                            &SessionEvent {
                                ts: chrono::Utc::now(),
                                kind: EventKind::Note { content: loop_note },
                            },
                        )
                        .await
                        .ok();
                    current_message = "FAIL_FAST_GUARD: multiple command steps failed. Stop running tools. Reuse successful outputs already in memory and return status DONE with the best concise answer and explicit uncertainty.".to_string();
                    current_images.clear();
                    continue;
                }
            }
        }

        let subagent_results = if wants_user_input || model.subagents.is_empty() {
            Vec::new()
        } else if brief {
            let agents_start = Instant::now();
            render_footer(
                &mut footer,
                "agents",
                spinner_idx,
                agents_start.elapsed(),
                state,
                None,
            );

            let mut fut = Box::pin(run_subagents(
                adapter.clone(),
                chat,
                memory,
                &model.subagents,
            ));
            let results = loop {
                tokio::select! {
                    res = &mut fut => {
                        break res;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
                let changed = drain_run_key_events_queue_only(state);
                if last_anim.elapsed() > Duration::from_millis(120) || changed {
                    if last_anim.elapsed() > Duration::from_millis(120) {
                        spinner_idx = (spinner_idx + 1) % FOOTER_SPINNER.len();
                        last_anim = Instant::now();
                    }
                    render_footer(
                        &mut footer,
                        "agents",
                        spinner_idx,
                        agents_start.elapsed(),
                        state,
                        None,
                    );
                }
            };
            results
        } else {
            run_subagents(adapter.clone(), chat, memory, &model.subagents).await
        };
        if !subagent_results.is_empty() {
            task_had_actions = true;
            if !brief && emit_cli_chrome {
                print_subagent_results(&subagent_results);
            } else if brief {
                println!("  subagents: {} completed", subagent_results.len());
            }
            if brief {
                render_footer(
                    &mut footer,
                    "ready",
                    spinner_idx,
                    Duration::ZERO,
                    state,
                    None,
                );
            }
            let observation = build_subagent_observation(&subagent_results);
            if debug {
                println!("\n=== OBSERVATION INJECTED (eli.subagents) ===");
                print!("{}", observation);
                if !observation.ends_with('\n') {
                    println!();
                }
                println!("=== END OBSERVATION INJECTED (eli.subagents) ===");
            }
            if let Some(ref mut existing) = step_observation {
                existing.push_str("\n");
                existing.push_str(&observation);
            } else {
                step_observation = Some(observation.clone());
            }
            memory.push(ChatMessage::tool(observation.clone(), "eli.subagents"));
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

        // Capture trajectory
        let _ = trajectory_logger
            .append(&eli_core::trajectory::TrajectoryStep {
                session_id: session_id.to_string(),
                step_index: step as usize,
                timestamp: chrono::Utc::now(),
                input_messages: trajectory_input,
                model_output_raw: out.clone(),
                observation: step_observation,
                usage: state.last_usage.clone(),
            })
            .await;

        match model.status {
            StepStatus::Done => {
                let show_wrap_up = task_had_actions || step > 1;

                let mut fallback = None;
                let synthesis = model
                    .synthesis
                    .as_ref()
                    .filter(|s| synthesis_has_content(s))
                    .or_else(|| {
                        fallback = build_fallback_synthesis(&task_insights, model.notes.trim());
                        fallback.as_ref()
                    });

                if !wants_user_input {
                    if let Some(synthesis) = synthesis {
                        // Only show synthesis box if there's substantial content beyond the step summary
                        if show_wrap_up
                            || !synthesis.summary.is_empty()
                            || !synthesis.next_steps.is_empty()
                        {
                            if emit_cli_chrome {
                                print_synthesis_box(&synthesis_title, synthesis);
                            }
                        }
                        // Skip print_answer_line - step summary already showed the answer
                    }
                    // Skip print_answer_line for notes - step summary already showed them
                }

                if profile == AgentProfile::Research {
                    let status = if wants_user_input {
                        "needs_user_input"
                    } else {
                        "done"
                    };
                    let partial = if synthesis.is_some() {
                        None
                    } else {
                        Some(model.notes.as_str())
                    };

                    match write_research_report_md(
                        project_root,
                        session_id,
                        chat,
                        &root_prompt,
                        synthesis,
                        status,
                        partial,
                    ) {
                        Ok(Some(path)) => {
                            let rel = path.strip_prefix(project_root).unwrap_or(&path);
                            if brief {
                                println!("  saved: {}", rel.display());
                            } else {
                                println!("(saved: {})", rel.display());
                            }

                            let note = format!(
                                "research_report_saved: {}\nstatus: {}\ntitle: {}",
                                rel.display(),
                                status,
                                truncate(&root_prompt, 120)
                            );
                            memory.push(ChatMessage::tool(note.clone(), "eli.research"));
                            store
                                .append(
                                    session_id,
                                    &SessionEvent {
                                        ts: chrono::Utc::now(),
                                        kind: EventKind::Note { content: note },
                                    },
                                )
                                .await
                                .ok();

                            state.record_research_report(
                                ResearchArtifact {
                                    rel_path: rel.display().to_string(),
                                    title: root_prompt.clone(),
                                    status: status.to_string(),
                                    created_utc: chrono::Utc::now().to_rfc3339(),
                                    answer_hint: synthesis
                                        .map(|s| s.answer.clone())
                                        .filter(|s| !s.trim().is_empty()),
                                },
                                24,
                            );

                            let brain_entry = format!(
                                "\n### {} (session {})\n- Research saved: {} ({})\n",
                                chrono::Utc::now().to_rfc3339(),
                                session_id,
                                rel.display(),
                                status
                            );
                            if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                                warn!(
                                    "eli brain: failed to persist research pointer (ignored): {e}"
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(e) => warn!("failed to write research report (ignored): {e}"),
                    }
                }

                // Show final summary for brief mode
                let task_elapsed = task_start.elapsed();
                state.total_work_time += task_elapsed;
                if brief && step > 1 && emit_cli_chrome {
                    println!(
                        "\n{}✓{} done in {} ({} steps)",
                        style::GREEN,
                        style::RESET,
                        format_duration(task_elapsed),
                        step
                    );
                }
                break;
            }
            StepStatus::KeepWorking => {
                if quick_query_mode && !forced_finalize_sent && step >= 4 {
                    forced_finalize_sent = true;
                    current_message = "FINALIZE NOW: You have enough evidence for this quick market question. Do not run more tools. Return status DONE with a concise answer and optional brief summary. For market direction, do NOT use open-vs-previous-close; only report intraday direction from timeseries first-to-latest prices, or state that direction is unavailable.".to_string();
                    current_images.clear();
                    continue;
                }
                if step == max_iters {
                    if emit_cli_chrome {
                        println!("(stopped: max autonomous steps reached)");
                    }
                    if profile == AgentProfile::Research {
                        let synthesis = model
                            .synthesis
                            .as_ref()
                            .filter(|s| synthesis_has_content(s));
                        match write_research_report_md(
                            project_root,
                            session_id,
                            chat,
                            &root_prompt,
                            synthesis,
                            "stopped_max_steps",
                            Some(model.notes.as_str()),
                        ) {
                            Ok(Some(path)) => {
                                let rel = path.strip_prefix(project_root).unwrap_or(&path);
                                if brief {
                                    println!("  saved: {}", rel.display());
                                } else {
                                    println!("(saved: {})", rel.display());
                                }

                                let note = format!(
                                    "research_report_saved: {}\nstatus: stopped_max_steps\ntitle: {}",
                                    rel.display(),
                                    truncate(&root_prompt, 120)
                                );
                                memory.push(ChatMessage::tool(note.clone(), "eli.research"));
                                store
                                    .append(
                                        session_id,
                                        &SessionEvent {
                                            ts: chrono::Utc::now(),
                                            kind: EventKind::Note { content: note },
                                        },
                                    )
                                    .await
                                    .ok();

                                state.record_research_report(
                                    ResearchArtifact {
                                        rel_path: rel.display().to_string(),
                                        title: root_prompt.clone(),
                                        status: "stopped_max_steps".to_string(),
                                        created_utc: chrono::Utc::now().to_rfc3339(),
                                        answer_hint: synthesis
                                            .map(|s| s.answer.clone())
                                            .filter(|s| !s.trim().is_empty()),
                                    },
                                    24,
                                );

                                let brain_entry = format!(
                                    "\n### {} (session {})\n- Research saved: {} (stopped_max_steps)\n",
                                    chrono::Utc::now().to_rfc3339(),
                                    session_id,
                                    rel.display()
                                );
                                if let Err(e) = append_eli_brain(project_root, &brain_entry) {
                                    warn!("eli brain: failed to persist research pointer (ignored): {e}");
                                }
                            }
                            Ok(None) => {}
                            Err(e) => warn!("failed to write research report (ignored): {e}"),
                        }
                    }
                }
            }
        }

        if !chat.auto {
            let task_elapsed = task_start.elapsed();
            state.total_work_time += task_elapsed;
            break;
        }

        if let Some(ask) = model.ask_user {
            if !ask.trim().is_empty() {
                if brief {
                    footer.take();
                }
                let (msg, imgs) = prompt_user(ask.trim())?;
                current_message = msg;
                current_images = imgs;
                continue;
            }
        }

        current_message = "KEEP WORKING".to_string();
    }

    if brief {
        footer.take();
    }

    Ok(())
}
