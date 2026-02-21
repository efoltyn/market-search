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

fn impl_fn_signature(item: &syn::ImplItemFn) -> String {
    let vis = match &item.vis {
        syn::Visibility::Public(_) => "pub ",
        syn::Visibility::Restricted(_) => "pub(crate) ",
        syn::Visibility::Inherited => "",
    };
    let asyncness = if item.sig.asyncness.is_some() { "async " } else { "" };
    let name = item.sig.ident.to_string();
    let params: String = item
        .sig
        .inputs
        .iter()
        .map(|arg| match arg {
            syn::FnArg::Receiver(r) => {
                if r.reference.is_some() {
                    if r.mutability.is_some() { "&mut self".to_string() } else { "&self".to_string() }
                } else {
                    "self".to_string()
                }
            }
            syn::FnArg::Typed(pt) => {
                let pname = match pt.pat.as_ref() {
                    syn::Pat::Ident(p) => p.ident.to_string(),
                    _ => "_".to_string(),
                };
                format!("{pname}: {}", type_to_string(&pt.ty))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let ret = match &item.sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, ty) => format!(" -> {}", type_to_string(ty)),
    };
    format!("{vis}{asyncness}fn {name}({params}){ret}")
}

fn fn_signature(item: &syn::ItemFn) -> String {
    let vis = match &item.vis {
        syn::Visibility::Public(_) => "pub ",
        syn::Visibility::Restricted(_) => "pub(crate) ",
        syn::Visibility::Inherited => "",
    };
    let asyncness = if item.sig.asyncness.is_some() { "async " } else { "" };
    let name = item.sig.ident.to_string();
    let params: String = item
        .sig
        .inputs
        .iter()
        .map(|arg| match arg {
            syn::FnArg::Receiver(r) => {
                if r.reference.is_some() {
                    if r.mutability.is_some() {
                        "&mut self".to_string()
                    } else {
                        "&self".to_string()
                    }
                } else {
                    "self".to_string()
                }
            }
            syn::FnArg::Typed(pt) => {
                let pname = match pt.pat.as_ref() {
                    syn::Pat::Ident(p) => p.ident.to_string(),
                    _ => "_".to_string(),
                };
                format!("{pname}: {}", type_to_string(&pt.ty))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let ret = match &item.sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, ty) => format!(" -> {}", type_to_string(ty)),
    };
    format!("{vis}{asyncness}fn {name}({params}){ret}")
}

fn struct_field_list(item: &syn::ItemStruct) -> Vec<String> {
    match &item.fields {
        syn::Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let name = f
                    .ident
                    .as_ref()
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                format!("{name}: {}", type_to_string(&f.ty))
            })
            .collect(),
        syn::Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, f)| format!("{i}: {}", type_to_string(&f.ty)))
            .collect(),
        syn::Fields::Unit => Vec::new(),
    }
}

fn summarize_rust_file(file: &syn::File) -> RustFileSummary {
    let mut summary = RustFileSummary {
        items_total: file.items.len(),
        functions: 0,
        function_names: Vec::new(),
        function_signatures: Vec::new(),
        structs: 0,
        struct_names: Vec::new(),
        struct_fields: std::collections::BTreeMap::new(),
        enums: 0,
        enum_names: Vec::new(),
        impls: 0,
        impl_targets: Vec::new(),
        impl_methods: std::collections::BTreeMap::new(),
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
                summary.function_signatures.push(fn_signature(v));
            }
            syn::Item::Struct(v) => {
                summary.structs += 1;
                summary.struct_names.push(v.ident.to_string());
                summary.struct_fields.insert(v.ident.to_string(), struct_field_list(v));
            }
            syn::Item::Enum(v) => {
                summary.enums += 1;
                summary.enum_names.push(v.ident.to_string());
            }
            syn::Item::Impl(v) => {
                summary.impls += 1;
                let target = format_impl_target(v);
                summary.impl_targets.push(target.clone());
                let methods: Vec<String> = v
                    .items
                    .iter()
                    .filter_map(|item| {
                        if let syn::ImplItem::Fn(m) = item {
                            Some(impl_fn_signature(m))
                        } else {
                            None
                        }
                    })
                    .collect();
                if !methods.is_empty() {
                    summary.impl_methods.insert(target, methods);
                }
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
        .map(|seg| {
            let name = seg.ident.to_string();
            match &seg.arguments {
                syn::PathArguments::None => name,
                syn::PathArguments::AngleBracketed(ab) => {
                    let args: Vec<String> = ab
                        .args
                        .iter()
                        .map(|a| match a {
                            syn::GenericArgument::Type(t) => type_to_string(t),
                            syn::GenericArgument::Lifetime(lt) => {
                                format!("'{}", lt.ident)
                            }
                            _ => "_".to_string(),
                        })
                        .collect();
                    format!("{name}<{}>", args.join(", "))
                }
                syn::PathArguments::Parenthesized(pb) => {
                    let inputs: Vec<String> = pb.inputs.iter().map(type_to_string).collect();
                    let ret = match &pb.output {
                        syn::ReturnType::Default => String::new(),
                        syn::ReturnType::Type(_, t) => format!(" -> {}", type_to_string(t)),
                    };
                    format!("{name}({}){ret}", inputs.join(", "))
                }
            }
        })
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

// ── find mode ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct FindMatch {
    file: String,
    line: usize,
    context: String,
}

fn find_symbols_in_path(root: &Path, symbols: &[String]) -> Result<serde_json::Value> {
    use aho_corasick::AhoCorasick;

    let files = if root.is_file() {
        vec![root.to_path_buf()]
    } else {
        collect_rust_files(root)?
    };

    let ac = AhoCorasick::new(symbols).context("build aho-corasick")?;
    let mut results: std::collections::BTreeMap<String, Vec<FindMatch>> =
        symbols.iter().map(|s| (s.clone(), Vec::new())).collect();

    for file_path in &files {
        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let path_str = file_path.display().to_string();
        for (line_idx, line) in content.lines().enumerate() {
            for mat in ac.find_iter(line) {
                // word-boundary check so `fetch_snapshot` doesn't match inside `fetch_snapshot_list`
                let start = mat.start();
                let end = mat.end();
                let bytes = line.as_bytes();
                let before_ok = start == 0
                    || (!bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_');
                let after_ok = end >= bytes.len()
                    || (!bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_');
                if !before_ok || !after_ok {
                    continue;
                }
                let sym = &symbols[mat.pattern().as_usize()];
                results.entry(sym.clone()).or_default().push(FindMatch {
                    file: path_str.clone(),
                    line: line_idx + 1,
                    context: line.trim().to_string(),
                });
            }
        }
    }

    let total_matches: usize = results.values().map(|v| v.len()).sum();
    Ok(json!({
        "mode": "find",
        "root_path": root.display().to_string(),
        "symbols": symbols,
        "total_matches": total_matches,
        "results": results,
    }))
}

// ── pub-api mode ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct PubApiFile {
    file: String,
    /// pub fn / pub async fn with full parameter + return type signatures.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    functions: Vec<String>,
    /// pub struct → field list "name: Type".
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    structs: std::collections::BTreeMap<String, Vec<String>>,
    /// pub enum → variant list "Variant" | "Variant(T)" | "Variant { field: T }".
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    enums: std::collections::BTreeMap<String, Vec<String>>,
    /// impl Target → pub method signatures.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    impl_methods: std::collections::BTreeMap<String, Vec<String>>,
    /// pub trait → method signatures.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    traits: std::collections::BTreeMap<String, Vec<String>>,
    /// pub type aliases.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    type_aliases: Vec<String>,
}

fn is_pub(vis: &syn::Visibility) -> bool {
    matches!(
        vis,
        syn::Visibility::Public(_) | syn::Visibility::Restricted(_)
    )
}

fn trait_fn_signature(item: &syn::TraitItemFn) -> String {
    let asyncness = if item.sig.asyncness.is_some() {
        "async "
    } else {
        ""
    };
    let name = item.sig.ident.to_string();
    let params: String = item
        .sig
        .inputs
        .iter()
        .map(|arg| match arg {
            syn::FnArg::Receiver(r) => {
                if r.reference.is_some() {
                    if r.mutability.is_some() {
                        "&mut self".to_string()
                    } else {
                        "&self".to_string()
                    }
                } else {
                    "self".to_string()
                }
            }
            syn::FnArg::Typed(pt) => {
                let pname = match pt.pat.as_ref() {
                    syn::Pat::Ident(p) => p.ident.to_string(),
                    _ => "_".to_string(),
                };
                format!("{pname}: {}", type_to_string(&pt.ty))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let ret = match &item.sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, ty) => format!(" -> {}", type_to_string(ty)),
    };
    format!("{asyncness}fn {name}({params}){ret}")
}

fn enum_variant_str(var: &syn::Variant) -> String {
    let name = var.ident.to_string();
    match &var.fields {
        syn::Fields::Unit => name,
        syn::Fields::Unnamed(u) => {
            let types: Vec<String> = u.unnamed.iter().map(|f| type_to_string(&f.ty)).collect();
            format!("{}({})", name, types.join(", "))
        }
        syn::Fields::Named(n) => {
            let fields: Vec<String> = n
                .named
                .iter()
                .map(|f| {
                    let fname = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
                    format!("{fname}: {}", type_to_string(&f.ty))
                })
                .collect();
            format!("{name} {{ {} }}", fields.join(", "))
        }
    }
}

fn extract_pub_api(source_path: &Path, root: &Path) -> Result<Option<PubApiFile>> {
    let source = std::fs::read_to_string(source_path)
        .with_context(|| format!("read {}", source_path.display()))?;
    let parsed = syn::parse_file(&source)
        .with_context(|| format!("parse {}", source_path.display()))?;

    let rel = source_path
        .strip_prefix(root)
        .unwrap_or(source_path)
        .display()
        .to_string();

    let mut api = PubApiFile {
        file: rel,
        functions: Vec::new(),
        structs: std::collections::BTreeMap::new(),
        enums: std::collections::BTreeMap::new(),
        impl_methods: std::collections::BTreeMap::new(),
        traits: std::collections::BTreeMap::new(),
        type_aliases: Vec::new(),
    };

    for item in &parsed.items {
        match item {
            syn::Item::Fn(v) if is_pub(&v.vis) => {
                api.functions.push(fn_signature(v));
            }
            syn::Item::Struct(v) if is_pub(&v.vis) => {
                api.structs.insert(v.ident.to_string(), struct_field_list(v));
            }
            syn::Item::Enum(v) if is_pub(&v.vis) => {
                let variants: Vec<String> = v.variants.iter().map(enum_variant_str).collect();
                api.enums.insert(v.ident.to_string(), variants);
            }
            syn::Item::Impl(v) => {
                let methods: Vec<String> = v
                    .items
                    .iter()
                    .filter_map(|i| {
                        if let syn::ImplItem::Fn(m) = i {
                            if is_pub(&m.vis) {
                                return Some(impl_fn_signature(m));
                            }
                        }
                        None
                    })
                    .collect();
                if !methods.is_empty() {
                    let target = format_impl_target(v);
                    api.impl_methods.entry(target).or_default().extend(methods);
                }
            }
            syn::Item::Trait(v) if is_pub(&v.vis) => {
                let methods: Vec<String> = v
                    .items
                    .iter()
                    .filter_map(|i| {
                        if let syn::TraitItem::Fn(m) = i {
                            Some(trait_fn_signature(m))
                        } else {
                            None
                        }
                    })
                    .collect();
                api.traits.insert(v.ident.to_string(), methods);
            }
            syn::Item::Type(v) if is_pub(&v.vis) => {
                api.type_aliases
                    .push(format!("pub type {} = {}", v.ident, type_to_string(&v.ty)));
            }
            _ => {}
        }
    }

    let empty = api.functions.is_empty()
        && api.structs.is_empty()
        && api.enums.is_empty()
        && api.impl_methods.is_empty()
        && api.traits.is_empty()
        && api.type_aliases.is_empty();

    if empty { Ok(None) } else { Ok(Some(api)) }
}

fn pub_api_for_path(root: &Path) -> Result<serde_json::Value> {
    let files = if root.is_file() {
        vec![root.to_path_buf()]
    } else {
        collect_rust_files(root)?
    };

    let mut api_files: Vec<PubApiFile> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for file_path in &files {
        match extract_pub_api(file_path, root) {
            Ok(Some(api)) => api_files.push(api),
            Ok(None) => {}
            Err(e) => errors.push(format!("{}: {e:#}", file_path.display())),
        }
    }

    let total_fns: usize = api_files.iter().map(|f| f.functions.len()).sum();
    let total_types: usize = api_files
        .iter()
        .map(|f| f.structs.len() + f.enums.len() + f.traits.len() + f.type_aliases.len())
        .sum();

    Ok(json!({
        "mode": "pub_api",
        "root_path": root.display().to_string(),
        "files_with_pub_items": api_files.len(),
        "total_pub_functions": total_fns,
        "total_pub_types": total_types,
        "files": api_files,
        "errors": errors,
    }))
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

    // --find: multi-symbol search using aho-corasick (short-circuits other modes)
    if !args.find.is_empty() {
        let resp = find_symbols_in_path(&source_path, &args.find)?;
        if let Some(out_path) = args.out {
            let out_path = redirect_finance_output(out_path);
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let json = serde_json::to_string_pretty(&resp).context("serialize find response")?;
            std::fs::write(&out_path, &json).context("write --out")?;
            println!(
                "{{\"ok\":true,\"path\":{}}}",
                serde_json::to_string(&out_path.display().to_string()).unwrap_or_default()
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).context("serialize find response")?
            );
        }
        return Ok(());
    }

    // --pub-api: public API surface map (short-circuits other modes)
    if args.pub_api {
        let resp = pub_api_for_path(&source_path)?;
        if let Some(out_path) = args.out {
            let out_path = redirect_finance_output(out_path);
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let json =
                serde_json::to_string_pretty(&resp).context("serialize pub-api response")?;
            std::fs::write(&out_path, &json).context("write --out")?;
            println!(
                "{{\"ok\":true,\"path\":{}}}",
                serde_json::to_string(&out_path.display().to_string()).unwrap_or_default()
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).context("serialize pub-api response")?
            );
        }
        return Ok(());
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
