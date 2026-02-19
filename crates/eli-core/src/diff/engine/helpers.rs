fn short_hash(h: &str) -> String {
    let s = h.trim();
    if s.len() <= 8 {
        return s.to_string();
    }
    s[..8].to_string()
}

fn compute_sha256(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn create_backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::InvalidInput("invalid filename".to_string()))?;
    let ts = Utc::now().format("%Y%m%d-%H%M%S-%f");
    let backup = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".eli-backup-{filename}-{ts}"));
    std::fs::copy(path, &backup)?;
    Ok(Some(backup))
}

fn render_diff(original: &str, updated: &str, path: &Path) -> String {
    let name = path.display().to_string();
    TextDiff::from_lines(original, updated)
        .unified_diff()
        .header(&name, &name)
        .to_string()
}

#[derive(Debug)]
struct PatchApplyError(String);

impl std::fmt::Display for PatchApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PatchApplyError {}

#[derive(Clone, Debug)]
struct PatchHunk {
    src_start: usize,
    lines: Vec<String>,
}

fn apply_patch_to_text(
    original_text: &str,
    patch_text: &str,
) -> std::result::Result<String, PatchApplyError> {
    let hunks = parse_patch_hunks(patch_text)?;
    if hunks.is_empty() {
        return Err(PatchApplyError("no hunks found in patch".to_string()));
    }

    let original_lines = split_lines_keepends(original_text);
    let mut result_lines: Vec<String> = Vec::new();
    let mut cursor: usize = 0;

    for hunk in hunks {
        let src_index = hunk.src_start.saturating_sub(1);
        if src_index > original_lines.len() {
            return Err(PatchApplyError(
                "patch hunk starts beyond end of file".to_string(),
            ));
        }
        if src_index < cursor {
            return Err(PatchApplyError(
                "patch hunks overlap or are out of order".to_string(),
            ));
        }

        result_lines.extend_from_slice(&original_lines[cursor..src_index]);
        cursor = src_index;

        let mut current_index = cursor;
        let mut applied: Vec<String> = Vec::new();

        for line in hunk.lines {
            if line.is_empty() {
                continue;
            }
            let token = line.chars().next().unwrap_or(' ');
            let content = &line[1..];

            match token {
                ' ' => {
                    let actual = expect_line(&original_lines, current_index, content)?;
                    applied.push(actual);
                    current_index += 1;
                }
                '-' => {
                    let _ = expect_line(&original_lines, current_index, content)?;
                    current_index += 1;
                }
                '+' => {
                    applied.push(content.to_string());
                }
                '\\' => {}
                other => {
                    return Err(PatchApplyError(format!(
                        "unsupported patch token '{other}'"
                    )))
                }
            }
        }

        result_lines.extend(applied);
        cursor = current_index;
    }

    result_lines.extend_from_slice(&original_lines[cursor..]);
    Ok(result_lines.concat())
}

fn parse_patch_hunks(patch_text: &str) -> std::result::Result<Vec<PatchHunk>, PatchApplyError> {
    let normalized = patch_text.replace("\r\n", "\n");
    let lines = split_lines_keepends(&normalized);
    let mut hunks = Vec::new();

    let mut idx = 0usize;
    while idx < lines.len() {
        let line = &lines[idx];
        if !line.starts_with("@@") {
            idx += 1;
            continue;
        }

        let src_start = parse_hunk_src_start(line)?;
        idx += 1;

        let mut hunk_lines = Vec::new();
        while idx < lines.len() {
            let next = &lines[idx];
            if next.starts_with("@@") {
                break;
            }
            if next.is_empty() {
                break;
            }
            let token = next.chars().next().unwrap_or(' ');
            if !matches!(token, ' ' | '+' | '-' | '\\') {
                break;
            }
            hunk_lines.push(next.clone());
            idx += 1;
        }

        hunks.push(PatchHunk {
            src_start,
            lines: hunk_lines,
        });
    }

    Ok(hunks)
}

fn parse_hunk_src_start(line: &str) -> std::result::Result<usize, PatchApplyError> {
    let trimmed = line.trim();
    if !trimmed.starts_with("@@") {
        return Err(PatchApplyError("invalid hunk header".to_string()));
    }

    let rest = &trimmed[2..];
    let close = rest
        .find("@@")
        .ok_or_else(|| PatchApplyError(format!("invalid hunk header: {trimmed}")))?;
    let inner = rest[..close].trim();

    let mut parts = inner.split_whitespace();
    let src = parts
        .next()
        .ok_or_else(|| PatchApplyError(format!("invalid hunk header: {trimmed}")))?;
    let dst = parts
        .next()
        .ok_or_else(|| PatchApplyError(format!("invalid hunk header: {trimmed}")))?;

    let src_start = parse_range_start(src, '-')?;
    let _ = parse_range_start(dst, '+')?;

    Ok(src_start)
}

fn parse_range_start(s: &str, prefix: char) -> std::result::Result<usize, PatchApplyError> {
    let s = s
        .strip_prefix(prefix)
        .ok_or_else(|| PatchApplyError(format!("invalid range '{s}'")))?;
    let mut it = s.split(',');
    let start = it
        .next()
        .unwrap_or("0")
        .parse::<usize>()
        .map_err(|_| PatchApplyError(format!("invalid range '{s}'")))?;
    // Parse the optional length just to validate the format.
    if let Some(len) = it.next() {
        let _ = len
            .parse::<usize>()
            .map_err(|_| PatchApplyError(format!("invalid range '{s}'")))?;
    }
    Ok(start)
}

fn expect_line(
    lines: &[String],
    index: usize,
    expected: &str,
) -> std::result::Result<String, PatchApplyError> {
    let Some(actual) = lines.get(index) else {
        return Err(PatchApplyError(
            "patch references line beyond end of file".to_string(),
        ));
    };

    if actual == expected {
        return Ok(actual.clone());
    }

    if trim_line_end(actual) == trim_line_end(expected) {
        return Ok(actual.clone());
    }

    return Err(PatchApplyError(format!(
        "patch mismatch. expected='{}' actual='{}'",
        trim_line_end(expected),
        trim_line_end(actual)
    )));
}

fn trim_line_end(s: &str) -> &str {
    s.trim_end_matches(&['\r', '\n'][..])
}

fn split_lines_keepends(s: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in s.char_indices() {
        if ch == '\n' {
            lines.push(s[start..=idx].to_string());
            start = idx + 1;
        }
    }
    if start < s.len() {
        lines.push(s[start..].to_string());
    }
    lines
}

pub struct UndoManager;

