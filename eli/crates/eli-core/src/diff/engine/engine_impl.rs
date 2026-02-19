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

