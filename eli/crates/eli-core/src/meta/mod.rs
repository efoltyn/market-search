use crate::{Error, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Json,
    Csv,
    Ndjson,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct MetaProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_query: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,
    pub source_kind: SourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MetaProvenance>,
}

impl Default for MetaContext {
    fn default() -> Self {
        Self {
            source_path: None,
            source_kind: SourceKind::Unknown,
            source_size_bytes: None,
            provenance: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeOptions {
    pub sample_rows: usize,
    pub sample_bytes: usize,
    pub max_depth: usize,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            sample_rows: 1000,
            sample_bytes: 2_000_000,
            max_depth: 8,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaDocument {
    pub meta_version: String,
    pub generated_at: String,
    pub source_path: String,
    pub source_kind: SourceKind,
    pub source_size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MetaProvenance>,
    pub schema_tree: SchemaNode,
    pub path_index: Vec<PathIndexEntry>,
    pub vitals: MetaVitals,
    pub slice_suggestion: SliceSuggestion,
    pub boilerplate: Boilerplate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaNode {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<String, SchemaNode>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<SchemaNode>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variants: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathIndexEntry {
    pub path: String,
    pub types: Vec<String>,
    pub present_count: u64,
    pub null_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub example_values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numeric: Option<NumericStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability_scale: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NumericStats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
    pub count: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaVitals {
    pub row_count: u64,
    pub object_count: u64,
    pub array_count: u64,
    pub scalar_count: u64,
    pub path_count: usize,
    pub nullable_paths: usize,
    pub numeric_paths: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub numeric_path_stats: BTreeMap<String, NumericStats>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SliceSuggestion {
    pub strategy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_rows: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rows: Option<usize>,
    pub rationale: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Boilerplate {
    pub python: String,
    pub jq: String,
    pub rust: String,
}

#[derive(Default, Clone)]
struct NumericAccumulator {
    count: u64,
    sum: f64,
    sum_sq: f64,
    min: Option<f64>,
    max: Option<f64>,
}

#[derive(Default, Clone)]
struct PathAccumulator {
    types: BTreeSet<String>,
    present_count: u64,
    null_count: u64,
    example_values: Vec<String>,
    numeric: NumericAccumulator,
}

#[derive(Default, Clone)]
struct VitalsAccumulator {
    row_count: u64,
    object_count: u64,
    array_count: u64,
    scalar_count: u64,
}

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

fn walk_json(
    value: &Value,
    path: &str,
    depth: usize,
    max_depth: usize,
    out: &mut BTreeMap<String, PathAccumulator>,
    vitals: &mut VitalsAccumulator,
) {
    let acc = out.entry(path.to_string()).or_default();
    let t = json_type(value);
    acc.types.insert(t.clone());
    acc.present_count += 1;
    if value.is_null() {
        acc.null_count += 1;
    }
    if let Some(num) = value.as_f64() {
        acc.numeric.count += 1;
        acc.numeric.sum += num;
        acc.numeric.sum_sq += num * num;
        acc.numeric.min = Some(acc.numeric.min.map(|m| m.min(num)).unwrap_or(num));
        acc.numeric.max = Some(acc.numeric.max.map(|m| m.max(num)).unwrap_or(num));
    }
    if acc.example_values.len() < 3 {
        if let Some(ex) = scalar_example(value) {
            if !acc.example_values.contains(&ex) {
                acc.example_values.push(ex);
            }
        }
    }

    match value {
        Value::Object(map) => {
            vitals.object_count += 1;
            if depth >= max_depth {
                return;
            }
            for (k, v) in map {
                let child = format!("{path}.{k}");
                walk_json(v, &child, depth + 1, max_depth, out, vitals);
            }
        }
        Value::Array(arr) => {
            vitals.array_count += 1;
            if depth >= max_depth {
                return;
            }
            let child = format!("{path}[]");
            for item in arr {
                walk_json(item, &child, depth + 1, max_depth, out, vitals);
            }
        }
        _ => vitals.scalar_count += 1,
    }
}

fn build_schema_node(value: &Value, depth: usize, max_depth: usize) -> SchemaNode {
    if depth >= max_depth {
        return SchemaNode {
            kind: json_type(value),
            fields: None,
            items: None,
            variants: None,
        };
    }
    match value {
        Value::Object(map) => {
            let mut fields = BTreeMap::new();
            for (k, v) in map {
                fields.insert(k.clone(), build_schema_node(v, depth + 1, max_depth));
            }
            SchemaNode {
                kind: "object".to_string(),
                fields: Some(fields),
                items: None,
                variants: None,
            }
        }
        Value::Array(arr) => {
            let item_node = if arr.is_empty() {
                SchemaNode {
                    kind: "unknown".to_string(),
                    fields: None,
                    items: None,
                    variants: None,
                }
            } else {
                let mut node = build_schema_node(&arr[0], depth + 1, max_depth);
                for item in arr.iter().skip(1) {
                    node = merge_schema(node, build_schema_node(item, depth + 1, max_depth));
                }
                node
            };
            SchemaNode {
                kind: "array".to_string(),
                fields: None,
                items: Some(Box::new(item_node)),
                variants: None,
            }
        }
        _ => SchemaNode {
            kind: json_type(value),
            fields: None,
            items: None,
            variants: None,
        },
    }
}

fn merge_schema(a: SchemaNode, b: SchemaNode) -> SchemaNode {
    if a.kind == b.kind {
        if a.kind == "object" {
            let mut fields = a.fields.unwrap_or_default();
            for (k, v) in b.fields.unwrap_or_default() {
                match fields.remove(&k) {
                    Some(prev) => {
                        fields.insert(k, merge_schema(prev, v));
                    }
                    None => {
                        fields.insert(k, v);
                    }
                }
            }
            return SchemaNode {
                kind: "object".to_string(),
                fields: Some(fields),
                items: None,
                variants: None,
            };
        }
        if a.kind == "array" {
            let merged_items = match (a.items, b.items) {
                (Some(x), Some(y)) => Some(Box::new(merge_schema(*x, *y))),
                (Some(x), None) => Some(x),
                (None, Some(y)) => Some(y),
                (None, None) => None,
            };
            return SchemaNode {
                kind: "array".to_string(),
                fields: None,
                items: merged_items,
                variants: None,
            };
        }
        return a;
    }

    let mut variants = BTreeSet::new();
    if a.kind == "union" {
        if let Some(vs) = a.variants {
            for v in vs {
                variants.insert(v);
            }
        }
    } else {
        variants.insert(a.kind);
    }
    if b.kind == "union" {
        if let Some(vs) = b.variants {
            for v in vs {
                variants.insert(v);
            }
        }
    } else {
        variants.insert(b.kind);
    }
    SchemaNode {
        kind: "union".to_string(),
        fields: None,
        items: None,
        variants: Some(variants.into_iter().collect()),
    }
}

fn build_path_index(paths: &BTreeMap<String, PathAccumulator>) -> Vec<PathIndexEntry> {
    paths
        .iter()
        .map(|(path, acc)| PathIndexEntry {
            path: path.clone(),
            types: acc.types.iter().cloned().collect(),
            present_count: acc.present_count,
            null_count: acc.null_count,
            example_values: acc.example_values.clone(),
            numeric: numeric_stats(&acc.numeric),
            units: None,
            probability_scale: None,
        })
        .collect()
}

fn annotate_path_index_semantics(path_index: &mut [PathIndexEntry], ctx: &MetaContext) {
    let tool = ctx
        .provenance
        .as_ref()
        .and_then(|p| p.tool.as_deref())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_odds_tool = tool.contains("finance.odds") || tool.contains("finance.sync");
    if !is_odds_tool {
        return;
    }

    for entry in path_index.iter_mut() {
        let path = entry.path.to_ascii_lowercase();
        if path.contains("field_semantics") {
            continue;
        }
        let terminal = path
            .rsplit('.')
            .next()
            .unwrap_or(path.as_str())
            .trim_end_matches("[]");

        if terminal.contains("probability") && terminal != "probability_scale" {
            entry.probability_scale = Some("0_to_1".to_string());
        }
        if (terminal.contains("yes_price")
            || terminal == "volume"
            || terminal == "total_volume"
            || terminal.ends_with("volume_sum")
            || terminal.contains("_volume_"))
            && !path.ends_with("yes_price_units")
            && !path.ends_with("volume_units")
        {
            entry.units = Some("cents".to_string());
        }
    }
}

fn build_vitals(path_index: &[PathIndexEntry], vitals: &VitalsAccumulator) -> MetaVitals {
    let nullable_paths = path_index.iter().filter(|p| p.null_count > 0).count();
    let mut numeric_paths = 0usize;
    let mut numeric_stats_map = BTreeMap::new();
    for p in path_index {
        if let Some(num) = &p.numeric {
            numeric_paths += 1;
            numeric_stats_map.insert(p.path.clone(), num.clone());
        }
    }
    MetaVitals {
        row_count: vitals.row_count,
        object_count: vitals.object_count,
        array_count: vitals.array_count,
        scalar_count: vitals.scalar_count,
        path_count: path_index.len(),
        nullable_paths,
        numeric_paths,
        numeric_path_stats: numeric_stats_map,
    }
}

fn build_slice_suggestion(kind: SourceKind, bytes: u64) -> SliceSuggestion {
    if bytes > 50_000_000 {
        return SliceSuggestion {
            strategy: "chunked".to_string(),
            chunk_rows: Some(5000),
            sample_rows: Some(200),
            rationale: format!(
                "Large file ({} bytes). Validate logic on samples, then process in chunks.",
                bytes
            ),
        };
    }
    if bytes > 5_000_000 {
        return SliceSuggestion {
            strategy: "sample_then_full".to_string(),
            chunk_rows: Some(2000),
            sample_rows: Some(100),
            rationale: format!(
                "Medium file ({} bytes). Probe first rows before full pass.",
                bytes
            ),
        };
    }
    let strategy = match kind {
        SourceKind::Csv => "full_read_small",
        SourceKind::Ndjson => "line_stream",
        _ => "full_read",
    };
    SliceSuggestion {
        strategy: strategy.to_string(),
        chunk_rows: None,
        sample_rows: Some(50),
        rationale: format!("Small file ({} bytes). Full parse is safe.", bytes),
    }
}

fn build_boilerplate(kind: SourceKind, source_path: &str, slice: &SliceSuggestion) -> Boilerplate {
    let chunk = slice.chunk_rows.unwrap_or(2000);
    match kind {
        SourceKind::Csv => Boilerplate {
            python: format!(
                "import pandas as pd\npath = r\"{source_path}\"\nfor chunk in pd.read_csv(path, chunksize={chunk}):\n    print(chunk.head(2))\n"
            ),
            jq: format!("jq -R -s 'split(\"\\n\")[:6]' \"{source_path}\""),
            rust: format!(
                "let mut rdr = csv::Reader::from_path(\"{source_path}\")?;\nfor rec in rdr.records().take(10) {{ let _ = rec?; }}\n"
            ),
        },
        SourceKind::Ndjson => Boilerplate {
            python: format!(
                "import json\npath = r\"{source_path}\"\nwith open(path) as f:\n    rows = [json.loads(line) for _, line in zip(range(50), f)]\nprint(rows[:2])\n"
            ),
            jq: format!("head -n 50 \"{source_path}\" | jq -s '.'"),
            rust: format!(
                "for line in std::fs::read_to_string(\"{source_path}\")?.lines().take(50) {{ let v: serde_json::Value = serde_json::from_str(line)?; let _ = v; }}\n"
            ),
        },
        _ => Boilerplate {
            python: format!(
                "import json\npath = r\"{source_path}\"\nwith open(path) as f:\n    data = json.load(f)\nprint(type(data), list(data)[:5] if isinstance(data, dict) else len(data))\n"
            ),
            jq: format!("jq 'paths | map(tostring) | join(\".\")' \"{source_path}\" | head -n 40"),
            rust: format!(
                "let raw = std::fs::read_to_string(\"{source_path}\")?;\nlet v: serde_json::Value = serde_json::from_str(&raw)?;\nprintln!(\"{{:?}}\", v);\n"
            ),
        },
    }
}

fn resolve_source_path(path: Option<&Path>) -> String {
    match path {
        Some(p) => {
            if p.is_absolute() {
                p.display().to_string()
            } else {
                std::env::current_dir()
                    .ok()
                    .map(|cwd| cwd.join(p).display().to_string())
                    .unwrap_or_else(|| p.display().to_string())
            }
        }
        None => String::new(),
    }
}

fn json_type(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "boolean".to_string(),
        Value::Number(n) => {
            if n.as_i64().is_some() || n.as_u64().is_some() {
                "integer".to_string()
            } else {
                "number".to_string()
            }
        }
        Value::String(_) => "string".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Object(_) => "object".to_string(),
    }
}

fn dominant_kind(types: &BTreeSet<String>) -> Option<String> {
    if types.is_empty() {
        return None;
    }
    if types.len() == 1 {
        return types.iter().next().cloned();
    }
    Some("union".to_string())
}

fn numeric_stats(acc: &NumericAccumulator) -> Option<NumericStats> {
    if acc.count == 0 {
        return None;
    }
    let mean = acc.sum / acc.count as f64;
    let variance = (acc.sum_sq / acc.count as f64) - (mean * mean);
    let stddev = variance.max(0.0).sqrt();
    Some(NumericStats {
        min: acc.min.unwrap_or(mean),
        max: acc.max.unwrap_or(mean),
        mean,
        stddev,
        count: acc.count,
    })
}

fn scalar_example(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".to_string()),
        Value::Bool(v) => Some(v.to_string()),
        Value::Number(v) => Some(v.to_string()),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Some("\"\"".to_string())
            } else {
                Some(
                    trimmed
                        .chars()
                        .take(80)
                        .collect::<String>()
                        .replace('\n', " "),
                )
            }
        }
        _ => None,
    }
}

fn csv_cell_to_json(cell: &str) -> Value {
    if cell.is_empty() {
        return Value::Null;
    }
    if let Ok(v) = cell.parse::<i64>() {
        return Value::Number(v.into());
    }
    if let Ok(v) = cell.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(v) {
            return Value::Number(n);
        }
    }
    let low = cell.to_ascii_lowercase();
    if low == "true" {
        return Value::Bool(true);
    }
    if low == "false" {
        return Value::Bool(false);
    }
    Value::String(cell.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn json_meta_collects_types_and_numeric_stats() {
        let value = serde_json::json!({
            "a": 1,
            "b": null,
            "c": [1,2,3],
            "d": [{"x":"s"},{"x":2}]
        });
        let meta = build_json_meta(
            &value,
            MetaContext {
                source_kind: SourceKind::Json,
                ..Default::default()
            },
        );
        assert_eq!(meta.meta_version, "v1");
        assert_eq!(meta.source_kind, SourceKind::Json);
        let has_a = meta.path_index.iter().any(|p| p.path == "$.a");
        let has_c_arr = meta.path_index.iter().any(|p| p.path == "$.c[]");
        assert!(has_a);
        assert!(has_c_arr);
        let a_entry = meta
            .path_index
            .iter()
            .find(|p| p.path == "$.a")
            .expect("a entry");
        assert!(a_entry.numeric.is_some());

        let c_entry = meta
            .path_index
            .iter()
            .find(|p| p.path == "$.c[]")
            .expect("c entry");
        let c_num = c_entry.numeric.as_ref().expect("c numeric stats");
        assert_eq!(c_num.count, 3);
        assert!((c_num.mean - 2.0).abs() < 1e-9);
    }

    #[test]
    fn json_meta_tracks_union_types_and_nested_paths() {
        let value = serde_json::json!({
            "mixed": [1, "x", null],
            "nested": [{"k": true}, {"k": false}]
        });
        let meta = build_json_meta(
            &value,
            MetaContext {
                source_kind: SourceKind::Json,
                ..Default::default()
            },
        );
        let mixed = meta
            .path_index
            .iter()
            .find(|p| p.path == "$.mixed[]")
            .expect("mixed path");
        assert!(mixed.types.contains(&"integer".to_string()));
        assert!(mixed.types.contains(&"string".to_string()));
        assert!(mixed.types.contains(&"null".to_string()));
        assert!(
            meta.path_index.iter().any(|p| p.path == "$.nested[].k"),
            "nested key path should be indexed"
        );
    }

    #[test]
    fn csv_meta_profiles_headers_and_rows() {
        let dir = std::env::temp_dir().join(format!("eli_meta_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("sample.csv");
        std::fs::write(
            &path,
            "name,price,active\nAAPL,195.2,true\nMSFT,410.1,false\n",
        )
        .expect("write csv");
        let meta = build_csv_meta(
            &path,
            ProbeOptions {
                sample_rows: 10,
                sample_bytes: 4096,
                max_depth: 8,
            },
        )
        .expect("build csv meta");
        assert_eq!(meta.source_kind, SourceKind::Csv);
        assert_eq!(meta.vitals.row_count, 2);
        assert!(meta.path_index.iter().any(|p| p.path == "$[].name"));
        assert!(meta.path_index.iter().any(|p| p.path == "$[].price"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecar_path_suffix_is_meta_json() {
        let p = PathBuf::from("/tmp/test.json");
        let sidecar = sidecar_path_for(&p);
        assert_eq!(sidecar, PathBuf::from("/tmp/test.json.meta.json"));
    }

    #[test]
    fn json_meta_adds_odds_probability_and_units_hints() {
        let value = serde_json::json!({
            "markets": [
                {"probability_yes": 0.23, "yes_price": 23, "volume": 223483}
            ]
        });
        let meta = build_json_meta(
            &value,
            MetaContext {
                source_kind: SourceKind::Json,
                provenance: Some(MetaProvenance {
                    tool: Some("finance.odds".to_string()),
                    command: Some("finance.odds".to_string()),
                    args: Vec::new(),
                    origin_query: None,
                }),
                ..Default::default()
            },
        );
        let prob = meta
            .path_index
            .iter()
            .find(|p| p.path == "$.markets[].probability_yes")
            .expect("probability path");
        assert_eq!(prob.probability_scale.as_deref(), Some("0_to_1"));

        let price = meta
            .path_index
            .iter()
            .find(|p| p.path == "$.markets[].yes_price")
            .expect("yes_price path");
        assert_eq!(price.units.as_deref(), Some("cents"));

        let vol = meta
            .path_index
            .iter()
            .find(|p| p.path == "$.markets[].volume")
            .expect("volume path");
        assert_eq!(vol.units.as_deref(), Some("cents"));
    }

    #[test]
    fn json_meta_does_not_leak_volume_units_to_probability_fields() {
        let value = serde_json::json!({
            "analysis": {
                "top_markets_by_volume": [
                    {"probability_yes": 0.51, "volume": 12345}
                ]
            }
        });
        let meta = build_json_meta(
            &value,
            MetaContext {
                source_kind: SourceKind::Json,
                provenance: Some(MetaProvenance {
                    tool: Some("finance.sync".to_string()),
                    command: Some("finance.sync".to_string()),
                    args: Vec::new(),
                    origin_query: None,
                }),
                ..Default::default()
            },
        );
        let prob = meta
            .path_index
            .iter()
            .find(|p| p.path == "$.analysis.top_markets_by_volume[].probability_yes")
            .expect("probability path");
        assert_eq!(prob.probability_scale.as_deref(), Some("0_to_1"));
        assert_eq!(prob.units, None);
        let vol = meta
            .path_index
            .iter()
            .find(|p| p.path == "$.analysis.top_markets_by_volume[].volume")
            .expect("volume path");
        assert_eq!(vol.units.as_deref(), Some("cents"));
    }
}
