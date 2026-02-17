use crate::contract::{DiffOp, FileDiff};
use crate::diff::safe_join;
use crate::{Error, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffResult {
    pub path: String,
    pub op: String,
    pub success: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub backup_path: Option<String>,
    #[serde(default)]
    pub preview: bool,
    #[serde(default)]
    pub diff: Option<String>,
}

pub struct DiffEngine {
    project_root: PathBuf,
}

impl DiffEngine {
    pub fn new(project_root: PathBuf) -> Result<Self> {
        Ok(Self { project_root })
    }

    /// Redirect artifact files to eli_research subdirectories.
    /// - .py files → eli_research/scripts/
    /// - .json files → eli_research/data/
    /// Only redirects files that are in the project root (not already in a subdirectory).
    fn redirect_artifact_path(&self, path: &Path) -> PathBuf {
        // Only redirect files directly in project root (no directory component)
        let has_dir = path.parent().map(|p| p != Path::new("")).unwrap_or(false);
        if has_dir {
            return path.to_path_buf();
        }

        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return path.to_path_buf();
        };

        let filename = path.file_name().unwrap_or_default();
        match ext {
            "py" => Path::new("eli_research/scripts").join(filename),
            "json" => Path::new("eli_research/data").join(filename),
            _ => path.to_path_buf(),
        }
    }

    pub fn apply_diff(&self, diff: &FileDiff, dry_run: bool) -> DiffResult {
        let original_path = Path::new(diff.path.trim());
        let path = self.redirect_artifact_path(original_path);
        let resolved = match safe_join(&self.project_root, &path) {
            Ok(p) => p,
            Err(e) => {
                return DiffResult {
                    path: diff.path.clone(),
                    op: format!("{:?}", diff.op).to_lowercase(),
                    success: false,
                    message: e.to_string(),
                    backup_path: None,
                    preview: dry_run,
                    diff: None,
                }
            }
        };

        match diff.op {
            DiffOp::Create => self.create_file(&resolved, &diff.after_text, dry_run),
            DiffOp::Replace => {
                self.replace_file(&resolved, &diff.before_sha256, &diff.after_text, dry_run)
            }
            DiffOp::Patch => self.patch_file(&resolved, &diff.before_sha256, &diff.patch, dry_run),
            DiffOp::Delete => self.delete_file(&resolved, &diff.before_sha256, dry_run),
        }
    }

    pub fn apply_diffs(&self, diffs: &[FileDiff], dry_run: bool) -> Vec<DiffResult> {
        diffs.iter().map(|d| self.apply_diff(d, dry_run)).collect()
    }

    fn create_file(&self, path: &Path, content: &str, dry_run: bool) -> DiffResult {
        if path.exists() {
            return DiffResult {
                path: path.display().to_string(),
                op: "create".to_string(),
                success: false,
                message: "file already exists".to_string(),
                backup_path: None,
                preview: dry_run,
                diff: None,
            };
        }

        if dry_run {
            return DiffResult {
                path: path.display().to_string(),
                op: "create".to_string(),
                success: true,
                message: "preview create".to_string(),
                backup_path: None,
                preview: true,
                diff: Some(render_diff("", content, path)),
            };
        }

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return DiffResult::err(path, "create", e.to_string());
            }
        }

        if let Err(e) = std::fs::write(path, content) {
            return DiffResult::err(path, "create", e.to_string());
        }

        DiffResult {
            path: path.display().to_string(),
            op: "create".to_string(),
            success: true,
            message: "file created".to_string(),
            backup_path: None,
            preview: false,
            diff: Some(render_diff("", content, path)),
        }
    }

    fn replace_file(
        &self,
        path: &Path,
        before_sha256: &str,
        content: &str,
        dry_run: bool,
    ) -> DiffResult {
        if !path.exists() {
            return DiffResult::err(path, "replace", "file does not exist".to_string());
        }

        let original = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return DiffResult::err(path, "replace", e.to_string()),
        };

        let current_hash = compute_sha256(path).unwrap_or_default();
        if !before_sha256.trim().is_empty() && current_hash != before_sha256.trim() {
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("eli-conflict");
            let conflict_path = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!("{filename}.eli-conflict"));
            if !dry_run {
                let _ = std::fs::write(&conflict_path, content);
            }
            return DiffResult {
                path: path.display().to_string(),
                op: "replace".to_string(),
                success: false,
                message: format!(
                    "hash mismatch (expected {}, got {}). wrote conflict to {}",
                    short_hash(before_sha256),
                    short_hash(&current_hash),
                    conflict_path.display()
                ),
                backup_path: Some(conflict_path.display().to_string()),
                preview: dry_run,
                diff: Some(render_diff(&original, content, path)),
            };
        }

        if dry_run {
            return DiffResult {
                path: path.display().to_string(),
                op: "replace".to_string(),
                success: true,
                message: "preview replace".to_string(),
                backup_path: None,
                preview: true,
                diff: Some(render_diff(&original, content, path)),
            };
        }

        let backup = create_backup(path).ok().flatten();
        if let Err(e) = std::fs::write(path, content) {
            return DiffResult::err(path, "replace", e.to_string());
        }

        DiffResult {
            path: path.display().to_string(),
            op: "replace".to_string(),
            success: true,
            message: "file replaced".to_string(),
            backup_path: backup.map(|p| p.display().to_string()),
            preview: false,
            diff: Some(render_diff(&original, content, path)),
        }
    }

    fn patch_file(
        &self,
        path: &Path,
        before_sha256: &str,
        patch_text: &str,
        dry_run: bool,
    ) -> DiffResult {
        if !path.exists() {
            return DiffResult::err(path, "patch", "file does not exist".to_string());
        }
        if patch_text.trim().is_empty() {
            return DiffResult::err(path, "patch", "patch text is empty".to_string());
        }

        let original = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return DiffResult::err(path, "patch", e.to_string()),
        };

        let current_hash = compute_sha256(path).unwrap_or_default();
        if !before_sha256.trim().is_empty() && current_hash != before_sha256.trim() {
            return DiffResult::err(
                path,
                "patch",
                format!(
                    "hash mismatch (expected {}, got {}). patch aborted",
                    short_hash(before_sha256),
                    short_hash(&current_hash)
                ),
            );
        }

        let patched = match apply_patch_to_text(&original, patch_text) {
            Ok(s) => s,
            Err(e) => return DiffResult::err(path, "patch", format!("patch failed: {e}")),
        };

        let preview_diff = render_diff(&original, &patched, path);
        if dry_run {
            return DiffResult {
                path: path.display().to_string(),
                op: "patch".to_string(),
                success: true,
                message: "preview patch".to_string(),
                backup_path: None,
                preview: true,
                diff: Some(preview_diff),
            };
        }

        let backup = create_backup(path).ok().flatten();
        if let Err(e) = std::fs::write(path, patched) {
            return DiffResult::err(path, "patch", e.to_string());
        }

        DiffResult {
            path: path.display().to_string(),
            op: "patch".to_string(),
            success: true,
            message: "patch applied".to_string(),
            backup_path: backup.map(|p| p.display().to_string()),
            preview: false,
            diff: Some(preview_diff),
        }
    }

    fn delete_file(&self, path: &Path, before_sha256: &str, dry_run: bool) -> DiffResult {
        if !path.exists() {
            return DiffResult::err(path, "delete", "file does not exist".to_string());
        }

        let original = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return DiffResult::err(path, "delete", e.to_string()),
        };

        let current_hash = compute_sha256(path).unwrap_or_default();
        if !before_sha256.trim().is_empty() && current_hash != before_sha256.trim() {
            return DiffResult::err(
                path,
                "delete",
                format!(
                    "hash mismatch (expected {}, got {}). delete aborted",
                    short_hash(before_sha256),
                    short_hash(&current_hash)
                ),
            );
        }

        if dry_run {
            return DiffResult {
                path: path.display().to_string(),
                op: "delete".to_string(),
                success: true,
                message: "preview delete".to_string(),
                backup_path: None,
                preview: true,
                diff: Some(render_diff(&original, "", path)),
            };
        }

        let backup = create_backup(path).ok().flatten();
        if let Err(e) = std::fs::remove_file(path) {
            return DiffResult::err(path, "delete", e.to_string());
        }

        DiffResult {
            path: path.display().to_string(),
            op: "delete".to_string(),
            success: true,
            message: "file deleted".to_string(),
            backup_path: backup.map(|p| p.display().to_string()),
            preview: false,
            diff: Some(render_diff(&original, "", path)),
        }
    }
}

impl DiffResult {
    fn err(path: &Path, op: &str, message: String) -> Self {
        Self {
            path: path.display().to_string(),
            op: op.to_string(),
            success: false,
            message,
            backup_path: None,
            preview: false,
            diff: None,
        }
    }
}

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

impl UndoManager {
    pub fn undo_step(diff_results: &[DiffResult]) -> Vec<String> {
        let mut messages = Vec::new();

        for result in diff_results.iter().rev() {
            if !result.success || result.preview {
                continue;
            }

            let path = PathBuf::from(&result.path);
            match result.op.as_str() {
                "create" => {
                    if path.exists() {
                        match std::fs::remove_file(&path) {
                            Ok(()) => {
                                messages.push(format!("Deleted created file: {}", path.display()))
                            }
                            Err(e) => {
                                messages.push(format!("Error deleting {}: {e}", path.display()))
                            }
                        }
                    }
                }
                "replace" | "patch" | "delete" => {
                    let Some(backup_path) = result.backup_path.as_deref() else {
                        messages.push(format!("Warning: No backup found for {}", path.display()));
                        continue;
                    };
                    let backup = PathBuf::from(backup_path);
                    if backup.exists() {
                        match std::fs::copy(&backup, &path) {
                            Ok(_) => {
                                messages.push(format!("Restored from backup: {}", path.display()))
                            }
                            Err(e) => messages.push(format!(
                                "Error restoring {} from {}: {e}",
                                path.display(),
                                backup.display()
                            )),
                        }
                    } else {
                        messages.push(format!(
                            "Warning: Backup missing for {}: {}",
                            path.display(),
                            backup.display()
                        ));
                    }
                }
                _ => {}
            }
        }

        messages
    }
}
