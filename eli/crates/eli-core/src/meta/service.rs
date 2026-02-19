pub fn build_json_meta(value: &Value, ctx: MetaContext) -> MetaDocument {
    let max_depth = 8;
    let mut paths: BTreeMap<String, PathAccumulator> = BTreeMap::new();
    let mut vitals = VitalsAccumulator::default();
    walk_json(value, "$", 0, max_depth, &mut paths, &mut vitals);
    if let Some(arr) = value.as_array() {
        vitals.row_count = arr.len() as u64;
    } else {
        vitals.row_count = 1;
    }

    let schema_tree = build_schema_node(value, 0, max_depth);
    let mut path_index = build_path_index(&paths);
    annotate_path_index_semantics(&mut path_index, &ctx);
    let source_path = resolve_source_path(ctx.source_path.as_deref());
    let source_size = ctx.source_size_bytes.unwrap_or_else(|| {
        serde_json::to_string(value)
            .map(|s| s.len() as u64)
            .unwrap_or(0)
    });
    let slice_suggestion = build_slice_suggestion(ctx.source_kind.clone(), source_size);
    let vitals_doc = build_vitals(path_index.as_slice(), &vitals);
    let boilerplate = build_boilerplate(ctx.source_kind.clone(), &source_path, &slice_suggestion);

    MetaDocument {
        meta_version: "v1".to_string(),
        generated_at: Utc::now().to_rfc3339(),
        source_path,
        source_kind: ctx.source_kind,
        source_size_bytes: source_size,
        provenance: ctx.provenance,
        schema_tree,
        path_index,
        vitals: vitals_doc,
        slice_suggestion,
        boilerplate,
    }
}

pub fn build_csv_meta(path: &Path, opts: ProbeOptions) -> Result<MetaDocument> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_path(path)
        .map_err(|e| Error::Other(format!("open csv {}: {e}", path.display())))?;
    let headers = rdr
        .headers()
        .map_err(|e| Error::Other(format!("read csv headers {}: {e}", path.display())))?
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    let mut paths: BTreeMap<String, PathAccumulator> = BTreeMap::new();
    let mut total_rows: u64 = 0;
    let mut sampled_rows: usize = 0;
    let mut vitals = VitalsAccumulator::default();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| Error::Other(format!("read csv row {}: {e}", path.display())))?;
        total_rows += 1;
        if sampled_rows < opts.sample_rows {
            sampled_rows += 1;
            for (idx, col) in headers.iter().enumerate() {
                let v = rec.get(idx).unwrap_or("").trim();
                let json_value = csv_cell_to_json(v);
                let p = format!("$[].{col}");
                walk_json(&json_value, &p, 1, opts.max_depth, &mut paths, &mut vitals);
            }
        }
    }
    vitals.row_count = total_rows;

    let mut fields = BTreeMap::new();
    for col in &headers {
        let p = format!("$[].{col}");
        let kind = paths
            .get(&p)
            .and_then(|acc| dominant_kind(&acc.types))
            .unwrap_or_else(|| "string".to_string());
        fields.insert(
            col.clone(),
            SchemaNode {
                kind,
                fields: None,
                items: None,
                variants: None,
            },
        );
    }
    let schema_tree = SchemaNode {
        kind: "array".to_string(),
        fields: None,
        items: Some(Box::new(SchemaNode {
            kind: "object".to_string(),
            fields: Some(fields),
            items: None,
            variants: None,
        })),
        variants: None,
    };

    let path_index = build_path_index(&paths);
    let source_path = resolve_source_path(Some(path));
    let source_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let slice_suggestion = build_slice_suggestion(SourceKind::Csv, source_size);
    let vitals_doc = build_vitals(path_index.as_slice(), &vitals);
    let boilerplate = build_boilerplate(SourceKind::Csv, &source_path, &slice_suggestion);

    Ok(MetaDocument {
        meta_version: "v1".to_string(),
        generated_at: Utc::now().to_rfc3339(),
        source_path,
        source_kind: SourceKind::Csv,
        source_size_bytes: source_size,
        provenance: None,
        schema_tree,
        path_index,
        vitals: vitals_doc,
        slice_suggestion,
        boilerplate,
    })
}

pub fn write_sidecar(meta: &MetaDocument, target_data_path: &Path) -> Result<PathBuf> {
    let sidecar = sidecar_path_for(target_data_path);
    if let Some(parent) = sidecar.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Other(format!("create sidecar dir {}: {e}", parent.display())))?;
    }
    let raw = serde_json::to_string_pretty(meta)
        .map_err(|e| Error::Other(format!("serialize sidecar meta {}: {e}", sidecar.display())))?;
    std::fs::write(&sidecar, raw)
        .map_err(|e| Error::Other(format!("write sidecar meta {}: {e}", sidecar.display())))?;
    Ok(sidecar)
}

pub fn sidecar_path_for(target_data_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta.json", target_data_path.display()))
}

