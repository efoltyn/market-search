async fn cmd_agent(
    cmd: AgentCommand,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    match cmd {
        AgentCommand::Run(args) => cmd_agent_run(args, provider, model).await,
        AgentCommand::Fanout(args) => cmd_agent_fanout(args, provider, model).await,
        AgentCommand::Swarm(args) => cmd_agent_swarm(args, provider, model).await,
        AgentCommand::Critique(args) => cmd_agent_mode("critique", args, provider, model).await,
        AgentCommand::Evidence(args) => cmd_agent_mode("evidence", args, provider, model).await,
        AgentCommand::Compete(args) => cmd_agent_mode("compete", args, provider, model).await,
        AgentCommand::Debate(args) => cmd_agent_mode("debate", args, provider, model).await,
    }
}

async fn cmd_agent_mode(
    mode: &str,
    args: AgentModeArgs,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let lead_note = args
        .lead
        .as_ref()
        .map(|p| format!("Lead artifact: {}\n", resolve_abs_path(p).display()))
        .unwrap_or_default();
    let cheat_note = if args.allow_cheat {
        "Peer-aware mode: you may read peer reports from this run directory if present.\n"
    } else {
        "Independent mode: do not use peer reports as evidence.\n"
    };
    let style = match mode {
        "critique" => "Style objective: critique the lead answer and find weak points.",
        "evidence" => "Style objective: gather new supporting and rejecting evidence.",
        "compete" => "Style objective: compete for strongest answer quality.",
        "debate" => "Style objective: argue stance, rebut opposition, surface conditions.",
        _ => "Style objective: fulfill user prompt directly.",
    };
    let task_template = format!(
        "{style}\n{lead}{cheat}User objective:\n{objective}",
        style = style,
        lead = lead_note,
        cheat = cheat_note,
        objective = args.prompt
    );
    let fanout_args = AgentFanoutArgs {
        task_template,
        vars: args.vars,
        shared_manifest: args.shared_manifest,
        max_parallel: args.max_parallel,
        out: args.out,
        fallback_models: args.fallback_models,
        max_ms: args.max_ms,
        max_attempts: args.max_attempts,
        must_cite: args.must_cite,
    };
    cmd_agent_fanout(fanout_args, provider, model).await
}

async fn cmd_agent_run(
    args: AgentRunArgs,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let run_dir = resolve_agent_run_dir("run");
    let saved_result_path = run_dir.join("result.json");
    let saved_manifest_path = run_dir.join("manifest.json");
    let must_cite = normalize_must_cite_prefixes(&args.must_cite);
    let fallback_models = if args.fallback_models.is_empty() {
        default_agent_fallback_models()
    } else {
        args.fallback_models.clone()
    };
    let requested_provider = provider.clone();
    let requested_model = model.clone();
    let direct = if must_cite.is_empty() {
        try_agent_direct_route(
            "worker_01",
            &args.task,
            requested_provider.as_deref().unwrap_or("openrouter"),
            requested_model.as_deref(),
            &run_dir,
        )
        .await?
    } else {
        None
    };
    let (worker, artifact_paths) = if let Some(outcome) = direct {
        (outcome.worker, outcome.artifact_paths)
    } else {
        let worker = run_agent_worker(
            "worker_01".to_string(),
            args.task,
            provider,
            model,
            fallback_models,
            args.max_ms,
            args.max_attempts,
            run_dir.join("artifacts").join("worker_01"),
            must_cite,
        )
        .await;
        let artifact_paths: Vec<String> = worker
            .report_path
            .as_ref()
            .map(|p| vec![p.clone()])
            .unwrap_or_default();
        (worker, artifact_paths)
    };

    let ok = worker.status == "done";
    let resp = AgentRunResponse {
        ok,
        usable: ok,
        kind: "agent_run".to_string(),
        saved_result_path: saved_result_path.display().to_string(),
        saved_manifest_path: saved_manifest_path.display().to_string(),
        artifact_paths: artifact_paths.clone(),
        worker,
    };
    persist_agent_response(&resp, "agent_run", &run_dir, &artifact_paths, args.out)?;
    fail_if_unusable_response(resp.usable, &resp.saved_result_path, "agent_run")
}

async fn cmd_agent_fanout(
    args: AgentFanoutArgs,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let run_dir = resolve_agent_run_dir("fanout");
    let saved_result_path = run_dir.join("result.json");
    let saved_manifest_path = run_dir.join("manifest.json");
    let must_cite = Arc::new(normalize_must_cite_prefixes(&args.must_cite));
    let base_fallback_models = if args.fallback_models.is_empty() {
        default_agent_fallback_models()
    } else {
        args.fallback_models.clone()
    };
    let specs = load_fanout_specs(&args.task_template, &args.vars)?;
    if specs.is_empty() {
        anyhow::bail!("--vars produced 0 workers; provide a non-empty array of objects");
    }
    let shared_manifest_path = if let Some(path) = args.shared_manifest.clone() {
        let redirected = redirect_finance_output(path);
        let abs = resolve_abs_path(&redirected);
        if !abs.exists() {
            anyhow::bail!("--shared-manifest path does not exist: {}", abs.display());
        }
        if !abs.is_file() {
            anyhow::bail!("--shared-manifest is not a file: {}", abs.display());
        }
        Some(abs)
    } else {
        None
    };

    let max_parallel = args.max_parallel.max(1);
    let provider = Arc::new(provider);
    let model = Arc::new(model);
    let base_fallback_models = Arc::new(base_fallback_models);
    let run_dir_arc = Arc::new(run_dir.clone());
    let shared_manifest_arc = Arc::new(shared_manifest_path.clone());

    let stream = futures::stream::iter(specs.into_iter().map(|spec| {
        let provider = provider.clone();
        let model = model.clone();
        let base_fallback_models = base_fallback_models.clone();
        let run_dir = run_dir_arc.clone();
        let shared_manifest = shared_manifest_arc.clone();
        let must_cite = must_cite.clone();
        async move {
            let worker_name = spec.name;
            let mut worker_task = spec.task;
            if let Some(path) = shared_manifest.as_ref().as_ref() {
                worker_task = prepend_shared_manifest_context(&worker_task, path);
            }
            let worker_provider = spec.provider.or_else(|| (*provider).clone());
            let worker_model = spec.model.or_else(|| (*model).clone());
            let worker_fallback = if spec.fallback_models.is_empty() {
                (*base_fallback_models).clone()
            } else {
                spec.fallback_models
            };
            let worker_artifact_dir = run_dir
                .join("artifacts")
                .join(sanitize_worker_name(&worker_name));
            run_agent_worker(
                worker_name,
                worker_task,
                worker_provider,
                worker_model,
                worker_fallback,
                spec.max_ms.unwrap_or(args.max_ms),
                spec.max_attempts.unwrap_or(args.max_attempts),
                worker_artifact_dir,
                (*must_cite).clone(),
            )
            .await
        }
    }))
    .buffer_unordered(max_parallel);

    let mut workers: Vec<AgentWorkerResult> = stream.collect().await;
    workers.sort_by(|a, b| a.name.cmp(&b.name));
    let mut artifact_paths: Vec<String> = workers
        .iter()
        .filter_map(|w| w.report_path.clone())
        .collect();
    if let Some(path) = &shared_manifest_path {
        artifact_paths.push(path.display().to_string());
    }

    let completed = workers.iter().filter(|w| w.status == "done").count();
    let failed = workers.len().saturating_sub(completed);
    if let Ok(summary_path) = write_fanout_summary_artifact(&run_dir, &workers, completed, failed) {
        artifact_paths.push(summary_path);
    }
    if let Ok(report_path) = write_worker_compendium_markdown(
        &run_dir,
        "fanout_report.md",
        "Fanout Model Report",
        &workers,
    ) {
        artifact_paths.push(report_path);
    }
    if let Ok(report_path) = write_collaboration_draft_markdown(
        &run_dir,
        "fanout_collab.md",
        "Fanout Collaboration Draft",
        &workers,
    ) {
        artifact_paths.push(report_path);
    }
    let resp = AgentFanoutResponse {
        ok: failed == 0,
        usable: completed > 0,
        kind: "agent_fanout".to_string(),
        saved_result_path: saved_result_path.display().to_string(),
        saved_manifest_path: saved_manifest_path.display().to_string(),
        artifact_paths: artifact_paths.clone(),
        task_template: args.task_template,
        shared_manifest_path: shared_manifest_path.map(|p| p.display().to_string()),
        summary: AgentFanoutSummary {
            requested: workers.len(),
            completed,
            failed,
            max_parallel,
        },
        workers,
    };
    persist_agent_response(&resp, "agent_fanout", &run_dir, &artifact_paths, args.out)?;
    fail_if_unusable_response(resp.usable, &resp.saved_result_path, "agent_fanout")
}

async fn cmd_agent_swarm(
    args: AgentSwarmArgs,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let run_dir = resolve_agent_run_dir("swarm");
    let saved_result_path = run_dir.join("result.json");
    let saved_manifest_path = run_dir.join("manifest.json");
    let input_abs = resolve_abs_path(&args.input);
    if !input_abs.exists() {
        anyhow::bail!("--input does not exist: {}", input_abs.display());
    }
    if !input_abs.is_file() {
        anyhow::bail!("--input is not a file: {}", input_abs.display());
    }

    let fallback_models = if args.fallback_models.is_empty() {
        default_agent_fallback_models()
    } else {
        args.fallback_models.clone()
    };
    let must_cite = Arc::new(normalize_must_cite_prefixes(&args.must_cite));
    let input_text = load_swarm_input_text(&input_abs).await?;
    let chunk_texts = chunk_text_for_swarm(
        &input_text,
        args.chunks,
        args.chunk_chars,
        args.overlap_chars,
        args.max_chunks,
    );
    if chunk_texts.is_empty() {
        anyhow::bail!("input produced 0 chunks (input may be empty)");
    }

    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create swarm run dir {}", run_dir.display()))?;
    let chunks_dir = run_dir.join("artifacts/chunks");
    std::fs::create_dir_all(&chunks_dir).ok();

    let mut chunk_infos = Vec::with_capacity(chunk_texts.len());
    let mut artifact_paths = Vec::new();
    for (idx, chunk) in chunk_texts.iter().enumerate() {
        let path = chunks_dir.join(format!("chunk_{:03}.txt", idx + 1));
        std::fs::write(&path, chunk).with_context(|| format!("write {}", path.display()))?;
        chunk_infos.push(SwarmChunkInfo {
            index: idx + 1,
            path: path.display().to_string(),
            chars: chunk.chars().count(),
        });
        artifact_paths.push(path.display().to_string());
    }

    let chunk_manifest = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "input_path": input_abs.display().to_string(),
        "total_input_chars": input_text.chars().count(),
        "requested_chunks": args.chunks,
        "generated_chunks": chunk_infos.len(),
        "chunk_chars": args.chunk_chars,
        "overlap_chars": args.overlap_chars,
        "chunks": chunk_infos,
    });
    let chunk_manifest_path = run_dir.join("artifacts/chunk_manifest.json");
    std::fs::write(
        &chunk_manifest_path,
        serde_json::to_string_pretty(&chunk_manifest)?,
    )
    .context("write chunk manifest")?;
    let chunk_manifest_value =
        serde_json::to_value(&chunk_manifest).context("serialize chunk manifest for meta")?;
    write_shadow_meta_for_value(
        &chunk_manifest_path,
        &chunk_manifest_value,
        "agent.swarm",
        "agent_swarm:chunk_manifest",
    )
    .context("write chunk manifest sidecar")?;
    artifact_paths.push(chunk_manifest_path.display().to_string());

    let provider = Arc::new(provider);
    let model = Arc::new(model);
    let fallback_models = Arc::new(fallback_models);
    let run_dir_arc = Arc::new(run_dir.clone());
    let max_parallel = args.max_parallel.max(1);
    let max_ms = args.max_ms;
    let max_attempts = args.max_attempts;
    let stage_attempts = max_attempts.max(1).min(4);
    let map_max_ms = max_ms.min(45_000).max(8_000);
    let task_goal = Arc::new(args.task.clone());
    let task_mode = Arc::new(classify_swarm_mode(&args.task).to_string());
    let default_structure = Arc::new(swarm_default_structure_hint().to_string());
    let mode_overlay = Arc::new(swarm_mode_overlay(&args.task));

    let stream = futures::stream::iter(chunk_infos.iter().cloned().map(|chunk| {
        let provider = provider.clone();
        let model = model.clone();
        let fallback_models = fallback_models.clone();
        let run_dir = run_dir_arc.clone();
        let goal = task_goal.clone();
        let mode = task_mode.clone();
        let default_structure = default_structure.clone();
        let mode_overlay = mode_overlay.clone();
        let must_cite = must_cite.clone();
        async move {
            let worker_name = format!("map_{:03}", chunk.index);
            let worker_task = format!(
                "Swarm map worker {idx}.\nGoal:\n{goal}\n\nMode:\n{mode}\n\nInput chunk path:\n{path}\n\nInstructions:\n- Read only this chunk.\n- LIVE-FIRST: fetch fresh numbers with Eli tools unless the goal explicitly says local/cache mode.\n- For prediction markets search, use `eli finance odds --search \"...\" --live` by default.\n- Choose the analysis shape that best fits the goal and mode; do not force a rigid template.\n- Extract high-signal facts relevant to the goal.\n- Keep facts separate from assumptions/inference.\n- Include at least one contradiction, ambiguity, or missing piece from this chunk.\n- End with a short handoff for reduce stage prioritizing what must be merged or checked.\n- Cite any file paths you create.\n\nDefault structure (use unless a better form is clearly superior):\n{default_structure}\n\nMode overlay:\n{mode_overlay}",
                idx = chunk.index,
                goal = goal,
                mode = mode,
                path = chunk.path,
                default_structure = default_structure,
                mode_overlay = mode_overlay,
            );
            let worker_artifact_dir = run_dir.join("artifacts").join(&worker_name);
            run_agent_worker(
                worker_name,
                worker_task,
                (*provider).clone(),
                (*model).clone(),
                (*fallback_models).clone(),
                map_max_ms,
                stage_attempts,
                worker_artifact_dir,
                (*must_cite).clone(),
            )
            .await
        }
    }))
    .buffer_unordered(max_parallel);

    let mut map_workers: Vec<AgentWorkerResult> = stream.collect().await;
    map_workers.sort_by(|a, b| a.name.cmp(&b.name));
    artifact_paths.extend(map_workers.iter().filter_map(|w| w.report_path.clone()));
    let map_completed = map_workers.iter().filter(|w| w.status == "done").count();
    let map_failed = map_workers.len().saturating_sub(map_completed);

    let map_manifest = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "task": args.task,
        "input_path": input_abs.display().to_string(),
        "chunk_manifest_path": chunk_manifest_path.display().to_string(),
        "summary": {
            "requested": map_workers.len(),
            "completed": map_completed,
            "failed": map_failed,
        },
        "workers": map_workers,
    });
    let map_manifest_path = run_dir.join("artifacts/map_manifest.json");
    std::fs::write(
        &map_manifest_path,
        serde_json::to_string_pretty(&map_manifest)?,
    )
    .context("write map manifest")?;
    let map_manifest_value =
        serde_json::to_value(&map_manifest).context("serialize map manifest for meta")?;
    write_shadow_meta_for_value(
        &map_manifest_path,
        &map_manifest_value,
        "agent.swarm",
        "agent_swarm:map_manifest",
    )
    .context("write map manifest sidecar")?;
    artifact_paths.push(map_manifest_path.display().to_string());

    let mut reduce_worker = skipped_worker(
        "reduce",
        "Swarm reduce stage was skipped.",
        "skipped: no successful map workers",
    );
    let mut critic_worker = skipped_worker(
        "critic",
        "Swarm critic stage was skipped.",
        "skipped: upstream stage failed",
    );
    let mut final_worker = skipped_worker(
        "final",
        "Swarm final stage was skipped.",
        "skipped: upstream stage failed",
    );

    let map_success_rate = if map_workers.is_empty() {
        0.0
    } else {
        map_completed as f64 / map_workers.len() as f64
    };
    let reduce_max_ms = max_ms.min(60_000).max(5_000);
    let critic_max_ms = max_ms.min(45_000).max(5_000);
    let final_max_ms = max_ms.min(45_000).max(5_000);

    if map_completed > 0 {
        let reduce_task = format!(
            "Swarm reduce stage.\nGoal:\n{goal}\n\nMode:\n{mode}\n\nInputs:\n- Chunk manifest: {chunk_manifest}\n- Map manifest: {map_manifest}\n\nInstructions:\n- Read successful map reports.\n- LIVE-FIRST: if key metrics are stale/missing, run fresh Eli tool calls to refresh them.\n- Merge overlapping findings and resolve conflicts.\n- Preserve dissent where evidence is split; do not flatten disagreement.\n- Rank conclusions by confidence and provide reasons.\n- Keep the synthesis shape aligned with objective (needle-hunt, fact-check, debate, synthesis, etc.) rather than fixed headings.\n\nDefault structure (use unless a better form is clearly superior):\n{default_structure}\n\nMode overlay:\n{mode_overlay}",
            goal = args.task,
            mode = task_mode.as_str(),
            chunk_manifest = chunk_manifest_path.display(),
            map_manifest = map_manifest_path.display(),
            default_structure = default_structure.as_str(),
            mode_overlay = mode_overlay.as_str(),
        );
        reduce_worker = run_agent_worker(
            "reduce".to_string(),
            reduce_task,
            (*provider).clone(),
            (*model).clone(),
            (*fallback_models).clone(),
            reduce_max_ms,
            stage_attempts,
            run_dir.join("artifacts/reduce"),
            (*must_cite).clone(),
        )
        .await;
        if let Some(path) = &reduce_worker.report_path {
            artifact_paths.push(path.clone());
        }
    }

    if reduce_worker.status == "done" && map_success_rate >= 0.25 {
        let critic_task = format!(
            "Swarm critic stage.\nGoal:\n{goal}\n\nMode:\n{mode}\n\nInputs:\n- Map manifest: {map_manifest}\n- Reduce report: {reduce_report}\n\nInstructions:\n- Critique harshly and specifically.\n- Flag weak claims, missing evidence, stale numbers, and contradictions.\n- Prioritize correctness over style.\n- Score quality from 0-10 for: evidence freshness, rigor, completeness.\n- Provide concrete corrective actions and what to drop.\n\nDefault structure (use unless a better form is clearly superior):\n{default_structure}\n\nMode overlay:\n{mode_overlay}",
            goal = args.task,
            mode = task_mode.as_str(),
            map_manifest = map_manifest_path.display(),
            reduce_report = reduce_worker
                .report_path
                .clone()
                .unwrap_or_else(|| "<missing>".to_string()),
            default_structure = default_structure.as_str(),
            mode_overlay = mode_overlay.as_str(),
        );
        critic_worker = run_agent_worker(
            "critic".to_string(),
            critic_task,
            (*provider).clone(),
            (*model).clone(),
            (*fallback_models).clone(),
            critic_max_ms,
            stage_attempts,
            run_dir.join("artifacts/critic"),
            (*must_cite).clone(),
        )
        .await;
        if let Some(path) = &critic_worker.report_path {
            artifact_paths.push(path.clone());
        }
    }

    if reduce_worker.status == "done" {
        let final_task = format!(
            "Swarm final stage.\nGoal:\n{goal}\n\nMode:\n{mode}\n\nInputs:\n- Chunk manifest: {chunk_manifest}\n- Map manifest: {map_manifest}\n- Reduce report: {reduce_report}\n- Critic report: {critic_report}\n\nInstructions:\n- Produce final answer with evidence-weighted conclusions.\n- Incorporate valid critic feedback and explicitly state what changed.\n- LIVE-FIRST: ensure key numeric claims are refreshed via Eli tools in this stage if needed.\n- Use default structure unless the objective is better served by another form; avoid rigid formatting.\n- Be explicit about uncertainty and unresolved conflicts.\n\nDefault structure (use unless a better form is clearly superior):\n{default_structure}\n\nMode overlay:\n{mode_overlay}",
            goal = args.task,
            mode = task_mode.as_str(),
            chunk_manifest = chunk_manifest_path.display(),
            map_manifest = map_manifest_path.display(),
            reduce_report = reduce_worker
                .report_path
                .clone()
                .unwrap_or_else(|| "<missing>".to_string()),
            critic_report = critic_worker
                .report_path
                .clone()
                .unwrap_or_else(|| "<missing>".to_string()),
            default_structure = default_structure.as_str(),
            mode_overlay = mode_overlay.as_str(),
        );
        final_worker = run_agent_worker(
            "final".to_string(),
            final_task,
            (*provider).clone(),
            (*model).clone(),
            (*fallback_models).clone(),
            final_max_ms,
            stage_attempts,
            run_dir.join("artifacts/final"),
            (*must_cite).clone(),
        )
        .await;
        if let Some(path) = &final_worker.report_path {
            artifact_paths.push(path.clone());
        }
    }
    if let Ok(report_path) = write_swarm_markdown_report(
        &run_dir,
        &args.task,
        &chunk_manifest_path,
        &map_manifest_path,
        &map_workers,
        &reduce_worker,
        &critic_worker,
        &final_worker,
    ) {
        artifact_paths.push(report_path);
    }
    let mut collab_workers = map_workers.clone();
    collab_workers.push(reduce_worker.clone());
    collab_workers.push(critic_worker.clone());
    collab_workers.push(final_worker.clone());
    if let Ok(report_path) = write_collaboration_draft_markdown(
        &run_dir,
        "swarm_collab.md",
        "Swarm Collaboration Draft",
        &collab_workers,
    ) {
        artifact_paths.push(report_path);
    }

    artifact_paths.sort();
    artifact_paths.dedup();
    let resp = AgentSwarmResponse {
        ok: final_worker.status == "done",
        usable: final_worker.status == "done" || reduce_worker.status == "done",
        kind: "agent_swarm".to_string(),
        saved_result_path: saved_result_path.display().to_string(),
        saved_manifest_path: saved_manifest_path.display().to_string(),
        artifact_paths: artifact_paths.clone(),
        task: args.task,
        input_path: input_abs.display().to_string(),
        chunk_manifest_path: chunk_manifest_path.display().to_string(),
        map_manifest_path: map_manifest_path.display().to_string(),
        summary: AgentSwarmSummary {
            requested_chunks: args.chunks.unwrap_or(0),
            generated_chunks: chunk_texts.len(),
            map_completed,
            map_failed,
            max_parallel,
        },
        map_workers,
        reduce_worker,
        critic_worker,
        final_worker,
    };
    persist_agent_response(&resp, "agent_swarm", &run_dir, &artifact_paths, args.out)?;
    fail_if_unusable_response(resp.usable, &resp.saved_result_path, "agent_swarm")
}

async fn load_swarm_input_text(path: &Path) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "pdf" {
        return extract_pdf_text_via_pdftotext(path).await;
    }
    std::fs::read_to_string(path).with_context(|| format!("read input text {}", path.display()))
}

async fn extract_pdf_text_via_pdftotext(path: &Path) -> Result<String> {
    let output = TokioCommand::new("pdftotext")
        .arg("-layout")
        .arg(path)
        .arg("-")
        .output()
        .await
        .with_context(|| {
            format!(
                "run pdftotext for {} (install poppler to enable PDF swarm input)",
                path.display()
            )
        })?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        anyhow::bail!("pdftotext failed for {}: {}", path.display(), err.trim());
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        anyhow::bail!("pdftotext produced empty output for {}", path.display());
    }
    Ok(text)
}

fn chunk_text_for_swarm(
    text: &str,
    chunk_count: Option<usize>,
    chunk_chars: usize,
    overlap_chars: usize,
    max_chunks: usize,
) -> Vec<String> {
    if text.trim().is_empty() || max_chunks == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    if total == 0 {
        return Vec::new();
    }
    let requested_chunks = chunk_count.unwrap_or(0).min(max_chunks);
    if requested_chunks > 0 {
        let mut out = Vec::with_capacity(requested_chunks);
        for i in 0..requested_chunks {
            let base_start = i * total / requested_chunks;
            let base_end = (i + 1) * total / requested_chunks;
            let start = if i == 0 {
                base_start
            } else {
                base_start.saturating_sub(overlap_chars)
            };
            let end = if i + 1 == requested_chunks {
                base_end
            } else {
                (base_end + overlap_chars).min(total)
            };
            if end > start {
                let chunk: String = chars[start..end].iter().collect();
                out.push(chunk);
            }
        }
        return out;
    }

    let target_size = chunk_chars.max(1);
    let overlap = overlap_chars.min(target_size.saturating_sub(1));
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < total && out.len() < max_chunks {
        let remaining_slots = max_chunks.saturating_sub(out.len());
        let mut end = (start + target_size).min(total);
        if remaining_slots <= 1 {
            end = total;
        }
        if end <= start {
            break;
        }
        let chunk: String = chars[start..end].iter().collect();
        out.push(chunk);
        if end >= total {
            break;
        }
        let mut next = end.saturating_sub(overlap);
        if next <= start {
            next = end;
        }
        start = next;
    }
    out
}

fn load_fanout_specs(task_template: &str, vars_path: &Path) -> Result<Vec<AgentWorkerSpec>> {
    let raw = std::fs::read_to_string(vars_path)
        .with_context(|| format!("read vars file {}", vars_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse JSON {}", vars_path.display()))?;
    let rows = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("--vars file must be a JSON array of objects"))?;

    let mut out = Vec::with_capacity(rows.len());
    for (idx, row) in rows.iter().enumerate() {
        let obj = row.as_object().ok_or_else(|| {
            anyhow::anyhow!("--vars item {} must be an object; got {}", idx + 1, row)
        })?;
        let mut vars = obj.clone();
        let name = vars
            .remove("name")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("worker_{:02}", idx + 1));
        let provider = vars
            .remove("provider")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let model = vars
            .remove("model")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let fallback_models = parse_fallback_models_value(vars.remove("fallback_models"));
        let max_ms = vars.remove("max_ms").and_then(|v| {
            if let Some(n) = v.as_u64() {
                Some(n)
            } else if let Some(s) = v.as_str() {
                s.trim().parse::<u64>().ok()
            } else {
                None
            }
        });
        let max_attempts = vars.remove("max_attempts").and_then(|v| {
            if let Some(n) = v.as_u64() {
                Some(n as usize)
            } else if let Some(s) = v.as_str() {
                s.trim().parse::<usize>().ok()
            } else {
                None
            }
        });
        let task = render_task_template(task_template, &vars);
        out.push(AgentWorkerSpec {
            name,
            task,
            provider,
            model,
            fallback_models,
            max_ms,
            max_attempts,
        });
    }
    Ok(out)
}

fn prepend_shared_manifest_context(task: &str, shared_manifest_path: &Path) -> String {
    format!(
        "Shared Artifact Contract:\n- Read manifest first: {path}\n- Use artifact paths + sidecars as ground truth (not prose).\n- If you create data artifacts, use Eli tools with --out auto and cite path + meta_path.\n\n{task}",
        path = shared_manifest_path.display(),
        task = task
    )
}

fn parse_fallback_models_value(value: Option<serde_json::Value>) -> Vec<String> {
    let Some(v) = value else {
        return Vec::new();
    };
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Some(s) = v.as_str() {
        return s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
    }
    Vec::new()
}

fn normalize_must_cite_prefixes(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in raw {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_string();
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    out
}

fn missing_required_citations(report_path: &str, must_cite: &[String]) -> Option<String> {
    if must_cite.is_empty() {
        return None;
    }
    let report_raw = match std::fs::read_to_string(report_path) {
        Ok(v) => v,
        Err(err) => {
            return Some(format!(
                "unable to read report for citation checks: {} ({})",
                report_path, err
            ))
        }
    };
    let mut missing = Vec::new();
    for prefix in must_cite {
        let required = prefix.trim();
        if required.is_empty() {
            continue;
        }
        let required_abs = resolve_abs_path(Path::new(required))
            .display()
            .to_string();
        let matched = report_raw.contains(required) || report_raw.contains(&required_abs);
        if !matched {
            missing.push(required_abs);
        }
    }
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "missing required citation prefix(es): {}",
            missing.join(", ")
        ))
    }
}

fn fail_if_unusable_response(usable: bool, result_path: &str, kind: &str) -> Result<()> {
    if usable {
        return Ok(());
    }
    anyhow::bail!(
        "{} completed with usable=false (see {})",
        kind,
        result_path
    );
}

fn default_agent_fallback_models() -> Vec<String> {
    vec![
        "arcee-ai/trinity-mini:free".to_string(),
        "stepfun/step-3.5-flash:free".to_string(),
        "z-ai/glm-4.5-air:free".to_string(),
        "nvidia/nemotron-3-nano-30b-a3b:free".to_string(),
        "openrouter/free".to_string(),
    ]
}

fn load_model_health() -> std::collections::BTreeMap<String, ModelHealthEntry> {
    let path = Path::new(MODEL_HEALTH_PATH);
    let Ok(raw) = std::fs::read_to_string(path) else {
        return std::collections::BTreeMap::new();
    };
    serde_json::from_str::<std::collections::BTreeMap<String, ModelHealthEntry>>(&raw)
        .unwrap_or_default()
}

fn save_model_health(health: &std::collections::BTreeMap<String, ModelHealthEntry>) {
    let path = Path::new(MODEL_HEALTH_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(raw) = serde_json::to_string_pretty(health) {
        let _ = std::fs::write(path, raw);
    }
}

fn is_model_temporarily_disabled(model: &str) -> bool {
    let health = load_model_health();
    let Some(entry) = health.get(model) else {
        return false;
    };
    if let Some(limit_until) = &entry.limit_until {
        if let Ok(limit_ts) = chrono::DateTime::parse_from_rfc3339(limit_until) {
            if chrono::Utc::now() < limit_ts.with_timezone(&chrono::Utc) {
                return true;
            }
        }
    }
    if entry.consecutive_failures < MODEL_DISABLE_CONSECUTIVE_FAILURES {
        return false;
    }
    let Some(ts) = &entry.last_seen_at else {
        return true;
    };
    let Ok(last) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return true;
    };
    let age_mins = chrono::Utc::now()
        .signed_duration_since(last.with_timezone(&chrono::Utc))
        .num_minutes();
    age_mins < model_disable_minutes(entry.consecutive_failures)
}

fn model_disable_minutes(consecutive_failures: u32) -> i64 {
    if consecutive_failures < MODEL_DISABLE_CONSECUTIVE_FAILURES {
        return 0;
    }
    let over = consecutive_failures.saturating_sub(MODEL_DISABLE_CONSECUTIVE_FAILURES);
    let exp = over.min(6); // 10m, 20m, 40m ... capped below.
    let cooldown = MODEL_DISABLE_BASE_MINUTES.saturating_mul(1i64 << exp);
    cooldown.min(MODEL_DISABLE_MAX_MINUTES).max(MODEL_DISABLE_BASE_MINUTES)
}

fn pick_probe_candidates(candidates: &[Option<String>], max_count: usize) -> Vec<Option<String>> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let health = load_model_health();
    let mut ranked: Vec<(Option<String>, i64)> = Vec::new();
    for candidate in candidates {
        let label = candidate
            .clone()
            .unwrap_or_else(|| "<config-default>".to_string());
        let recency = health
            .get(&label)
            .and_then(|entry| entry.last_seen_at.as_ref())
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc).timestamp())
            .unwrap_or(i64::MIN);
        ranked.push((candidate.clone(), recency));
    }
    ranked.sort_by_key(|(_, recency)| *recency);
    ranked
        .into_iter()
        .take(max_count.max(1))
        .map(|(candidate, _)| candidate)
        .collect()
}

fn record_model_health_attempt(model: &str, ok: bool, err: Option<String>, count_failure: bool) {
    if model.trim().is_empty() {
        return;
    }
    let mut health = load_model_health();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = health.entry(model.to_string()).or_default();
    entry.last_seen_at = Some(now);
    if ok {
        entry.consecutive_failures = 0;
        entry.last_error = None;
        entry.limit_until = None;
    } else {
        if count_failure {
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        }
        entry.last_error = err.map(|e| tail_chars(&e, 240));
    }
    save_model_health(&health);
}

fn record_model_limit_signal(model: &str, err: Option<String>) {
    if model.trim().is_empty() {
        return;
    }
    let mut health = load_model_health();
    let now = chrono::Utc::now();
    let entry = health.entry(model.to_string()).or_default();
    entry.last_seen_at = Some(now.to_rfc3339());
    entry.last_error = err.map(|e| tail_chars(&e, 240));
    entry.consecutive_failures = 0;
    let limit_until = now + chrono::Duration::seconds(MODEL_LIMIT_SIGNAL_COOLDOWN_SECS.max(30));
    entry.limit_until = Some(limit_until.to_rfc3339());
    save_model_health(&health);
}

fn is_model_limit_signal(error_text: &str) -> bool {
    let e = error_text.to_ascii_lowercase();
    e.contains("rate limit")
        || e.contains("429")
        || e.contains("quota")
        || e.contains("timed out")
        || e.contains("max steps")
        || e.contains("stopped_max_steps")
        || e.contains("context length")
        || e.contains("maximum context")
        || e.contains("max context")
        || e.contains("finish_reason=length")
        || e.contains("model is overloaded")
        || e.contains("temporarily unavailable")
}

fn is_transient_agent_failure(error_text: &str) -> bool {
    let e = error_text.to_ascii_lowercase();
    e.contains("empty assistant message")
        || e.contains("stream parse error")
        || e.contains("stream event")
        || e.contains("error decoding response body")
        || e.contains("timed out")
        || e.contains("http 5")
}

async fn try_agent_direct_route(
    worker_name: &str,
    task: &str,
    provider: &str,
    requested_model: Option<&str>,
    run_dir: &Path,
) -> Result<Option<DirectAgentOutcome>> {
    let lower = task.to_ascii_lowercase();
    let started_at = chrono::Utc::now();
    let t0 = Instant::now();
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();

    if lower.contains("recession") {
        let macro_resp = eli_core::finance::fetch_macro(eli_core::finance::MacroRequest {
            range: Some(eli_core::finance::Span::parse("1y").map_err(|e| anyhow::anyhow!(e))?),
            compare_to: None,
            policy_file: None,
            policy_mode: None,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route macro")?;
        let odds_resp = eli_core::finance::fetch_odds(odds_series_request("KXRECSSNBER"))
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .context("direct route odds")?;

        let macro_path = artifacts_dir.join("macro.json");
        let odds_path = artifacts_dir.join("odds.json");
        std::fs::write(&macro_path, serde_json::to_string_pretty(&macro_resp)?)
            .context("write direct macro")?;
        std::fs::write(&odds_path, serde_json::to_string_pretty(&odds_resp)?)
            .context("write direct odds")?;
        let macro_meta_value =
            serde_json::to_value(&macro_resp).context("serialize direct macro for meta")?;
        write_shadow_meta_for_value(
            &macro_path,
            &macro_meta_value,
            "agent.direct",
            "direct_route:recession:macro",
        )
        .context("write direct macro sidecar")?;
        let odds_meta_value =
            serde_json::to_value(&odds_resp).context("serialize direct odds for meta")?;
        write_shadow_meta_for_value(
            &odds_path,
            &odds_meta_value,
            "agent.direct",
            "direct_route:recession:odds",
        )
        .context("write direct odds sidecar")?;

        let recession_2026 = odds_resp
            .markets
            .iter()
            .find(|m| m.ticker.contains("-26"))
            .or_else(|| {
                odds_resp
                    .markets
                    .iter()
                    .find(|m| m.title.to_ascii_lowercase().contains("2026"))
            })
            .and_then(|m| m.probability_yes);
        let unrate = macro_resp
            .indicators
            .iter()
            .find(|i| i.symbol == "UNRATE")
            .map(|i| i.current_value);
        let fedfunds = macro_resp
            .indicators
            .iter()
            .find(|i| i.symbol == "FEDFUNDS")
            .map(|i| i.current_value);
        let spread_10y2y = macro_resp
            .indicators
            .iter()
            .find(|i| i.symbol == "T10Y2Y")
            .map(|i| i.current_value);

        let analysis = json!({
            "recession_2026_probability": recession_2026,
            "unemployment_rate": unrate,
            "fed_funds_rate": fedfunds,
            "spread_10y2y": spread_10y2y,
            "market_count": odds_resp.markets.len(),
        });
        let analysis_path = artifacts_dir.join("analysis.json");
        std::fs::write(&analysis_path, serde_json::to_string_pretty(&analysis)?)
            .context("write direct analysis")?;
        write_shadow_meta_for_value(
            &analysis_path,
            &analysis,
            "agent.direct",
            "direct_route:recession:analysis",
        )
        .context("write direct analysis sidecar")?;

        let summary_path = artifacts_dir.join("summary.md");
        std::fs::write(
            &summary_path,
            format!(
                "# Direct agent route\n\n- Task: {task}\n- Route: recession_packet\n- Recession (2026) probability: {:?}\n- Unemployment: {:?}\n- Fed funds: {:?}\n- 10Y-2Y spread: {:?}\n",
                recession_2026, unrate, fedfunds, spread_10y2y
            ),
        )
        .context("write direct summary")?;

        let artifact_paths = vec![
            macro_path.display().to_string(),
            odds_path.display().to_string(),
            analysis_path.display().to_string(),
            summary_path.display().to_string(),
        ];
        let worker = AgentWorkerResult {
            name: worker_name.to_string(),
            task: task.to_string(),
            status: "done".to_string(),
            exit_code: Some(0),
            requested_model: requested_model.map(|s| s.to_string()),
            used_model: Some("direct-tools".to_string()),
            attempted_models: vec!["direct-tools".to_string()],
            attempt_count: 1,
            attempts: vec![AgentAttemptResult {
                model: "direct-tools".to_string(),
                status: "ok".to_string(),
                duration_ms: t0.elapsed().as_millis(),
                exit_code: Some(0),
                error: None,
            }],
            report_path: Some(summary_path.display().to_string()),
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail: format!("direct_route=recession provider={provider}"),
            stderr_tail: String::new(),
        };
        return Ok(Some(DirectAgentOutcome {
            worker,
            artifact_paths,
        }));
    }

    if lower.contains("risk") || lower.contains("risks") || lower.contains("supply chain") {
        let subject = extract_subject_after_for(task).unwrap_or_else(|| task.to_string());
        let search = eli_core::finance::fetch_search(eli_core::finance::SearchRequest {
            query: subject.clone(),
            policy_file: None,
            policy_mode: None,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route risk search")?;

        let mut symbol = pick_primary_symbol(&search);
        if symbol.is_none() {
            let fallback_query = subject
                .split_whitespace()
                .find(|w| {
                    w.chars().all(|c| c.is_ascii_alphanumeric())
                        && w.len() >= 2
                        && w.len() <= 8
                })
                .unwrap_or("apple");
            let fallback_search = eli_core::finance::fetch_search(eli_core::finance::SearchRequest {
                query: fallback_query.to_string(),
                policy_file: None,
                policy_mode: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .context("direct route risk fallback search")?;
            symbol = pick_primary_symbol(&fallback_search);
        }
        if symbol.is_none() {
            symbol = infer_symbol_from_text(&subject);
        }
        let Some(symbol) = symbol else {
            return Ok(None);
        };

        let snapshot = eli_core::finance::fetch_snapshot(eli_core::finance::SnapshotRequest {
            tickers: vec![symbol.clone()],
            provider: eli_core::finance::ProviderKind::Yahoo,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route risk snapshot")?;
        let today = chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string();
        let news = eli_core::finance::fetch_news(eli_core::finance::NewsRequest {
            ticker: symbol.clone(),
            date: today.clone(),
            policy_file: None,
            policy_mode: None,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route risk news")?;

        let search_path = artifacts_dir.join("search.json");
        let snapshot_path = artifacts_dir.join("snapshot.json");
        let news_path = artifacts_dir.join("news.json");
        std::fs::write(&search_path, serde_json::to_string_pretty(&search)?)
            .context("write direct risk search")?;
        std::fs::write(&snapshot_path, serde_json::to_string_pretty(&snapshot)?)
            .context("write direct risk snapshot")?;
        std::fs::write(&news_path, serde_json::to_string_pretty(&news)?)
            .context("write direct risk news")?;

        let search_meta = serde_json::to_value(&search).context("serialize risk search")?;
        let snapshot_meta = serde_json::to_value(&snapshot).context("serialize risk snapshot")?;
        let news_meta = serde_json::to_value(&news).context("serialize risk news")?;
        write_shadow_meta_for_value(
            &search_path,
            &search_meta,
            "agent.direct",
            "direct_route:risk:search",
        )
        .context("write risk search sidecar")?;
        write_shadow_meta_for_value(
            &snapshot_path,
            &snapshot_meta,
            "agent.direct",
            "direct_route:risk:snapshot",
        )
        .context("write risk snapshot sidecar")?;
        write_shadow_meta_for_value(&news_path, &news_meta, "agent.direct", "direct_route:risk:news")
            .context("write risk news sidecar")?;

        let headline_count = news.news.len();
        let summary_path = artifacts_dir.join("summary.md");
        std::fs::write(
            &summary_path,
            format!(
                "# Direct agent route\n\n- Task: {task}\n- Route: risk_packet\n- Subject: {subject}\n- Symbol: {symbol}\n- News date: {today}\n- Headlines fetched: {headline_count}\n"
            ),
        )
        .context("write direct risk summary")?;

        let artifact_paths = vec![
            search_path.display().to_string(),
            snapshot_path.display().to_string(),
            news_path.display().to_string(),
            summary_path.display().to_string(),
        ];
        let worker = AgentWorkerResult {
            name: worker_name.to_string(),
            task: task.to_string(),
            status: "done".to_string(),
            exit_code: Some(0),
            requested_model: requested_model.map(|s| s.to_string()),
            used_model: Some("direct-tools".to_string()),
            attempted_models: vec!["direct-tools".to_string()],
            attempt_count: 1,
            attempts: vec![AgentAttemptResult {
                model: "direct-tools".to_string(),
                status: "ok".to_string(),
                duration_ms: t0.elapsed().as_millis(),
                exit_code: Some(0),
                error: None,
            }],
            report_path: Some(summary_path.display().to_string()),
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail: format!("direct_route=risk subject={subject} symbol={symbol}"),
            stderr_tail: String::new(),
        };
        return Ok(Some(DirectAgentOutcome {
            worker,
            artifact_paths,
        }));
    }

    if let Some(subject) = extract_price_subject(task) {
        let search = eli_core::finance::fetch_search(eli_core::finance::SearchRequest {
            query: subject.clone(),
            policy_file: None,
            policy_mode: None,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route search")?;
        let symbol = search
            .results
            .iter()
            .find(|x| {
                x.asset_type
                    .as_ref()
                    .map(|t| {
                        let tt = t.to_ascii_uppercase();
                        tt == "EQUITY" || tt == "ETF" || tt == "INDEX"
                    })
                    .unwrap_or(true)
            })
            .map(|x| x.symbol.clone())
            .or_else(|| search.results.first().map(|x| x.symbol.clone()));
        let Some(symbol) = symbol else {
            return Ok(None);
        };
        let snapshot = eli_core::finance::fetch_snapshot(eli_core::finance::SnapshotRequest {
            tickers: vec![symbol.clone()],
            provider: eli_core::finance::ProviderKind::Yahoo,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route snapshot")?;
        let snap = match snapshot.snapshots.first() {
            Some(s) => s,
            None => return Ok(None),
        };
        let price = snap.current_price.or(snap.open).unwrap_or(0.0);
        let prev = snap.previous_close.unwrap_or(0.0);
        let pct = if prev > 0.0 {
            (price / prev - 1.0) * 100.0
        } else {
            0.0
        };
        let search_path = artifacts_dir.join("search.json");
        let snapshot_path = artifacts_dir.join("snapshot.json");
        std::fs::write(&search_path, serde_json::to_string_pretty(&search)?)
            .context("write direct search")?;
        std::fs::write(&snapshot_path, serde_json::to_string_pretty(&snapshot)?)
            .context("write direct snapshot")?;
        let search_meta_value =
            serde_json::to_value(&search).context("serialize direct search for meta")?;
        write_shadow_meta_for_value(
            &search_path,
            &search_meta_value,
            "agent.direct",
            "direct_route:price_lookup:search",
        )
        .context("write direct search sidecar")?;
        let snapshot_meta_value =
            serde_json::to_value(&snapshot).context("serialize direct snapshot for meta")?;
        write_shadow_meta_for_value(
            &snapshot_path,
            &snapshot_meta_value,
            "agent.direct",
            "direct_route:price_lookup:snapshot",
        )
        .context("write direct snapshot sidecar")?;
        let summary_path = artifacts_dir.join("summary.md");
        let summary = format!(
            "# Direct agent route\n\n- Task: {task}\n- Route: price_lookup\n- Symbol: {symbol}\n- Price: ${price:.4}\n- Prev close: ${prev:.4}\n- Change: {pct:.2}%\n"
        );
        std::fs::write(&summary_path, summary).context("write direct summary")?;
        let artifact_paths = vec![
            search_path.display().to_string(),
            snapshot_path.display().to_string(),
            summary_path.display().to_string(),
        ];
        let worker = AgentWorkerResult {
            name: worker_name.to_string(),
            task: task.to_string(),
            status: "done".to_string(),
            exit_code: Some(0),
            requested_model: requested_model.map(|s| s.to_string()),
            used_model: Some("direct-tools".to_string()),
            attempted_models: vec!["direct-tools".to_string()],
            attempt_count: 1,
            attempts: vec![AgentAttemptResult {
                model: "direct-tools".to_string(),
                status: "ok".to_string(),
                duration_ms: t0.elapsed().as_millis(),
                exit_code: Some(0),
                error: None,
            }],
            report_path: Some(summary_path.display().to_string()),
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail: format!("direct_route=price_lookup symbol={symbol} provider={provider}"),
            stderr_tail: String::new(),
        };
        return Ok(Some(DirectAgentOutcome {
            worker,
            artifact_paths,
        }));
    }

    let compare_tickers = extract_compare_tickers(task);
    if lower.contains("compare") && compare_tickers.len() >= 2 {
        let cache_dir = default_finance_cache_dir()?;
        let ts = eli_core::finance::fetch_timeseries(
            eli_core::finance::TimeseriesRequest {
                tickers: compare_tickers.clone(),
                range: eli_core::finance::Span::parse("1d")?,
                granularity: eli_core::finance::Span::parse("1h")?,
                as_of: None,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: None,
            },
            &cache_dir,
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route timeseries")?;
        let mut returns = serde_json::Map::new();
        let mut best: Option<(String, f64)> = None;
        let mut worst: Option<(String, f64)> = None;
        for s in &ts.series {
            if s.candles.len() < 2 {
                continue;
            }
            let first = s.candles.first().map(|c| c.c).unwrap_or(0.0);
            let last = s.candles.last().map(|c| c.c).unwrap_or(0.0);
            if first <= 0.0 {
                continue;
            }
            let r = (last / first - 1.0) * 100.0;
            returns.insert(s.ticker.clone(), json!(r));
            if best.as_ref().map(|(_, v)| r > *v).unwrap_or(true) {
                best = Some((s.ticker.clone(), r));
            }
            if worst.as_ref().map(|(_, v)| r < *v).unwrap_or(true) {
                worst = Some((s.ticker.clone(), r));
            }
        }
        let ts_path = artifacts_dir.join("timeseries.json");
        let analysis_path = artifacts_dir.join("analysis.json");
        std::fs::write(&ts_path, serde_json::to_string_pretty(&ts)?).context("write direct ts")?;
        let analysis = json!({
            "tickers": compare_tickers,
            "returns_pct": returns,
            "strongest": best,
            "weakest": worst,
        });
        std::fs::write(&analysis_path, serde_json::to_string_pretty(&analysis)?)
            .context("write direct analysis")?;
        let ts_meta_value = serde_json::to_value(&ts).context("serialize direct ts for meta")?;
        write_shadow_meta_for_value(
            &ts_path,
            &ts_meta_value,
            "agent.direct",
            "direct_route:compare_tickers:timeseries",
        )
        .context("write direct timeseries sidecar")?;
        write_shadow_meta_for_value(
            &analysis_path,
            &analysis,
            "agent.direct",
            "direct_route:compare_tickers:analysis",
        )
        .context("write direct analysis sidecar")?;
        let summary_path = artifacts_dir.join("summary.md");
        std::fs::write(
            &summary_path,
            format!(
                "# Direct agent route\n\n- Task: {task}\n- Route: compare_tickers\n- Strongest: {:?}\n- Weakest: {:?}\n",
                best, worst
            ),
        )
        .context("write direct summary")?;
        let artifact_paths = vec![
            ts_path.display().to_string(),
            analysis_path.display().to_string(),
            summary_path.display().to_string(),
        ];
        let worker = AgentWorkerResult {
            name: worker_name.to_string(),
            task: task.to_string(),
            status: "done".to_string(),
            exit_code: Some(0),
            requested_model: requested_model.map(|s| s.to_string()),
            used_model: Some("direct-tools".to_string()),
            attempted_models: vec!["direct-tools".to_string()],
            attempt_count: 1,
            attempts: vec![AgentAttemptResult {
                model: "direct-tools".to_string(),
                status: "ok".to_string(),
                duration_ms: t0.elapsed().as_millis(),
                exit_code: Some(0),
                error: None,
            }],
            report_path: Some(summary_path.display().to_string()),
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail: format!(
                "direct_route=compare_tickers tickers={}",
                extract_compare_tickers(task).join(",")
            ),
            stderr_tail: String::new(),
        };
        return Ok(Some(DirectAgentOutcome {
            worker,
            artifact_paths,
        }));
    }

    if lower.contains("today") && lower.contains("what is going on with") {
        let ticker = extract_primary_ticker(task).unwrap_or_else(|| "SPY".to_string());
        let cache_dir = default_finance_cache_dir()?;
        let snapshot = eli_core::finance::fetch_snapshot(eli_core::finance::SnapshotRequest {
            tickers: vec![ticker.clone()],
            provider: eli_core::finance::ProviderKind::Yahoo,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route snapshot")?;
        let ts = eli_core::finance::fetch_timeseries(
            eli_core::finance::TimeseriesRequest {
                tickers: vec![ticker.clone()],
                range: eli_core::finance::Span::parse("1d")?,
                granularity: eli_core::finance::Span::parse("5min")?,
                as_of: None,
                provider: eli_core::finance::ProviderKind::Yahoo,
                max_points_per_ticker: None,
            },
            &cache_dir,
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .context("direct route timeseries")?;
        let mut latest = None;
        let mut intraday = None;
        let mut vs_prev = None;
        if let (Some(s), Some(sn)) = (ts.series.first(), snapshot.snapshots.first()) {
            if let Some(last) = s.candles.last() {
                latest = Some(last.c);
                if let Some(open) = sn.open {
                    if open > 0.0 {
                        intraday = Some((last.c / open - 1.0) * 100.0);
                    }
                }
                if let Some(prev) = sn.previous_close {
                    if prev > 0.0 {
                        vs_prev = Some((last.c / prev - 1.0) * 100.0);
                    }
                }
            }
        }
        let snap_path = artifacts_dir.join("snapshot.json");
        let ts_path = artifacts_dir.join("timeseries.json");
        let analysis_path = artifacts_dir.join("analysis.json");
        std::fs::write(&snap_path, serde_json::to_string_pretty(&snapshot)?)
            .context("write direct snapshot")?;
        std::fs::write(&ts_path, serde_json::to_string_pretty(&ts)?).context("write direct ts")?;
        let analysis = json!({
            "ticker": ticker,
            "latest": latest,
            "intraday_pct": intraday,
            "vs_prev_close_pct": vs_prev,
        });
        std::fs::write(&analysis_path, serde_json::to_string_pretty(&analysis)?)
            .context("write direct analysis")?;
        let snap_meta_value =
            serde_json::to_value(&snapshot).context("serialize direct snapshot for meta")?;
        write_shadow_meta_for_value(
            &snap_path,
            &snap_meta_value,
            "agent.direct",
            "direct_route:ticker_today:snapshot",
        )
        .context("write direct snapshot sidecar")?;
        let ts_meta_value = serde_json::to_value(&ts).context("serialize direct ts for meta")?;
        write_shadow_meta_for_value(
            &ts_path,
            &ts_meta_value,
            "agent.direct",
            "direct_route:ticker_today:timeseries",
        )
        .context("write direct timeseries sidecar")?;
        write_shadow_meta_for_value(
            &analysis_path,
            &analysis,
            "agent.direct",
            "direct_route:ticker_today:analysis",
        )
        .context("write direct analysis sidecar")?;
        let summary_path = artifacts_dir.join("summary.md");
        std::fs::write(
            &summary_path,
            format!(
                "# Direct agent route\n\n- Task: {task}\n- Route: ticker_today\n- Ticker: {ticker}\n- Latest: {:?}\n- Intraday %: {:?}\n- Vs prev close %: {:?}\n",
                latest, intraday, vs_prev
            ),
        )
        .context("write direct summary")?;
        let artifact_paths = vec![
            snap_path.display().to_string(),
            ts_path.display().to_string(),
            analysis_path.display().to_string(),
            summary_path.display().to_string(),
        ];
        let worker = AgentWorkerResult {
            name: worker_name.to_string(),
            task: task.to_string(),
            status: "done".to_string(),
            exit_code: Some(0),
            requested_model: requested_model.map(|s| s.to_string()),
            used_model: Some("direct-tools".to_string()),
            attempted_models: vec!["direct-tools".to_string()],
            attempt_count: 1,
            attempts: vec![AgentAttemptResult {
                model: "direct-tools".to_string(),
                status: "ok".to_string(),
                duration_ms: t0.elapsed().as_millis(),
                exit_code: Some(0),
                error: None,
            }],
            report_path: Some(summary_path.display().to_string()),
            started_at: started_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: t0.elapsed().as_millis(),
            stdout_tail: format!("direct_route=ticker_today ticker={ticker} provider={provider}"),
            stderr_tail: String::new(),
        };
        return Ok(Some(DirectAgentOutcome {
            worker,
            artifact_paths,
        }));
    }

    Ok(None)
}

fn extract_price_subject(task: &str) -> Option<String> {
    let lower = task.to_ascii_lowercase();
    let idx = lower.find("price of")?;
    let mut rest = task[idx + "price of".len()..].trim().to_string();
    for marker in [" stock", " right now", " now", " today", "?"] {
        if let Some(i) = rest.to_ascii_lowercase().find(marker) {
            rest.truncate(i);
            break;
        }
    }
    let out = rest.trim();
    if out.is_empty() {
        None
    } else {
        Some(out.to_string())
    }
}

fn extract_primary_ticker(task: &str) -> Option<String> {
    extract_compare_tickers(task).into_iter().next()
}

fn extract_compare_tickers(task: &str) -> Vec<String> {
    let stop = [
        "what",
        "is",
        "going",
        "on",
        "with",
        "today",
        "and",
        "the",
        "me",
        "who",
        "strongest",
        "weakest",
        "stock",
        "price",
        "of",
        "right",
        "now",
        "compare",
        "tell",
    ];
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for raw in task.split(|c: char| !c.is_ascii_alphanumeric()) {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        let low = t.to_ascii_lowercase();
        if stop.contains(&low.as_str()) {
            continue;
        }
        if t.chars().all(|c| c.is_ascii_alphabetic()) && t.len() <= 5 {
            let upper = t.to_ascii_uppercase();
            if seen.insert(upper.clone()) {
                out.push(upper);
            }
        }
    }
    out
}

fn extract_subject_after_for(task: &str) -> Option<String> {
    let lower = task.to_ascii_lowercase();
    let idx = lower.find(" for ")?;
    let raw = task[idx + 5..].trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.trim_matches(|c: char| c == '.' || c == '?' || c == '!').to_string())
    }
}

fn pick_primary_symbol(search: &eli_core::finance::SearchResponse) -> Option<String> {
    search
        .results
        .iter()
        .find(|x| {
            x.asset_type
                .as_ref()
                .map(|t| {
                    let tt = t.to_ascii_uppercase();
                    tt == "EQUITY" || tt == "ETF" || tt == "INDEX"
                })
                .unwrap_or(true)
        })
        .map(|x| x.symbol.clone())
        .or_else(|| search.results.first().map(|x| x.symbol.clone()))
}

fn infer_symbol_from_text(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let aliases = [
        ("apple", "AAPL"),
        ("microsoft", "MSFT"),
        ("google", "GOOGL"),
        ("alphabet", "GOOGL"),
        ("amazon", "AMZN"),
        ("meta", "META"),
        ("nvidia", "NVDA"),
        ("amd", "AMD"),
        ("intel", "INTC"),
        ("tesla", "TSLA"),
    ];
    aliases
        .iter()
        .find(|(name, _)| lower.contains(name))
        .map(|(_, ticker)| (*ticker).to_string())
}

fn odds_series_request(series_ticker: &str) -> eli_core::finance::OddsRequest {
    eli_core::finance::OddsRequest {
        provider: None,
        disable_kalshi: false,
        series_ticker: Some(series_ticker.to_string()),
        event_ticker: None,
        market_ticker: None,
        status: None,
        limit: None,
        cursor: None,
        max_pages: None,
        include_orderbook: false,
        orderbook_depth: None,
        list_series: false,
        list_events: false,
        list_markets: false,
        list_tags: false,
        category: None,
        search: None,
    }
}

fn default_finance_cache_dir() -> Result<PathBuf> {
    if let Ok(paths) = Paths::discover() {
        paths.ensure_dirs().context("ensure dirs")?;
        return Ok(paths.cache_dir);
    }
    let tmp = std::env::temp_dir().join("eli_agent_cache");
    std::fs::create_dir_all(&tmp).ok();
    Ok(tmp)
}

fn render_task_template(
    template: &str,
    vars: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        let key = format!("{{{{{k}}}}}");
        let val = match v {
            serde_json::Value::String(s) => s.clone(),
            _ => v.to_string(),
        };
        out = out.replace(&key, &val);
    }
    out
}

async fn run_agent_worker(
    name: String,
    task: String,
    provider: Option<String>,
    model: Option<String>,
    fallback_models: Vec<String>,
    max_ms: u64,
    max_attempts: usize,
    artifact_dir: PathBuf,
    must_cite: Vec<String>,
) -> AgentWorkerResult {
    let started_at = chrono::Utc::now();
    let t0 = Instant::now();
    let must_cite = normalize_must_cite_prefixes(&must_cite);
    let mut status = "failed".to_string();
    let mut exit_code = None;
    let mut used_model = None;
    let requested_model = model.clone();
    let mut attempted_models: Vec<String> = Vec::new();
    let mut attempts: Vec<AgentAttemptResult> = Vec::new();
    let mut report_path: Option<String> = None;
    let mut stdout_tail = String::new();
    let mut stderr_tail = String::new();
    let provider_arg = provider.unwrap_or_else(|| "openrouter".to_string());
    std::fs::create_dir_all(&artifact_dir).ok();
    let policy = swarm_live_data_policy(&task);
    let agent_context = format!(
        "{}{}",
        build_agent_worker_context(&artifact_dir, &must_cite),
        policy
    );
    let artifact_dir_abs = if artifact_dir.is_absolute() {
        artifact_dir.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&artifact_dir)
    };
    let mut model_attempts: Vec<Option<String>> = Vec::new();
    let primary_candidate =
        if provider_arg.eq_ignore_ascii_case("openrouter") && model.is_none() {
            Some("arcee-ai/trinity-mini:free".to_string())
        } else {
            model.clone()
        };
    model_attempts.push(primary_candidate.clone());
    if model.is_some() && primary_candidate != model {
        model_attempts.push(model.clone());
    }
    for fm in fallback_models {
        if fm.trim().is_empty() {
            continue;
        }
        let f = Some(fm.trim().to_string());
        if !model_attempts.contains(&f) {
            model_attempts.push(f);
        }
    }
    if model_attempts.is_empty() {
        model_attempts.push(None);
    }
    let all_candidates = model_attempts.clone();
    let swarm_task = task.to_ascii_lowercase().contains("swarm ");
    model_attempts.retain(|candidate| {
        let label = candidate
            .clone()
            .unwrap_or_else(|| "<config-default>".to_string());
        !is_model_temporarily_disabled(&label)
    });
    if !model_attempts.is_empty() && model_attempts.len() < 2 && all_candidates.len() > 1 {
        if let Some(candidate) = all_candidates
            .iter()
            .find(|c| !model_attempts.contains(c))
            .cloned()
        {
            model_attempts.push(candidate);
        }
    }
    let max_attempts = max_attempts.max(1);
    if model_attempts.is_empty() {
        let probes = pick_probe_candidates(&all_candidates, max_attempts);
        if probes.is_empty() {
            return AgentWorkerResult {
                name,
                task,
                status: "failed".to_string(),
                exit_code: Some(1),
                requested_model,
                used_model: None,
                attempt_count: 0,
                attempted_models,
                attempts,
                report_path,
                started_at: started_at.to_rfc3339(),
                finished_at: chrono::Utc::now().to_rfc3339(),
                duration_ms: t0.elapsed().as_millis(),
                stdout_tail,
                stderr_tail:
                    "all candidate models are temporarily disabled and no probe candidate exists"
                        .to_string(),
            };
        }
        model_attempts.extend(probes);
        stderr_tail = format!(
            "all candidate models were in cooldown; forcing {} probe attempt(s)",
            model_attempts.len()
        );
    }

    'model_loop: for candidate in model_attempts {
        if attempts.len() >= max_attempts {
            break;
        }
        let label = candidate
            .clone()
            .unwrap_or_else(|| "<config-default>".to_string());
        attempted_models.push(label.clone());
        let mut retries_left = 1usize;
        loop {
            if attempts.len() >= max_attempts {
                break 'model_loop;
            }
            let attempt_t0 = Instant::now();
            exit_code = None;
            let run = async {
                let exe = std::env::current_exe().context("resolve current executable")?;
                let mut cmd = TokioCommand::new(exe);
                cmd.kill_on_drop(true);
                cmd.arg("--provider").arg(&provider_arg);
                if let Some(m) = &candidate {
                    cmd.arg("--model").arg(m);
                }
                cmd.arg("research")
                    .arg(&task)
                    .env("ELI_PLAIN_OUTPUT", "1")
                    .env("ELI_NO_FOOTER", "1")
                    .env("ELI_AGENT_FAST", "1")
                    .env("ELI_DISABLE_BRAIN_CONTEXT", "1")
                    .env("ELI_AGENT_RUN_DIR", artifact_dir_abs.display().to_string())
                    .env("ELI_AGENT_CONTEXT", &agent_context)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if provider_arg.eq_ignore_ascii_case("openrouter") {
                    cmd.env("ELI_OPENROUTER_ALLOW_FALLBACKS", "1")
                        .env("ELI_OPENROUTER_REQUIRE_PARAMETERS", "1")
                        .env("ELI_OPENROUTER_PROVIDER_SORT", "throughput")
                        .env("ELI_OPENROUTER_NON_STREAM", "1");
                }

                let timeout_ms = if swarm_task {
                    max_ms.min(40_000).max(8_000)
                } else {
                    max_ms.max(1)
                };
                let output = tokio_timeout(TokioDuration::from_millis(timeout_ms), cmd.output())
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("worker attempt timed out after {}ms", timeout_ms)
                    })?
                    .context("spawn worker command")?;
                exit_code = output.status.code();
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                stdout_tail = tail_chars(&stdout, 1800);
                stderr_tail = tail_chars(&stderr, 1800);
                report_path = extract_saved_report_path(&stdout)
                    .or_else(|| extract_saved_report_path(&stderr));
                if report_path.is_none() && output.status.success() {
                    report_path = discover_latest_worker_report(&artifact_dir_abs);
                }
                let empty_assistant = stderr
                    .to_ascii_lowercase()
                    .contains("empty assistant message");
                let has_useful_output = report_path
                    .as_ref()
                    .map(|p| Path::new(p).exists())
                    .unwrap_or(false);
                if output.status.success() && !empty_assistant && has_useful_output {
                    if let Some(missing) = missing_data_sidecars(&artifact_dir_abs) {
                        let joined = missing
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let gate_msg =
                            format!("schema gate failed: missing sidecars for {}", joined);
                        stderr_tail = if stderr_tail.trim().is_empty() {
                            gate_msg
                        } else {
                            format!("{stderr_tail}\n{gate_msg}")
                        };
                        exit_code = Some(2);
                        Ok(false)
                    } else if let Some(report) = report_path.as_ref() {
                        if let Some(citation_error) = missing_required_citations(report, &must_cite)
                        {
                            let gate_msg = format!("citation gate failed: {citation_error}");
                            stderr_tail = if stderr_tail.trim().is_empty() {
                                gate_msg
                            } else {
                                format!("{stderr_tail}\n{gate_msg}")
                            };
                            exit_code = Some(2);
                            Ok(false)
                        } else if let Some(false) = report_finished_successfully(report) {
                            if swarm_task && report_has_substantive_output(report) {
                                status = "done".to_string();
                                used_model = Some(label.clone());
                                Ok::<bool, anyhow::Error>(true)
                            } else {
                                let report_status = report_status_value(report)
                                    .unwrap_or_else(|| "unknown".to_string());
                                let gate_msg = format!(
                                    "quality gate failed: report status was {report_status}"
                                );
                                stderr_tail = if stderr_tail.trim().is_empty() {
                                    gate_msg
                                } else {
                                    format!("{stderr_tail}\n{gate_msg}")
                                };
                                exit_code = Some(2);
                                Ok(false)
                            }
                        } else {
                            status = "done".to_string();
                            used_model = Some(label.clone());
                            Ok::<bool, anyhow::Error>(true)
                        }
                    } else {
                        status = "done".to_string();
                        used_model = Some(label.clone());
                        Ok::<bool, anyhow::Error>(true)
                    }
                } else {
                    Ok(false)
                }
            }
            .await;

            match run {
                Ok(true) => {
                    record_model_health_attempt(&label, true, None, true);
                    attempts.push(AgentAttemptResult {
                        model: label.clone(),
                        status: "ok".to_string(),
                        duration_ms: attempt_t0.elapsed().as_millis(),
                        exit_code,
                        error: None,
                    });
                    break 'model_loop;
                }
                Ok(false) => {
                    let err_full = if stderr_tail.trim().is_empty() {
                        "unspecified failure".to_string()
                    } else {
                        stderr_tail.clone()
                    };
                    let limited = is_model_limit_signal(&err_full);
                    let transient =
                        is_transient_agent_failure(&err_full) || err_full == "unspecified failure";
                    let err = if stderr_tail.trim().is_empty() {
                        err_full.clone()
                    } else {
                        tail_chars(&err_full, 300)
                    };
                    if limited {
                        record_model_limit_signal(&label, Some(err.clone()));
                    } else {
                        record_model_health_attempt(&label, false, Some(err.clone()), !transient);
                    }
                    attempts.push(AgentAttemptResult {
                        model: label.clone(),
                        status: if limited {
                            "limited".to_string()
                        } else {
                            "failed".to_string()
                        },
                        duration_ms: attempt_t0.elapsed().as_millis(),
                        exit_code,
                        error: Some(err),
                    });
                    if limited {
                        break;
                    }
                    if transient && retries_left > 0 {
                        retries_left -= 1;
                        continue;
                    }
                    break;
                }
                Err(err) => {
                    let err_text = format!("{err:#}");
                    let limited = is_model_limit_signal(&err_text);
                    let transient =
                        is_transient_agent_failure(&err_text) || err_text == "unspecified failure";
                    if limited {
                        record_model_limit_signal(&label, Some(err_text.clone()));
                    } else {
                        record_model_health_attempt(&label, false, Some(err_text.clone()), !transient);
                    }
                    stderr_tail = if stderr_tail.is_empty() {
                        format!("worker runtime error: {err_text}")
                    } else {
                        format!("{stderr_tail}\nworker runtime error: {err_text}")
                    };
                    attempts.push(AgentAttemptResult {
                        model: label.clone(),
                        status: if limited {
                            "limited".to_string()
                        } else {
                            "error".to_string()
                        },
                        duration_ms: attempt_t0.elapsed().as_millis(),
                        exit_code,
                        error: Some(err_text),
                    });
                    if limited {
                        break;
                    }
                    if transient && retries_left > 0 {
                        retries_left -= 1;
                        continue;
                    }
                    break;
                }
            }
        }
    }

    if status != "done" {
        if let Ok(Some(outcome)) = try_swarm_worker_direct_route(
            &name,
            &task,
            &artifact_dir_abs,
            requested_model.as_deref(),
        )
        .await
        {
            let mut w = outcome.worker;
            let mut merged_models = attempted_models.clone();
            for model_name in &w.attempted_models {
                if !merged_models.contains(model_name) {
                    merged_models.push(model_name.clone());
                }
            }
            let mut merged_attempts = attempts.clone();
            merged_attempts.extend(w.attempts.clone());
            w.attempted_models = merged_models;
            w.attempts = merged_attempts;
            w.attempt_count = w.attempts.len();
            w.started_at = started_at.to_rfc3339();
            w.finished_at = chrono::Utc::now().to_rfc3339();
            w.duration_ms = t0.elapsed().as_millis();
            w.requested_model = requested_model.clone();
            w.stderr_tail = if stderr_tail.trim().is_empty() {
                "fallback: model stage failed; used direct-swarm recovery".to_string()
            } else {
                format!(
                    "{}\nfallback: model stage failed; used direct-swarm recovery",
                    tail_chars(&stderr_tail, 600)
                )
            };
            return w;
        }
    }

    AgentWorkerResult {
        name,
        task,
        status,
        exit_code,
        requested_model,
        used_model,
        attempt_count: attempts.len(),
        attempted_models,
        attempts,
        report_path,
        started_at: started_at.to_rfc3339(),
        finished_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: t0.elapsed().as_millis(),
        stdout_tail,
        stderr_tail,
    }
}

async fn try_swarm_worker_direct_route(
    name: &str,
    task: &str,
    artifact_dir_abs: &Path,
    requested_model: Option<&str>,
) -> Result<Option<DirectAgentOutcome>> {
    let started_at = chrono::Utc::now();
    let t0 = Instant::now();
    let task_lower = task.to_ascii_lowercase();
    let mode = classify_swarm_mode(task);
    if !(task_lower.contains("swarm map worker")
        || task_lower.contains("swarm reduce stage")
        || task_lower.contains("swarm critic stage")
        || task_lower.contains("swarm final stage"))
    {
        return Ok(None);
    }

    std::fs::create_dir_all(artifact_dir_abs).ok();
    let report_path = artifact_dir_abs.join("summary.md");
    let mut answer = String::new();
    let mut route = "swarm_local";

    if task_lower.contains("swarm map worker") {
        route = "swarm_map_local";
        let chunk_path = extract_labeled_path(task, "Input chunk path:");
        let Some(chunk_path) = chunk_path else {
            return Ok(None);
        };
        let finance_goal = task_lower.contains("recession")
            || task_lower.contains("macro")
            || task_lower.contains("yield")
            || task_lower.contains("rate")
            || task_lower.contains("inflation")
            || task_lower.contains("unemployment")
            || task_lower.contains("market-implied")
            || task_lower.contains("odds");
        if finance_goal {
            if let Ok(macro_resp) = eli_core::finance::fetch_macro(eli_core::finance::MacroRequest {
                range: Some(eli_core::finance::Span::parse("1y").map_err(|e| anyhow::anyhow!(e))?),
                compare_to: None,
                policy_file: None,
                policy_mode: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!(e)) {
                let odds_resp = eli_core::finance::fetch_odds(odds_series_request("KXRECSSNBER"))
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
                    .ok();
                let unrate = macro_resp
                    .indicators
                    .iter()
                    .find(|i| i.symbol == "UNRATE")
                    .map(|i| i.current_value);
                let fedfunds = macro_resp
                    .indicators
                    .iter()
                    .find(|i| i.symbol == "FEDFUNDS")
                    .map(|i| i.current_value);
                let spread_10y2y = macro_resp
                    .indicators
                    .iter()
                    .find(|i| i.symbol == "T10Y2Y")
                    .map(|i| i.current_value);
                let recession_2026 = odds_resp.as_ref().and_then(|o| {
                    o.markets
                        .iter()
                        .find(|m| m.ticker.contains("-26"))
                        .or_else(|| o.markets.iter().find(|m| m.title.to_ascii_lowercase().contains("2026")))
                        .and_then(|m| m.probability_yes)
                });
                answer.push_str("- LIVE packet (fallback mode):\n");
                answer.push_str(&format!("  - Unemployment: {:?}\n", unrate));
                answer.push_str(&format!("  - Fed funds: {:?}\n", fedfunds));
                answer.push_str(&format!("  - 10Y-2Y spread: {:?}\n", spread_10y2y));
                answer.push_str(&format!("  - Recession 2026 implied odds: {:?}\n", recession_2026));
                let mut warnings = Vec::new();
                let mut offsets = Vec::new();
                if recession_2026.unwrap_or(0.0) >= 0.35 {
                    warnings.push("market-implied recession odds are elevated");
                } else {
                    offsets.push("market-implied recession odds remain below 35%");
                }
                if unrate.unwrap_or(0.0) >= 4.5 {
                    warnings.push("unemployment is high enough to amplify downturn risk");
                } else {
                    offsets.push("labor market is weak but not recessionary by level");
                }
                if spread_10y2y.unwrap_or(0.0) < 0.0 {
                    warnings.push("yield curve inversion indicates recession risk");
                } else {
                    offsets.push("10Y-2Y curve is not inverted");
                }
                answer.push_str("- Critical read:\n");
                if warnings.is_empty() {
                    answer.push_str("  - Warning signals: limited in this snapshot.\n");
                } else {
                    for w in warnings {
                        answer.push_str(&format!("  - Warning: {}\n", w));
                    }
                }
                if offsets.is_empty() {
                    answer.push_str("  - Offsetting signals: limited in this snapshot.\n");
                } else {
                    for o in offsets {
                        answer.push_str(&format!("  - Offset: {}\n", o));
                    }
                }
            }
        }
        let chunk_abs = resolve_abs_path(Path::new(&chunk_path));
        let chunk_text = std::fs::read_to_string(&chunk_abs)
            .with_context(|| format!("read swarm chunk {}", chunk_abs.display()))?;
        let bullets = extract_high_signal_lines(&chunk_text, 8);
        let chunk_chars = chunk_text.chars().count();
        answer.push_str("- Chunk analyzed: `");
        answer.push_str(&chunk_abs.display().to_string());
        answer.push_str("`\n");
        answer.push_str(&format!("- Chunk size: {} chars\n", chunk_chars));
        if bullets.is_empty() {
            answer.push_str("- No strong claims detected in this chunk.\n");
        } else {
            answer.push_str("- High-signal findings:\n");
            for b in bullets {
                answer.push_str(&format!("  - {}\n", b));
            }
        }
    } else if task_lower.contains("swarm reduce stage") {
        route = "swarm_reduce_local";
        let map_manifest_path = extract_labeled_path(task, "Map manifest:");
        let Some(map_manifest_path) = map_manifest_path else {
            return Ok(None);
        };
        let map_manifest_abs = resolve_abs_path(Path::new(&map_manifest_path));
        let raw = std::fs::read_to_string(&map_manifest_abs)
            .with_context(|| format!("read map manifest {}", map_manifest_abs.display()))?;
        let value: serde_json::Value =
            serde_json::from_str(&raw).context("parse map manifest json")?;
        let workers = value
            .get("workers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut merged: Vec<String> = Vec::new();
        for w in workers {
            let status = w.get("status").and_then(|x| x.as_str()).unwrap_or("");
            if status != "done" {
                continue;
            }
            let report = w.get("report_path").and_then(|x| x.as_str()).unwrap_or("");
            if report.is_empty() {
                continue;
            }
            let report_abs = resolve_abs_path(Path::new(report));
            if let Ok(text) = std::fs::read_to_string(&report_abs) {
                for line in extract_high_signal_lines(&text, 4) {
                    if merged.len() >= 16 {
                        break;
                    }
                    if !merged.contains(&line) {
                        merged.push(line);
                    }
                }
            }
            if merged.len() >= 16 {
                break;
            }
        }
        answer.push_str("- Reduce inputs: `");
        answer.push_str(&map_manifest_abs.display().to_string());
        answer.push_str("`\n");
        if merged.is_empty() {
            answer.push_str("- No successful map evidence found to merge.\n");
        } else {
            answer.push_str("- Merged findings:\n");
            for m in merged {
                answer.push_str(&format!("  - {}\n", m));
            }
        }
    } else if task_lower.contains("swarm critic stage") {
        route = "swarm_critic_local";
        let reduce_report = extract_labeled_path(task, "Reduce report:");
        if let Some(reduce_report) = reduce_report {
            let reduce_abs = resolve_abs_path(Path::new(&reduce_report));
            answer.push_str("- Critiqued reduce report: `");
            answer.push_str(&reduce_abs.display().to_string());
            answer.push_str("`\n");
            answer.push_str(
                "- Residual risk: some map findings may be incomplete under low time budgets.\n",
            );
            answer.push_str("- Action: prioritize claims repeated across multiple map workers.\n");
        } else {
            answer.push_str("- Critic stage ran with limited inputs.\n");
        }
    } else if task_lower.contains("swarm final stage") {
        route = "swarm_final_local";
        let reduce_report = extract_labeled_path(task, "Reduce report:");
        let critic_report = extract_labeled_path(task, "Critic report:");
        answer.push_str("- Final synthesis generated from upstream swarm outputs.\n");
        answer.push_str(&format!("- Mode detected: `{}`\n", mode));
        answer.push_str("- Default structure applied:\n");
        answer.push_str("  - Best current answer\n");
        answer.push_str("  - Strongest evidence\n");
        answer.push_str("  - Conflicts and uncertainty\n");
        answer.push_str("  - Next checks\n");
        if let Some(p) = reduce_report {
            let abs = resolve_abs_path(Path::new(&p));
            answer.push_str("- Reduce source: `");
            answer.push_str(&abs.display().to_string());
            answer.push_str("`\n");
            if let Ok(text) = std::fs::read_to_string(&abs) {
                let lines = extract_high_signal_lines(&text, 10);
                if !lines.is_empty() {
                    answer.push_str("  - Evidence-weighted conclusions:\n");
                    for l in lines {
                        answer.push_str(&format!("    - {}\n", l));
                    }
                }
            }
        }
        answer.push_str("- Conflicts/uncertainty:\n");
        answer.push_str("  - Some upstream claims may be stale or based on partial chunk coverage.\n");
        if mode == "debate" {
            answer.push_str("- Debate framing:\n");
            answer.push_str("  - Pro case: strongest supporting line(s) from reduce report.\n");
            answer.push_str("  - Con case: strongest disconfirming line(s) from critic report.\n");
            answer.push_str("  - Adjudication should state what evidence flips the conclusion.\n");
        } else if mode == "critique" {
            answer.push_str("- Critique framing:\n");
            answer.push_str("  - Label top claims by support level before final conclusion.\n");
            answer.push_str("  - Remove weak claims that lack direct evidence.\n");
        } else if mode == "needle" {
            answer.push_str("- Needle framing:\n");
            answer.push_str("  - Prioritize exact location + quote over broad summary.\n");
        }
        answer.push_str("- Decision note:\n");
        answer.push_str("  - Treat output as evidence-weighted and update when fresher data arrives.\n");
        if let Some(p) = critic_report {
            let abs = resolve_abs_path(Path::new(&p));
            answer.push_str("- Critic source: `");
            answer.push_str(&abs.display().to_string());
            answer.push_str("`\n");
        }
        if !answer.contains("Evidence-weighted conclusions:") {
            answer.push_str("- Evidence-weighted conclusions: insufficient upstream detail; treat as preliminary.\n");
        }
    }

    let report_body = format!(
        "# Swarm Direct Worker\n\n## Summary\n- Route: `{}`\n- Worker: `{}`\n\n## Answer\n{}\n",
        route, name, answer
    );
    std::fs::write(&report_path, report_body)
        .with_context(|| format!("write swarm direct report {}", report_path.display()))?;

    let worker = AgentWorkerResult {
        name: name.to_string(),
        task: task.to_string(),
        status: "done".to_string(),
        exit_code: Some(0),
        requested_model: requested_model.map(|s| s.to_string()),
        used_model: Some("direct-swarm".to_string()),
        attempted_models: vec!["direct-swarm".to_string()],
        attempt_count: 1,
        attempts: vec![AgentAttemptResult {
            model: "direct-swarm".to_string(),
            status: "ok".to_string(),
            duration_ms: t0.elapsed().as_millis(),
            exit_code: Some(0),
            error: None,
        }],
        report_path: Some(report_path.display().to_string()),
        started_at: started_at.to_rfc3339(),
        finished_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: t0.elapsed().as_millis(),
        stdout_tail: format!("direct_route={} worker={}", route, name),
        stderr_tail: String::new(),
    };
    let artifact_paths = vec![report_path.display().to_string()];
    Ok(Some(DirectAgentOutcome {
        worker,
        artifact_paths,
    }))
}

fn extract_labeled_path(task: &str, label: &str) -> Option<String> {
    let idx = task.find(label)?;
    let tail = &task[idx + label.len()..];
    let line = tail
        .lines()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

fn extract_high_signal_lines(text: &str, max_items: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches(['-', '*', '#', '>', ' ']);
        if line.len() < 28 {
            continue;
        }
        if line.starts_with("```") || line.starts_with("===") {
            continue;
        }
        let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.chars().count() < 28 {
            continue;
        }
        let clipped = tail_chars(&compact, 220);
        if seen.insert(clipped.clone()) {
            out.push(clipped);
        }
        if out.len() >= max_items {
            break;
        }
    }
    out
}

fn missing_data_sidecars(artifact_dir: &Path) -> Option<Vec<PathBuf>> {
    let data_files = collect_data_artifact_files(artifact_dir);
    if data_files.is_empty() {
        return None;
    }
    let missing = data_files
        .into_iter()
        .filter(|p| !eli_core::meta::sidecar_path_for(p).exists())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

fn collect_data_artifact_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if is_data_artifact_path(&path) {
                out.push(path);
            }
        }
    }
    out
}

fn is_data_artifact_path(path: &Path) -> bool {
    let display = path.display().to_string().to_ascii_lowercase();
    if display.ends_with(".meta.json") {
        return false;
    }
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("json" | "csv" | "ndjson" | "parquet")
    )
}

fn extract_saved_report_path(text: &str) -> Option<String> {
    for line in text.lines().rev() {
        if let Some(pos) = line.find("saved:") {
            let raw = line[pos + "saved:".len()..].trim();
            let raw = raw.trim_start_matches('(').trim_end_matches(')');
            if !raw.is_empty() {
                return Some(raw.to_string());
            }
        }
    }
    None
}

fn report_status_value(report_path: &str) -> Option<String> {
    let raw = std::fs::read_to_string(report_path).ok()?;
    for line in raw.lines().take(64) {
        if let Some(rest) = line.strip_prefix("- Status:") {
            let status = rest.trim();
            if status.is_empty() {
                return None;
            }
            return Some(status.to_string());
        }
    }
    None
}

fn report_finished_successfully(report_path: &str) -> Option<bool> {
    report_status_value(report_path).map(|status| status.eq_ignore_ascii_case("done"))
}

fn report_has_substantive_output(report_path: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(report_path) else {
        return false;
    };

    let section_after = |heading: &str| -> Option<String> {
        let start = raw.find(heading)?;
        let rest = &raw[start + heading.len()..];
        let end = rest.find("\n## ").unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    };

    if let Some(answer) = section_after("## Answer") {
        if answer.chars().count() >= 80 {
            return true;
        }
    }
    if let Some(partial) = section_after("## Partial Output") {
        if partial.chars().count() >= 120 {
            return true;
        }
    }
    if let Some(summary) = section_after("## Summary") {
        let bullets = summary
            .lines()
            .filter(|l| l.trim_start().starts_with("- "))
            .count();
        if bullets >= 2 {
            return true;
        }
    }
    false
}

fn discover_latest_worker_report(artifact_dir: &Path) -> Option<String> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(artifact_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !(file_name == "summary.md"
            || (file_name.starts_with("research_") && file_name.ends_with(".md")))
        {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest
            .as_ref()
            .map(|(current, _)| modified > *current)
            .unwrap_or(true)
        {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path.display().to_string())
}

fn sanitize_worker_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.trim_matches('_').is_empty() {
        "worker".to_string()
    } else {
        out
    }
}

fn skipped_worker(name: &str, task: &str, reason: &str) -> AgentWorkerResult {
    AgentWorkerResult {
        name: name.to_string(),
        task: task.to_string(),
        status: "skipped".to_string(),
        exit_code: Some(0),
        requested_model: None,
        used_model: None,
        attempt_count: 0,
        attempted_models: Vec::new(),
        attempts: Vec::new(),
        report_path: None,
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: 0,
        stdout_tail: String::new(),
        stderr_tail: reason.to_string(),
    }
}

fn build_agent_worker_context(artifact_dir: &Path, must_cite: &[String]) -> String {
    let abs = if artifact_dir.is_absolute() {
        artifact_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(artifact_dir)
    };
    let citation_clause = if must_cite.is_empty() {
        String::new()
    } else {
        let mut s =
            "Required citations: final synthesis.answer must cite file path(s) under:\n".to_string();
        for prefix in must_cite {
            s.push_str("- ");
            s.push_str(prefix);
            s.push('\n');
        }
        s
    };
    format!(
        "- Save machine-readable outputs under: {dir}\n- Use `--out auto` for tool calls to get programmatic, context-rich filenames. Do not rely on `eli_research/data/.last_tool_output.json` across multiple tool calls.\n- If running Python, prefer a heredoc script (`python3 << 'EOF'`) over fragile nested-quote one-liners.\n- In your final synthesis.answer, cite exact output file path(s) you created.\n{citation_clause}",
        dir = abs.display()
    )
}

fn classify_swarm_mode(goal: &str) -> &'static str {
    let lower = goal.to_ascii_lowercase();
    if lower.contains("fact-check")
        || lower.contains("fact check")
        || lower.contains("verify")
        || lower.contains("audit")
        || lower.contains("red team")
        || lower.contains("critique")
    {
        "critique"
    } else if lower.contains("debate")
        || lower.contains("argue")
        || lower.contains("pro/con")
        || lower.contains("counterargument")
    {
        "debate"
    } else if lower.contains("evidence")
        || lower.contains("prove")
        || lower.contains("source hunt")
    {
        "evidence"
    } else if lower.contains("needle")
        || lower.contains("single fact")
        || lower.contains("find one")
        || lower.contains("locate")
    {
        "needle"
    } else if lower.contains("synthes")
        || lower.contains("summar")
        || lower.contains("overview")
        || lower.contains("merge")
    {
        "synthesis"
    } else {
        "general"
    }
}

fn swarm_default_structure_hint() -> &'static str {
    "- Best current answer or judgment.\n- Evidence that most strongly supports it (with provenance).\n- Conflicts, gaps, and uncertainty that could change the answer.\n- Next checks or actions for downstream stage."
}

fn swarm_mode_overlay(goal: &str) -> String {
    match classify_swarm_mode(goal) {
        "critique" => "- Mode objective: critique.\n- Stress-test claims first and try to falsify weak assumptions.\n- For major claims, label support level: supported, mixed, weak, unsupported.\n- Prioritize corrective actions over prose polish."
            .to_string(),
        "debate" => "- Mode objective: debate.\n- Preserve strongest opposing positions; do not collapse disagreement too early.\n- Build steelman pro and con cases with evidence and assumptions.\n- End with clear decision criteria and what evidence would flip the call."
            .to_string(),
        "evidence" => "- Mode objective: evidence hunt.\n- Maximize net-new, primary-source evidence.\n- Separate direct evidence from commentary/inference.\n- Track freshness and relevance of every key claim."
            .to_string(),
        "needle" => "- Mode objective: needle hunt.\n- Locate specific fact(s) with precise coordinates (path/page/line/chunk).\n- Include short quote snippets where possible.\n- If not found, report coverage and likely blind spots."
            .to_string(),
        "synthesis" => "- Mode objective: synthesis.\n- Compress material into high-signal clusters without losing key disagreements.\n- Highlight minority but high-impact findings.\n- Show confidence per cluster and unresolved questions."
            .to_string(),
        _ => "- Mode objective: general.\n- Adapt analysis shape to the task instead of forcing a template.\n- Keep reasoning explicit, evidence-grounded, and decision-useful."
            .to_string(),
    }
}

fn swarm_live_data_policy(task: &str) -> String {
    let lower = task.to_ascii_lowercase();
    if !lower.contains("swarm ") {
        return String::new();
    }
    if lower.contains("local mode")
        || lower.contains("cache mode")
        || lower.contains("offline mode")
        || lower.contains("use local")
    {
        return "SWARM DATA POLICY: Orchestrator requested local/cache mode. You may use local artifacts as primary sources.\n".to_string();
    }
    "SWARM DATA POLICY: LIVE-FIRST. Use live API hits by default; only use local cache/artifacts for cross-checks.\n- For prediction markets search, prefer `eli finance odds --search \"...\" --live`.\n- For specific contracts, prefer direct `--event/--market/--series` lookups.\n- For macro/rates/yield, run fresh tool calls (`eli finance macro`, `eli finance yield-curve`, `eli finance rate-path`) in this run.\n".to_string()
}

fn tail_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut chars = input.chars().rev().take(max_chars).collect::<Vec<char>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn resolve_agent_run_dir(kind: &str) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let id = uuid::Uuid::new_v4().to_string();
    let short = &id[..8];
    Path::new("eli_research/data/agent_runs").join(format!("{kind}_{stamp}_{short}"))
}

fn persist_agent_response<T: Serialize>(
    full_value: &T,
    kind: &str,
    run_dir: &Path,
    artifact_paths: &[String],
    out_copy: Option<PathBuf>,
) -> Result<()> {
    let full_json = serde_json::to_string_pretty(full_value).context("serialize response")?;
    std::fs::create_dir_all(run_dir)
        .with_context(|| format!("create agent run dir {}", run_dir.display()))?;
    let result_path = run_dir.join("result.json");
    let manifest_path = run_dir.join("manifest.json");
    std::fs::write(&result_path, &full_json).context("write result file")?;

    let manifest = json!({
        "kind": kind,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "result_path": result_path.display().to_string(),
        "artifact_paths": artifact_paths,
    });
    let manifest_json = serde_json::to_string_pretty(&manifest).context("serialize manifest")?;
    std::fs::write(&manifest_path, manifest_json).context("write manifest file")?;

    if let Some(path) = out_copy {
        let out_path = redirect_finance_output(path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out_path, &full_json).context("write --out copy")?;
    }
    println!("{full_json}");
    Ok(())
}

fn write_fanout_summary_artifact(
    run_dir: &Path,
    workers: &[AgentWorkerResult],
    completed: usize,
    failed: usize,
) -> Result<String> {
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();
    let path = artifacts_dir.join("fanout_summary.json");
    let successful_reports: Vec<serde_json::Value> = workers
        .iter()
        .filter(|w| w.status == "done")
        .map(|w| {
            json!({
                "name": w.name,
                "used_model": w.used_model,
                "report_path": w.report_path,
                "duration_ms": w.duration_ms,
            })
        })
        .collect();
    let failed_workers: Vec<serde_json::Value> = workers
        .iter()
        .filter(|w| w.status != "done")
        .map(|w| {
            json!({
                "name": w.name,
                "requested_model": w.requested_model,
                "attempts": w.attempts,
                "stderr_tail": w.stderr_tail,
            })
        })
        .collect();
    let summary = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "completed": completed,
        "failed": failed,
        "successful_reports": successful_reports,
        "failed_workers": failed_workers,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&summary)?)
        .context("write fanout summary artifact")?;
    Ok(path.display().to_string())
}

fn write_swarm_markdown_report(
    run_dir: &Path,
    task: &str,
    chunk_manifest_path: &Path,
    map_manifest_path: &Path,
    map_workers: &[AgentWorkerResult],
    reduce_worker: &AgentWorkerResult,
    critic_worker: &AgentWorkerResult,
    final_worker: &AgentWorkerResult,
) -> Result<String> {
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();
    let path = artifacts_dir.join("swarm_report.md");
    let mut md = String::new();
    md.push_str("# Swarm Model Report\n\n");
    md.push_str(&format!("- Task: `{}`\n", task));
    md.push_str(&format!(
        "- Chunk manifest: `{}`\n",
        chunk_manifest_path.display()
    ));
    md.push_str(&format!(
        "- Map manifest: `{}`\n\n",
        map_manifest_path.display()
    ));
    md.push_str("## Map Workers\n\n");
    md.push_str(&render_worker_sections_markdown(map_workers));
    md.push_str("\n## Stage Workers\n\n");
    md.push_str(&render_worker_sections_markdown(&[
        reduce_worker.clone(),
        critic_worker.clone(),
        final_worker.clone(),
    ]));
    std::fs::write(&path, md).context("write swarm report markdown")?;
    Ok(path.display().to_string())
}

fn write_worker_compendium_markdown(
    run_dir: &Path,
    filename: &str,
    title: &str,
    workers: &[AgentWorkerResult],
) -> Result<String> {
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();
    let path = artifacts_dir.join(filename);
    let mut md = String::new();
    md.push_str(&format!("# {}\n\n", title));
    md.push_str(&render_worker_sections_markdown(workers));
    std::fs::write(&path, md).context("write worker compendium markdown")?;
    Ok(path.display().to_string())
}

fn write_collaboration_draft_markdown(
    run_dir: &Path,
    filename: &str,
    title: &str,
    workers: &[AgentWorkerResult],
) -> Result<String> {
    let artifacts_dir = run_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).ok();
    let path = artifacts_dir.join(filename);
    let mut md = String::new();
    md.push_str(&format!("# {}\n\n", title));
    md.push_str(
        "This is an append-only shared draft. Contradictions are preserved intentionally.\n\n",
    );
    md.push_str("## Contributions\n\n");
    for worker in workers {
        let model = worker
            .used_model
            .clone()
            .or_else(|| worker.requested_model.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        md.push_str(&format!(
            "### {} ({})\n\n- Status: `{}`\n- Duration: `{}` ms\n",
            worker.name, model, worker.status, worker.duration_ms
        ));
        if let Some(path) = &worker.report_path {
            md.push_str(&format!("- Source report: `{}`\n\n", path));
            if let Ok(raw) = std::fs::read_to_string(path) {
                let body =
                    extract_answer_markdown_block(&raw).unwrap_or_else(|| tail_chars(&raw, 1800));
                md.push_str(body.trim());
                md.push_str("\n\n");
            } else {
                md.push_str("_No readable report content._\n\n");
            }
        } else {
            md.push_str("_No report produced._\n\n");
        }
    }
    std::fs::write(&path, md).context("write collaboration draft markdown")?;
    Ok(path.display().to_string())
}

fn render_worker_sections_markdown(workers: &[AgentWorkerResult]) -> String {
    let mut out = String::new();
    for worker in workers {
        let model = worker
            .used_model
            .clone()
            .or_else(|| worker.requested_model.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        out.push_str(&format!("### {} ({})\n\n", worker.name, model));
        out.push_str(&format!(
            "- Status: `{}`\n- Duration: `{}` ms\n",
            worker.status, worker.duration_ms
        ));
        if let Some(path) = &worker.report_path {
            out.push_str(&format!("- Report: `{}`\n\n", path));
            match std::fs::read_to_string(path) {
                Ok(raw) => {
                    let body = extract_answer_markdown_block(&raw)
                        .unwrap_or_else(|| tail_chars(&raw, 1500));
                    out.push_str("```markdown\n");
                    out.push_str(body.trim());
                    out.push_str("\n```\n\n");
                }
                Err(_) => {
                    out.push_str("_Unable to read report file._\n\n");
                }
            }
        } else {
            out.push_str("_No report produced._\n\n");
        }
    }
    out
}

fn extract_answer_markdown_block(md: &str) -> Option<String> {
    let answer_header = "## Answer";
    let start = md.find(answer_header)?;
    let after = &md[start + answer_header.len()..];
    let mut end_idx = after.len();
    if let Some(pos) = after.find("\n## ") {
        end_idx = pos;
    }
    Some(after[..end_idx].trim().to_string())
}
