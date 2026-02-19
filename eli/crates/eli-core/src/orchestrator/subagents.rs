pub async fn run_subagents(
    adapter: Arc<dyn LlmAdapter>,
    cfg: &ChatConfig,
    memory: &Memory,
    tasks: &[SubagentTask],
) -> Vec<SubagentResult> {
    if tasks.is_empty() {
        return Vec::new();
    }

    let context = build_subagent_context(memory);
    let cfg = cfg.clone();
    let tasks: Vec<SubagentTask> = tasks.to_vec();
    let max_parallel = cfg.resolved_parallel_subagents();

    let stream = stream::iter(tasks.into_iter().enumerate()).map(|(idx, task)| {
        let adapter = adapter.clone();
        let cfg = cfg.clone();
        let context = context.clone();
        async move {
            let res = run_one_subagent(adapter, cfg, task, context).await;
            (idx, res)
        }
    });

    let mut out: Vec<(usize, SubagentResult)> =
        stream.buffer_unordered(max_parallel).collect().await;
    out.sort_by_key(|(idx, _)| *idx);
    out.into_iter().map(|(_, result)| result).collect()
}

async fn run_one_subagent(
    adapter: Arc<dyn LlmAdapter>,
    cfg: ChatConfig,
    task: SubagentTask,
    context: String,
) -> SubagentResult {
    let name = task.name.trim().to_string();
    let task_text = task.task.trim().to_string();
    if name.is_empty() || task_text.is_empty() {
        return SubagentResult {
            name: if name.is_empty() {
                "subagent".to_string()
            } else {
                name
            },
            output: String::new(),
            error: Some("empty subagent name or task".to_string()),
        };
    }

    let prompt = format!(
        "Task:\n{task}\n\nContext:\n{context}",
        task = task_text,
        context = context
    );

    let req = ChatRequest {
        model: task
            .model
            .as_deref()
            .unwrap_or(cfg.model.as_str())
            .to_string(),
        messages: vec![
            ChatMessage::system(subagent_system_prompt(&name)),
            ChatMessage::user(prompt),
        ],
        temperature: task.temperature.or(cfg.temperature).or(Some(0.2)),
        max_tokens: task.max_tokens.or(Some(800)),
        response_format: None,
        stream: false,
    };

    match adapter.chat(req).await {
        Ok(out) => SubagentResult {
            name,
            output: truncate_chars(out.trim(), SUMMARY_OUTPUT_MAX_CHARS),
            error: None,
        },
        Err(e) => SubagentResult {
            name,
            output: String::new(),
            error: Some(e.to_string()),
        },
    }
}

