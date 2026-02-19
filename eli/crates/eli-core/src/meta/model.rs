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

