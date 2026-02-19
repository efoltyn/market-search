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

