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
