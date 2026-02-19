async fn cmd_web(cmd: WebCommand) -> Result<()> {
    match cmd {
        WebCommand::Crawl(args) => cmd_web_crawl(args).await,
        WebCommand::Search(args) => cmd_web_search(args).await,
        WebCommand::Read(args) => cmd_web_read(args).await,
        WebCommand::Extract(args) => cmd_web_extract(args).await,
    }
}


fn generate_struct_getters_quote(item: &syn::Item) -> Result<String> {
    let syn::Item::Struct(s) = item else {
        anyhow::bail!("template struct_getters expects a struct item");
    };

    let struct_ident = &s.ident;
    let fields_named = match &s.fields {
        syn::Fields::Named(named) => named,
        _ => anyhow::bail!("template struct_getters requires named fields"),
    };

    let mut methods = Vec::new();
    for f in &fields_named.named {
        let Some(field_ident) = &f.ident else {
            continue;
        };
        let field_ty = &f.ty;
        methods.push(quote! {
            pub fn #field_ident(&self) -> &#field_ty {
                &self.#field_ident
            }
        });
    }

    let tokens = quote! {
        impl #struct_ident {
            #(#methods)*
        }
    };
    Ok(tokens.to_string())
}

fn summarize_rust_file(file: &syn::File) -> RustFileSummary {
    let mut summary = RustFileSummary {
        items_total: file.items.len(),
        functions: 0,
        function_names: Vec::new(),
        structs: 0,
        struct_names: Vec::new(),
        enums: 0,
        enum_names: Vec::new(),
        impls: 0,
        impl_targets: Vec::new(),
        traits: 0,
        trait_names: Vec::new(),
        modules: 0,
        module_names: Vec::new(),
        uses: 0,
        use_paths: Vec::new(),
        consts: 0,
        const_names: Vec::new(),
        statics: 0,
        type_aliases: 0,
        type_alias_names: Vec::new(),
        macros: 0,
        others: 0,
    };

    for item in &file.items {
        match item {
            syn::Item::Fn(v) => {
                summary.functions += 1;
                summary.function_names.push(v.sig.ident.to_string());
            }
            syn::Item::Struct(v) => {
                summary.structs += 1;
                summary.struct_names.push(v.ident.to_string());
            }
            syn::Item::Enum(v) => {
                summary.enums += 1;
                summary.enum_names.push(v.ident.to_string());
            }
            syn::Item::Impl(v) => {
                summary.impls += 1;
                summary.impl_targets.push(format_impl_target(v));
            }
            syn::Item::Trait(v) => {
                summary.traits += 1;
                summary.trait_names.push(v.ident.to_string());
            }
            syn::Item::Mod(v) => {
                summary.modules += 1;
                summary.module_names.push(v.ident.to_string());
            }
            syn::Item::Use(v) => {
                summary.uses += 1;
                summary.use_paths.push(use_tree_to_string(&v.tree));
            }
            syn::Item::Const(v) => {
                summary.consts += 1;
                summary.const_names.push(v.ident.to_string());
            }
            syn::Item::Static(_) => summary.statics += 1,
            syn::Item::Type(v) => {
                summary.type_aliases += 1;
                summary.type_alias_names.push(v.ident.to_string());
            }
            syn::Item::Macro(_) => summary.macros += 1,
            _ => summary.others += 1,
        }
    }

    summary.function_names.sort();
    summary.struct_names.sort();
    summary.enum_names.sort();
    summary.impl_targets.sort();
    summary.trait_names.sort();
    summary.module_names.sort();
    summary.use_paths.sort();
    summary.const_names.sort();
    summary.type_alias_names.sort();

    summary
}

fn format_impl_target(item: &syn::ItemImpl) -> String {
    let self_ty = type_to_string(&item.self_ty);
    if let Some((_, trait_path, _)) = &item.trait_ {
        return format!("{} for {}", path_to_string(trait_path), self_ty);
    }
    format!("impl {}", self_ty)
}

fn type_to_string(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(t) => path_to_string(&t.path),
        syn::Type::Reference(t) => format!("&{}", type_to_string(&t.elem)),
        syn::Type::Slice(t) => format!("[{}]", type_to_string(&t.elem)),
        syn::Type::Array(t) => format!("[{}; _]", type_to_string(&t.elem)),
        syn::Type::Tuple(t) => {
            let parts = t.elems.iter().map(type_to_string).join(", ");
            format!("({parts})")
        }
        _ => "other".to_string(),
    }
}

fn path_to_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|seg| seg.ident.to_string())
        .join("::")
}

fn use_tree_to_string(tree: &syn::UseTree) -> String {
    match tree {
        syn::UseTree::Path(v) => format!("{}::{}", v.ident, use_tree_to_string(&v.tree)),
        syn::UseTree::Name(v) => v.ident.to_string(),
        syn::UseTree::Rename(v) => format!("{} as {}", v.ident, v.rename),
        syn::UseTree::Glob(_) => "*".to_string(),
        syn::UseTree::Group(v) => {
            let inner = v.items.iter().map(use_tree_to_string).join(", ");
            format!("{{{inner}}}")
        }
    }
}

#[derive(Debug, Clone)]
struct TopLevelFnSpan {
    name: String,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct RustFileAnalysis {
    source_path: String,
    bytes: usize,
    line_count: usize,
    summary: RustFileSummary,
    largest_fn_span: usize,
    largest_fn_name: Option<String>,
    largest_fn_start: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct RustCodeFileMetric {
    source_path: String,
    line_count: usize,
    bytes: usize,
    items_total: usize,
    functions: usize,
    modules: usize,
    structs: usize,
    enums: usize,
    impls: usize,
    traits: usize,
    largest_fn_span: usize,
    largest_fn_name: Option<String>,
    largest_fn_start: Option<usize>,
    functions_per_kloc: f64,
}

#[derive(Debug, Clone, Serialize)]
struct RustCodeParseError {
    source_path: String,
    error: String,
}

#[derive(Debug, Clone, Serialize)]
struct RustCodeBatchTotals {
    requested_files: usize,
    analyzed_files: usize,
    parse_errors: usize,
    total_line_count: usize,
    total_bytes: usize,
    total_items: usize,
    total_functions: usize,
    total_modules: usize,
    max_largest_fn_span: usize,
}

#[derive(Debug, Clone, Serialize)]
struct RustCodeBatchHotspots {
    largest_files: Vec<RustCodeFileMetric>,
    largest_function_spans: Vec<RustCodeFileMetric>,
    function_dense_files: Vec<RustCodeFileMetric>,
}

static TOP_LEVEL_FN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")
        .expect("valid top-level fn regex")
});

fn top_level_fn_spans(source: &str) -> Vec<TopLevelFnSpan> {
    let lines: Vec<&str> = source.lines().collect();
    let mut starts: Vec<TopLevelFnSpan> = Vec::new();
    let mut depth: i64 = 0;
    for (idx, raw) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim_start();
        if depth == 0 {
            if let Some(caps) = TOP_LEVEL_FN_RE.captures(line) {
                starts.push(TopLevelFnSpan {
                    name: caps
                        .get(1)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default(),
                    start: line_no,
                    end: line_no,
                });
            }
        }

        depth += raw.matches('{').count() as i64;
        depth -= raw.matches('}').count() as i64;
        if depth < 0 {
            depth = 0;
        }
    }

    for idx in 0..starts.len() {
        if idx + 1 < starts.len() {
            starts[idx].end = starts[idx + 1].start.saturating_sub(1);
        } else {
            starts[idx].end = lines.len();
        }
    }
    starts
}

fn analyze_rust_file(source_path: &Path) -> Result<RustFileAnalysis> {
    let source = std::fs::read_to_string(source_path)
        .with_context(|| format!("read {}", source_path.display()))?;
    let parsed = syn::parse_file(&source)
        .with_context(|| format!("parse rust file {}", source_path.display()))?;

    let summary = summarize_rust_file(&parsed);
    let spans = top_level_fn_spans(&source);

    let mut largest_fn_span = 0usize;
    let mut largest_fn_name = None;
    let mut largest_fn_start = None;
    for span in spans {
        let current = span.end.saturating_sub(span.start) + 1;
        if current > largest_fn_span {
            largest_fn_span = current;
            largest_fn_name = Some(span.name);
            largest_fn_start = Some(span.start);
        }
    }

    Ok(RustFileAnalysis {
        source_path: source_path.display().to_string(),
        bytes: source.as_bytes().len(),
        line_count: source.lines().count(),
        summary,
        largest_fn_span,
        largest_fn_name,
        largest_fn_start,
    })
}

fn generate_struct_getters_for_file(source_path: &Path) -> Result<Option<String>> {
    let source = std::fs::read_to_string(source_path)
        .with_context(|| format!("read {}", source_path.display()))?;
    let parsed = syn::parse_file(&source)
        .with_context(|| format!("parse rust file {}", source_path.display()))?;

    let mut generated_parts = Vec::new();
    for item in &parsed.items {
        if let Ok(code) = generate_struct_getters_quote(item) {
            generated_parts.push(code);
        }
    }
    if generated_parts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(generated_parts.join("\n\n")))
    }
}

fn should_skip_code_scan_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "target_refactor"
            | "target_local"
            | ".cargo_local_local"
            | "node_modules"
    )
}

fn collect_rust_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("read_dir {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("file_type {}", path.display()))?;
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if should_skip_code_scan_dir(&name) {
                    continue;
                }
                stack.push(path);
                continue;
            }

            if file_type.is_file()
                && path.extension().and_then(|v| v.to_str()) == Some("rs")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn to_code_file_metric(analysis: &RustFileAnalysis) -> RustCodeFileMetric {
    let functions_per_kloc = if analysis.line_count == 0 {
        0.0
    } else {
        (analysis.summary.functions as f64 / analysis.line_count as f64) * 1000.0
    };
    RustCodeFileMetric {
        source_path: analysis.source_path.clone(),
        line_count: analysis.line_count,
        bytes: analysis.bytes,
        items_total: analysis.summary.items_total,
        functions: analysis.summary.functions,
        modules: analysis.summary.modules,
        structs: analysis.summary.structs,
        enums: analysis.summary.enums,
        impls: analysis.summary.impls,
        traits: analysis.summary.traits,
        largest_fn_span: analysis.largest_fn_span,
        largest_fn_name: analysis.largest_fn_name.clone(),
        largest_fn_start: analysis.largest_fn_start,
        functions_per_kloc,
    }
}

fn truncate_ranked(mut rows: Vec<RustCodeFileMetric>, top: usize) -> Vec<RustCodeFileMetric> {
    if top == 0 {
        return Vec::new();
    }
    rows.truncate(top);
    rows
}

async fn cmd_code_batch(args: &CodeArgs, root_path: &Path) -> Result<serde_json::Value> {
    if args.generate {
        anyhow::bail!("--generate works only for a single Rust file path");
    }

    let mut files = collect_rust_files(root_path)?;
    if let Some(max_files) = args.max_files {
        files.truncate(max_files);
    }
    if files.is_empty() {
        anyhow::bail!("no Rust files found under {}", root_path.display());
    }
    let requested_files = files.len();

    let workers = args
        .workers
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .max(1);

    let mut analyses: Vec<RustFileAnalysis> = Vec::new();
    let mut parse_errors: Vec<RustCodeParseError> = Vec::new();
    let results = futures::stream::iter(files.into_iter().map(|path| {
        tokio::task::spawn_blocking(move || {
            let display = path.display().to_string();
            let analyzed = analyze_rust_file(&path).map_err(|e| format!("{e:#}"));
            (display, analyzed)
        })
    }))
    .buffer_unordered(workers)
    .collect::<Vec<_>>()
    .await;

    for result in results {
        match result {
            Ok((_path, Ok(analysis))) => {
                if analysis.line_count >= args.min_loc {
                    analyses.push(analysis);
                }
            }
            Ok((path, Err(error))) => parse_errors.push(RustCodeParseError {
                source_path: path,
                error,
            }),
            Err(error) => parse_errors.push(RustCodeParseError {
                source_path: root_path.display().to_string(),
                error: format!("join error: {error}"),
            }),
        }
    }

    analyses.sort_by(|a, b| {
        b.line_count
            .cmp(&a.line_count)
            .then_with(|| a.source_path.cmp(&b.source_path))
    });

    let file_metrics: Vec<RustCodeFileMetric> = analyses.iter().map(to_code_file_metric).collect();
    let totals = RustCodeBatchTotals {
        requested_files,
        analyzed_files: file_metrics.len(),
        parse_errors: parse_errors.len(),
        total_line_count: file_metrics.iter().map(|v| v.line_count).sum(),
        total_bytes: file_metrics.iter().map(|v| v.bytes).sum(),
        total_items: file_metrics.iter().map(|v| v.items_total).sum(),
        total_functions: file_metrics.iter().map(|v| v.functions).sum(),
        total_modules: file_metrics.iter().map(|v| v.modules).sum(),
        max_largest_fn_span: file_metrics
            .iter()
            .map(|v| v.largest_fn_span)
            .max()
            .unwrap_or(0),
    };

    let mut largest_files = file_metrics.clone();
    largest_files.sort_by(|a, b| {
        b.line_count
            .cmp(&a.line_count)
            .then_with(|| a.source_path.cmp(&b.source_path))
    });

    let mut largest_spans = file_metrics.clone();
    largest_spans.sort_by(|a, b| {
        b.largest_fn_span
            .cmp(&a.largest_fn_span)
            .then_with(|| b.line_count.cmp(&a.line_count))
            .then_with(|| a.source_path.cmp(&b.source_path))
    });

    let mut function_dense = file_metrics.clone();
    function_dense.sort_by(|a, b| {
        b.functions_per_kloc
            .partial_cmp(&a.functions_per_kloc)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.functions.cmp(&a.functions))
            .then_with(|| a.source_path.cmp(&b.source_path))
    });

    let hotspots = RustCodeBatchHotspots {
        largest_files: truncate_ranked(largest_files, args.top),
        largest_function_spans: truncate_ranked(largest_spans, args.top),
        function_dense_files: truncate_ranked(function_dense, args.top),
    };

    Ok(json!({
        "mode": "batch",
        "root_path": root_path.display().to_string(),
        "config": {
            "min_loc": args.min_loc,
            "workers": workers,
            "top": args.top,
            "max_files": args.max_files,
            "include_files": args.include_files,
        },
        "totals": totals,
        "hotspots": hotspots,
        "parse_errors": parse_errors,
        "files": if args.include_files { json!(file_metrics) } else { serde_json::Value::Null },
    }))
}

fn cmd_code_single(args: &CodeArgs, source_path: &Path) -> Result<serde_json::Value> {
    let analysis = analyze_rust_file(source_path)?;
    let generated = if args.generate {
        generate_struct_getters_for_file(source_path)?
    } else {
        None
    };

    Ok(json!({
        "mode": "single",
        "source_path": analysis.source_path,
        "bytes": analysis.bytes,
        "line_count": analysis.line_count,
        "largest_fn_span": analysis.largest_fn_span,
        "largest_fn_name": analysis.largest_fn_name,
        "largest_fn_start": analysis.largest_fn_start,
        "summary": analysis.summary,
        "generated": generated,
    }))
}

async fn cmd_code(args: CodeArgs) -> Result<()> {
    let source_path = resolve_abs_path(&args.path);
    if !source_path.exists() {
        anyhow::bail!("path does not exist: {}", source_path.display());
    }

    let resp = if source_path.is_file() {
        cmd_code_single(&args, &source_path)?
    } else if source_path.is_dir() {
        cmd_code_batch(&args, &source_path).await?
    } else {
        anyhow::bail!(
            "path is neither a file nor directory: {}",
            source_path.display()
        );
    };

    if let Some(out_path) = args.out {
        let out_path = redirect_finance_output(out_path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(&resp).context("serialize code response")?;
        std::fs::write(&out_path, &json).context("write code --out")?;
        println!(
            "{{\"ok\":true,\"path\":{}}}",
            serde_json::to_string(&out_path.display().to_string())
                .unwrap_or_else(|_| "\"\"".to_string()),
        );
        return Ok(());
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&resp).context("serialize code response")?
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentAttemptResult {
    model: String,
    status: String,
    duration_ms: u128,
    exit_code: Option<i32>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentWorkerResult {
    name: String,
    task: String,
    status: String,
    exit_code: Option<i32>,
    requested_model: Option<String>,
    used_model: Option<String>,
    attempted_models: Vec<String>,
    attempt_count: usize,
    attempts: Vec<AgentAttemptResult>,
    report_path: Option<String>,
    started_at: String,
    finished_at: String,
    duration_ms: u128,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Debug, Clone, Serialize)]
struct AgentRunResponse {
    ok: bool,
    usable: bool,
    kind: String,
    saved_result_path: String,
    saved_manifest_path: String,
    artifact_paths: Vec<String>,
    worker: AgentWorkerResult,
}

#[derive(Debug, Clone, Serialize)]
struct AgentFanoutSummary {
    requested: usize,
    completed: usize,
    failed: usize,
    max_parallel: usize,
}

#[derive(Debug, Clone, Serialize)]
struct AgentFanoutResponse {
    ok: bool,
    usable: bool,
    kind: String,
    saved_result_path: String,
    saved_manifest_path: String,
    artifact_paths: Vec<String>,
    task_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    shared_manifest_path: Option<String>,
    summary: AgentFanoutSummary,
    workers: Vec<AgentWorkerResult>,
}

#[derive(Debug, Clone, Serialize)]
struct AgentSwarmSummary {
    requested_chunks: usize,
    generated_chunks: usize,
    map_completed: usize,
    map_failed: usize,
    max_parallel: usize,
}

#[derive(Debug, Clone, Serialize)]
struct AgentSwarmResponse {
    ok: bool,
    usable: bool,
    kind: String,
    saved_result_path: String,
    saved_manifest_path: String,
    artifact_paths: Vec<String>,
    task: String,
    input_path: String,
    chunk_manifest_path: String,
    map_manifest_path: String,
    summary: AgentSwarmSummary,
    map_workers: Vec<AgentWorkerResult>,
    reduce_worker: AgentWorkerResult,
    critic_worker: AgentWorkerResult,
    final_worker: AgentWorkerResult,
}

#[derive(Debug, Clone, Serialize)]
struct SwarmChunkInfo {
    index: usize,
    path: String,
    chars: usize,
}

#[derive(Debug, Clone)]
struct AgentWorkerSpec {
    name: String,
    task: String,
    provider: Option<String>,
    model: Option<String>,
    fallback_models: Vec<String>,
    max_ms: Option<u64>,
    max_attempts: Option<usize>,
}

struct DirectAgentOutcome {
    worker: AgentWorkerResult,
    artifact_paths: Vec<String>,
}
