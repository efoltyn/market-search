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
