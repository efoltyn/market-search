fn cache_path(cache_dir: &Path, key: &str) -> PathBuf {
    cache_dir
        .join("finance")
        .join("timeseries")
        .join(format!("{key}.json"))
}

fn debug_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ELI_DEBUG_DIR") {
        return PathBuf::from(dir);
    }
    std::env::temp_dir().join("eli-debug")
}

pub(crate) fn write_debug_payload(tool: &str, request: &str, payload: &str) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(request.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let ts = Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let filename = format!("{tool}_{ts}_{}.json", &hash[..12.min(hash.len())]);
    let path = debug_dir().join(tool).join(filename);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return None;
        }
    }
    if std::fs::write(&path, payload).is_err() {
        return None;
    }
    Some(path.display().to_string())
}

