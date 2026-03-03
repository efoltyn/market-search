use super::evaluator::evaluate_subscription;
use super::io::{
    append_alert_packet, append_error_packet, clear_pid, clear_stop_request, load_daemon_state,
    load_registry, save_daemon_state, save_registry, stop_requested, write_pid,
};
use super::packets::{build_alert_packet, build_error_packet};
use super::{resolve_paths, ConnectorState};
use crate::Result;
use chrono::Utc;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub sentinel_dir: Option<PathBuf>,
    pub queue_file: Option<PathBuf>,
    pub packets_file: Option<PathBuf>,
    pub interval_secs: u64,
}

fn cooldown_elapsed(last: Option<chrono::DateTime<Utc>>, cooldown_secs: u64) -> bool {
    match last {
        None => true,
        Some(ts) => Utc::now().signed_duration_since(ts).num_seconds() >= cooldown_secs as i64,
    }
}

fn fix_prompt_for_connector(connector: &str, error: &str) -> String {
    let source_path = match connector {
        "polymarket" => "/Users/elifoltyn/Desktop/eli-code/eli/crates/eli-core/src/finance/providers/odds/polymarket.rs",
        "kalshi" => "/Users/elifoltyn/Desktop/eli-code/eli/crates/eli-core/src/finance/providers/odds/kalshi.rs",
        "pyth" => "/Users/elifoltyn/Desktop/eli-code/eli/crates/eli-core/src/finance/prices/fetch/service.rs",
        _ => "/Users/elifoltyn/Desktop/eli-code/eli/crates/eli-core/src/sentinel",
    };
    format!(
        "Sentinel connector `{connector}` is failing repeatedly. Investigate `{source_path}`. Last error: {error}. Patch the connector, rebuild `bin/eli`, restart `eli sentinel start`, and validate with `eli sentinel status`."
    )
}

pub async fn run_daemon(opts: DaemonOptions) -> Result<()> {
    let paths = resolve_paths(opts.sentinel_dir, opts.queue_file, opts.packets_file)?;
    paths.ensure_dirs()?;
    clear_stop_request(&paths)?;

    let pid = std::process::id();
    write_pid(&paths, pid)?;

    let mut state = load_daemon_state(&paths)?;
    if state.started_at.is_none() {
        state.started_at = Some(Utc::now());
    }
    state.pid = Some(pid);
    state.heartbeat_at = Some(Utc::now());
    save_daemon_state(&paths, &state)?;

    let interval_secs = opts.interval_secs.max(1);
    loop {
        if stop_requested(&paths) {
            break;
        }

        let mut registry = load_registry(&paths)?;
        let mut registry_changed = false;
        let mut state = load_daemon_state(&paths)?;
        state.pid = Some(pid);
        state.heartbeat_at = Some(Utc::now());

        for sub in registry.subscriptions.iter_mut().filter(|s| s.enabled) {
            let started = std::time::Instant::now();
            match evaluate_subscription(sub).await {
                Ok(eval) => {
                    for obs in eval.observations.values() {
                        let entry = state
                            .connector_status
                            .entry(obs.source.clone())
                            .or_insert_with(ConnectorState::default);
                        entry.ok = true;
                        entry.failure_count = 0;
                        entry.last_error = None;
                        entry.last_success_at = Some(Utc::now());
                    }

                    if eval.triggered && cooldown_elapsed(sub.last_triggered_at, sub.cooldown_secs) {
                        let packet = build_alert_packet(
                            &paths,
                            sub,
                            &eval,
                            started.elapsed().as_millis() as u64,
                        )?;
                        append_alert_packet(&paths, &packet)?;
                        sub.last_triggered_at = Some(Utc::now());
                        state.last_packet_id = Some(packet.packet_id.clone());
                        registry_changed = true;
                    }
                }
                Err(err) => {
                    let now = Utc::now();
                    let entry = state
                        .connector_status
                        .entry(err.connector.clone())
                        .or_insert_with(ConnectorState::default);
                    entry.ok = false;
                    entry.failure_count = entry.failure_count.saturating_add(1);
                    entry.last_error = Some(err.message.clone());

                    let emit_error = entry.failure_count >= 3
                        && entry
                            .last_error_packet_at
                            .map(|ts| now.signed_duration_since(ts).num_seconds() >= 300)
                            .unwrap_or(true);
                    if emit_error {
                        let fix_prompt = fix_prompt_for_connector(&err.connector, &err.message);
                        let packet = build_error_packet(
                            &paths,
                            &err.connector,
                            entry.failure_count,
                            &err.message,
                            fix_prompt,
                        )?;
                        append_error_packet(&paths, &packet)?;
                        entry.last_error_packet_at = Some(now);
                        state.last_packet_id = Some(packet.base.packet_id.clone());
                    }
                }
            }
        }

        if registry_changed {
            save_registry(&paths, &registry)?;
        }
        state.heartbeat_at = Some(Utc::now());
        save_daemon_state(&paths, &state)?;
        tokio::time::sleep(tokio::time::Duration::from_secs(interval_secs)).await;
    }

    clear_stop_request(&paths)?;
    clear_pid(&paths)?;
    let mut state = load_daemon_state(&paths)?;
    state.pid = None;
    state.heartbeat_at = Some(Utc::now());
    save_daemon_state(&paths, &state)?;
    Ok(())
}
