use super::{AlertPacket, DaemonState, ErrorPacket, SentinelPaths, SubscriptionRegistry};
use crate::{Error, Result};
use chrono::Utc;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, json)?;
    Ok(())
}

fn append_jsonl_line<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(value)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

pub fn load_registry(paths: &SentinelPaths) -> Result<SubscriptionRegistry> {
    if !paths.subscriptions_file.exists() {
        return Ok(SubscriptionRegistry::default());
    }
    let raw = std::fs::read_to_string(&paths.subscriptions_file)?;
    let mut registry = match toml::from_str::<SubscriptionRegistry>(&raw) {
        Ok(parsed) => parsed,
        Err(primary_err) => {
            let backup_path = subscriptions_backup_path(&paths.subscriptions_file);
            if backup_path.exists() {
                let backup_raw = std::fs::read_to_string(&backup_path)?;
                if let Ok(parsed) = toml::from_str::<SubscriptionRegistry>(&backup_raw) {
                    parsed
                } else {
                    return Err(primary_err.into());
                }
            } else {
                return Err(primary_err.into());
            }
        }
    };
    // Deduplicate by id — a duplicate key in the TOML file can corrupt the daemon.
    let mut seen_ids = std::collections::HashSet::new();
    registry.subscriptions.retain(|s| seen_ids.insert(s.id.clone()));
    Ok(registry)
}

pub fn save_registry(paths: &SentinelPaths, registry: &SubscriptionRegistry) -> Result<()> {
    let raw = toml::to_string_pretty(registry)?;
    std::fs::create_dir_all(&paths.root_dir)?;
    let tmp_path = paths.subscriptions_file.with_extension("toml.tmp");
    let backup_path = subscriptions_backup_path(&paths.subscriptions_file);
    if paths.subscriptions_file.exists() {
        let _ = std::fs::copy(&paths.subscriptions_file, &backup_path);
    }
    std::fs::write(&tmp_path, raw)?;
    std::fs::rename(&tmp_path, &paths.subscriptions_file)?;
    Ok(())
}

fn subscriptions_backup_path(path: &Path) -> PathBuf {
    path.with_extension("toml.bak")
}

pub fn load_daemon_state(paths: &SentinelPaths) -> Result<DaemonState> {
    if !paths.daemon_state_file.exists() {
        return Ok(DaemonState::default());
    }
    let raw = std::fs::read_to_string(&paths.daemon_state_file)?;
    let parsed: DaemonState = serde_json::from_str(&raw)?;
    Ok(parsed)
}

pub fn save_daemon_state(paths: &SentinelPaths, state: &DaemonState) -> Result<()> {
    write_json_file(&paths.daemon_state_file, state)
}

pub fn write_pid(paths: &SentinelPaths, pid: u32) -> Result<()> {
    std::fs::create_dir_all(&paths.root_dir)?;
    std::fs::write(&paths.pid_file, format!("{pid}\n"))?;
    Ok(())
}

pub fn read_pid(paths: &SentinelPaths) -> Result<Option<u32>> {
    if !paths.pid_file.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&paths.pid_file)?;
    let pid = raw
        .trim()
        .parse::<u32>()
        .map_err(|e| Error::Other(format!("parse daemon pid failed: {e}")))?;
    Ok(Some(pid))
}

pub fn clear_pid(paths: &SentinelPaths) -> Result<()> {
    if paths.pid_file.exists() {
        std::fs::remove_file(&paths.pid_file)?;
    }
    Ok(())
}

pub fn stop_requested(paths: &SentinelPaths) -> bool {
    paths.stop_file.exists()
}

pub fn write_stop_request(paths: &SentinelPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.root_dir)?;
    std::fs::write(&paths.stop_file, format!("{}\n", Utc::now().to_rfc3339()))?;
    Ok(())
}

pub fn clear_stop_request(paths: &SentinelPaths) -> Result<()> {
    if paths.stop_file.exists() {
        std::fs::remove_file(&paths.stop_file)?;
    }
    Ok(())
}

pub fn append_alert_packet(paths: &SentinelPaths, packet: &AlertPacket) -> Result<()> {
    append_jsonl_line(&paths.queue_file, packet)?;
    append_jsonl_line(&paths.packets_file, packet)?;
    Ok(())
}

pub fn append_error_packet(paths: &SentinelPaths, packet: &ErrorPacket) -> Result<()> {
    append_jsonl_line(&paths.queue_file, packet)?;
    append_jsonl_line(&paths.error_packets_file, packet)?;
    Ok(())
}

pub fn open_log(paths: &SentinelPaths) -> Result<File> {
    if let Some(parent) = paths.log_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_file)
        .map_err(Into::into)
}
