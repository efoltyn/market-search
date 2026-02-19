pub fn extract_first_json_value(text: &str) -> Option<serde_json::Value> {
    if let Some((value, _, _)) = extract_first_json_segment(text) {
        return Some(value);
    }
    if let Some(fenced) = extract_fenced_code_block(text, "json") {
        if let Ok(value) = parse_json_lenient(fenced) {
            return Some(value);
        }
    }
    None
}

pub fn extract_first_json<T: DeserializeOwned>(text: &str) -> Result<Option<T>> {
    let Some(value) = extract_first_json_value(text) else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value(value)?))
}

fn extract_fenced_code_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
    let fence = "```";
    let mut i = 0;
    while let Some(start) = text[i..].find(fence) {
        let start = i + start;
        let after = start + fence.len();
        let line_end = text[after..].find('\n').map(|n| after + n)?;
        let tag = text[after..line_end].trim();
        if !tag.eq_ignore_ascii_case(lang) {
            i = line_end + 1;
            continue;
        }
        let block_start = line_end + 1;
        let end = text[block_start..].find(fence).map(|n| block_start + n)?;
        return Some(text[block_start..end].trim());
    }
    None
}

fn parse_json_lenient(s: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_str::<serde_json::Value>(s)?)
}

fn extract_first_json_segment(text: &str) -> Option<(serde_json::Value, usize, usize)> {
    for (idx, ch) in text.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }
        let Some(end) = balanced_json_end(text, idx) else {
            continue;
        };
        let slice = &text[idx..end];
        let Ok(value) = parse_json_lenient(slice) else {
            continue;
        };
        return Some((value, idx, end));
    }
    None
}

fn balanced_json_end(text: &str, start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (off, ch) in text[start..].char_indices() {
        let idx = start + off;
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Serialize, SerdeDeserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepStatus {
    KeepWorking,
    Done,
}

#[derive(Clone, Copy, Debug, Serialize, SerdeDeserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Create,
    Replace,
    Patch,
    Delete,
}

#[derive(Clone, Debug, Serialize, SerdeDeserialize)]
pub struct FileDiff {
    pub path: String,
    pub op: DiffOp,

    #[serde(default)]
    pub before_sha256: String,

    #[serde(default)]
    pub after_text: String,

    #[serde(default)]
    pub patch: String,
}

#[derive(Clone, Debug, Serialize, SerdeDeserialize)]
pub struct SubagentTask {
    pub name: String,
    pub task: String,

    #[serde(default)]
    pub model: Option<String>,

    #[serde(default)]
    pub temperature: Option<f32>,

    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Clone, Debug, Serialize, SerdeDeserialize)]
pub struct Synthesis {
    #[serde(default)]
    pub summary: Vec<String>,

    #[serde(default)]
    pub answer: String,

    #[serde(default)]
    pub next_steps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, SerdeDeserialize)]
pub struct ModelResponse {
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub checklist: Vec<String>,
    #[serde(default)]
    pub focus: String,
    pub status: StepStatus,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub commands_parallel: bool,
    #[serde(default)]
    pub screen: Vec<serde_json::Value>,
    #[serde(default)]
    pub diffs: Vec<FileDiff>,
    #[serde(default)]
    pub notes: String,

    #[serde(default)]
    pub synthesis: Option<Synthesis>,

    #[serde(default)]
    pub ask_user: Option<String>,

    #[serde(default)]
    pub subagents: Vec<SubagentTask>,
}

