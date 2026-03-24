use super::evaluator::evaluate_subscription;
use super::io::{
    append_alert_packet, append_error_packet, clear_pid, clear_stop_request, load_daemon_state,
    load_registry, save_daemon_state, save_registry, stop_requested, write_pid,
};
use super::packets::{build_alert_packet, build_error_packet};
use super::{
    resolve_paths, AlertPacket, ConnectorState, SpawnBudgetState, SpawnTarget,
    SubscriptionRegistry, SubscriptionSpec,
};
use crate::Result;
use chrono::{DateTime, Duration, Utc};
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

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum AgentWriter {
    Codex,
    Claude,
    Gemini,
}

impl AgentWriter {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }
}

fn load_sentinel_config() -> crate::config::SentinelConfig {
    let config_paths = crate::config::Paths::discover().unwrap_or_else(|_| crate::config::Paths {
        config_dir: PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/eli"),
        data_dir: PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".eli/data"),
        cache_dir: PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".eli/cache"),
    });
    crate::config::load_or_default(&config_paths)
        .unwrap_or_default()
        .sentinel
}

fn resolve_reports_dir(cfg: &crate::config::SentinelConfig) -> PathBuf {
    let raw = &cfg.reports_dir;
    if raw.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(format!("{}{}", home, &raw[1..]))
    } else {
        PathBuf::from(raw)
    }
}

fn command_for_writer(cfg: &crate::config::SentinelConfig, writer: AgentWriter) -> String {
    match writer {
        AgentWriter::Codex => {
            let cmd = cfg.codex_agent_cmd.trim();
            if cmd.is_empty() {
                cfg.ai_agent_cmd.clone()
            } else {
                cmd.to_string()
            }
        }
        AgentWriter::Claude => {
            let cmd = cfg.claude_agent_cmd.trim();
            if cmd.is_empty() {
                cfg.ai_agent_cmd.clone()
            } else {
                cmd.to_string()
            }
        }
        AgentWriter::Gemini => {
            let cmd = cfg.gemini_agent_cmd.trim();
            if cmd.is_empty() {
                cfg.ai_agent_cmd.clone()
            } else {
                cmd.to_string()
            }
        }
    }
}

fn resolve_spawn_targets(
    subscription_target: &SpawnTarget,
    default_target: &SpawnTarget,
) -> Vec<AgentWriter> {
    let effective = if matches!(subscription_target, SpawnTarget::Default) {
        default_target
    } else {
        subscription_target
    };
    match effective {
        SpawnTarget::Default => vec![AgentWriter::Codex],
        SpawnTarget::Codex => vec![AgentWriter::Codex],
        SpawnTarget::Claude => vec![AgentWriter::Claude],
        SpawnTarget::Gemini => vec![AgentWriter::Gemini],
        SpawnTarget::Both => vec![AgentWriter::Codex, AgentWriter::Claude],
        SpawnTarget::All => vec![AgentWriter::Codex, AgentWriter::Claude, AgentWriter::Gemini],
    }
}

fn prune_spawn_history(history: &mut Vec<DateTime<Utc>>, now: DateTime<Utc>) {
    let cutoff = now - Duration::hours(1);
    history.retain(|ts| *ts >= cutoff);
}

fn spawn_history_mut<'a>(
    budget: &'a mut SpawnBudgetState,
    writer: AgentWriter,
) -> &'a mut Vec<DateTime<Utc>> {
    match writer {
        AgentWriter::Codex => &mut budget.codex_recent_spawns,
        AgentWriter::Claude => &mut budget.claude_recent_spawns,
        AgentWriter::Gemini => &mut budget.gemini_recent_spawns,
    }
}

fn max_spawns_per_hour(cfg: &crate::config::SentinelConfig, writer: AgentWriter) -> u32 {
    match writer {
        AgentWriter::Codex => cfg.codex_max_spawns_per_hour,
        AgentWriter::Claude => cfg.claude_max_spawns_per_hour,
        AgentWriter::Gemini => cfg.gemini_max_spawns_per_hour,
    }
}

fn spawn_budget_remaining(
    budget: &mut SpawnBudgetState,
    cfg: &crate::config::SentinelConfig,
    writer: AgentWriter,
    now: DateTime<Utc>,
) -> u32 {
    let max_per_hour = max_spawns_per_hour(cfg, writer);
    if max_per_hour == 0 {
        return 0;
    }
    let history = spawn_history_mut(budget, writer);
    prune_spawn_history(history, now);
    max_per_hour.saturating_sub(history.len() as u32)
}

fn record_spawn(budget: &mut SpawnBudgetState, writer: AgentWriter, at: DateTime<Utc>) {
    let history = spawn_history_mut(budget, writer);
    history.push(at);
    prune_spawn_history(history, at);
}

// ── agent spawn ───────────────────────────────────────────────────────────────

fn sanitize_name_for_path(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Scan ~/.eli/reports/ for recent .md reports and extract YAML front matter summaries.
fn load_report_journal(reports_dir: &std::path::Path, limit: usize) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(reports_dir) else {
        return vec![];
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.truncate(limit);

    files.into_iter().filter_map(|(_, path)| {
        let text = std::fs::read_to_string(&path).ok()?;
        // Extract YAML front matter between --- markers
        let trimmed = text.trim_start();
        if !trimmed.starts_with("---") { return None; }
        let after = &trimmed[3..];
        let end = after.find("\n---")?;
        let yaml = &after[..end];
        // Extract key fields
        let title = yaml_field(yaml, "title").unwrap_or_else(|| "Untitled".to_string());
        let rec = yaml_field(yaml, "recommendation").unwrap_or_else(|| "—".to_string());
        let triggered_at = yaml_field(yaml, "triggered_at").unwrap_or_else(|| "unknown".to_string());
        let sub_name = yaml_field(yaml, "sub_name").unwrap_or_default();
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        Some(format!(
            "  • [{triggered_at}] \"{title}\" (trigger: {sub_name})\n    Recommendation: {rec}\n    File: {fname}"
        ))
    }).collect()
}

fn yaml_field(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&format!("{key}:")) {
            let val = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

fn build_spawn_prompt(
    packet: &AlertPacket,
    triggering_sub: &SubscriptionSpec,
    registry: &SubscriptionRegistry,
    report_path: &std::path::Path,
    reports_dir: &std::path::Path,
) -> String {
    let now = Utc::now();

    // Build observed vars string
    let observed: String = packet
        .observed_vars
        .iter()
        .map(|(k, v)| format!("    {k} = {v:.4}"))
        .collect::<Vec<_>>()
        .join("\n");

    // Build registry status: all subscriptions, fired vs quiet
    let mut fired_lines = Vec::new();
    let mut quiet_lines = Vec::new();
    for sub in &registry.subscriptions {
        if !sub.enabled {
            continue;
        }
        let last_trigger = sub
            .last_triggered_at
            .map(|t| {
                let age = now.signed_duration_since(t).num_minutes();
                format!("fired {}m ago", age)
            })
            .unwrap_or_else(|| "never fired".to_string());
        let line = format!(
            "    [{sev:?}] \"{name}\" | expr: {expr} | {last_trigger}",
            sev = sub.severity,
            name = sub.name,
            expr = sub.expr,
            last_trigger = last_trigger
        );
        if sub.id == triggering_sub.id {
            fired_lines.push(format!("  ★ {line}  ← THIS TRIGGER"));
        } else if sub
            .last_triggered_at
            .map(|t| now.signed_duration_since(t).num_hours() < 24)
            .unwrap_or(false)
        {
            fired_lines.push(format!("  ↑ {line}  (also fired recently)"));
        } else {
            quiet_lines.push(format!("  · {line}"));
        }
    }
    let registry_section = {
        let mut lines = vec!["RECENTLY FIRED:".to_string()];
        lines.extend(fired_lines);
        lines.push(String::new());
        lines.push("QUIET (not triggered recently):".to_string());
        lines.extend(quiet_lines);
        lines.join("\n")
    };

    // Load recent report journal
    let journal = load_report_journal(reports_dir, 5);
    let journal_section = if journal.is_empty() {
        "  (no prior reports — this is the baseline)".to_string()
    } else {
        journal.join("\n")
    };

    let report_path_str = report_path.display();
    let reports_dir_str = reports_dir.display();
    let sanitized_name = sanitize_name_for_path(&triggering_sub.name);
    let trigger_type = if triggering_sub.fire_at.is_some() { "SCHEDULED" } else { "CONDITION" };

    // Build prediction ledger section if this is a prediction daemon.
    let prediction_ledger = if let Some(prediction_text) = &triggering_sub.prediction {
        let result_label = packet.prediction_result.as_deref().unwrap_or("UNKNOWN");
        let authored = triggering_sub.created_at
            .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let deadline_str = triggering_sub.deadline
            .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_else(|| "none".to_string());
        let fire_at_str = triggering_sub.fire_at
            .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_else(|| "n/a (condition-based)".to_string());
        let target_line = match (triggering_sub.target_var.as_deref(), triggering_sub.target_value) {
            (Some(v), Some(t)) => format!("TARGET VAR:    {} → {}", v, t),
            (Some(v), None) => format!("TARGET VAR:    {}", v),
            _ => String::new(),
        };
        let actual_line = match (triggering_sub.target_var.as_deref(), packet.prediction_actual) {
            (Some(v), Some(a)) => {
                let delta = triggering_sub.target_value.map(|t| a - t);
                let delta_str = delta.map(|d| format!(" (delta: {:+.4})", d)).unwrap_or_default();
                format!("ACTUAL VALUE:  {} = {}{}", v, a, delta_str)
            }
            _ => String::new(),
        };
        let outcome_context = match result_label {
            "HIT" => {
                let days_early = triggering_sub.deadline
                    .map(|d| d.signed_duration_since(now).num_days())
                    .unwrap_or(0);
                if days_early > 0 {
                    format!("Condition met {} days before deadline.", days_early)
                } else {
                    "Condition met.".to_string()
                }
            }
            "MISS" => {
                "Deadline elapsed — condition was NOT met.\n\
                 Reconcile which assumption failed:\n\
                 - Was the thesis directionally wrong?\n\
                 - Was the timing wrong (right thesis, wrong window)?\n\
                 - Did a countervailing force emerge that wasn't in the model?"
                    .to_string()
            }
            _ => String::new(),
        };
        format!(
            "\n═══ PREDICTION LEDGER ════════════════════════════════════════\n\
             This daemon encoded a PREDICTION. Your job is to reconcile.\n\
             AUTHORED:      {authored}\n\
             SCHEDULED FOR: {fire_at_str}\n\
             PREDICTION:    \"{prediction_text}\"\n\
             CONDITION:     {expr}\n\
             {target_line}\n\
             DEADLINE:      {deadline_str}\n\
             OUTCOME:       {result_label}\n\
             {actual_line}\n\n\
             {outcome_context}\n\
             ═══════════════════════════════════════════════════════════════\n",
            authored = authored,
            fire_at_str = fire_at_str,
            prediction_text = prediction_text,
            expr = triggering_sub.expr,
            target_line = target_line,
            deadline_str = deadline_str,
            result_label = result_label,
            actual_line = actual_line,
            outcome_context = outcome_context,
        )
    } else {
        String::new()
    };

    format!(
        r#"You are an ELI Sentinel Agent. A market condition just changed.
Your job is not to narrate the tape. Your job is to decide whether the capital-allocation answer changed, write the report file, and exit.
Do not trust your knowledge cutoff as current reality — use your tools to observe what is happening NOW.
Prefer Eli tools over open-ended browsing. Keep the run as short as possible without becoming vague or useless.

═══ TRIGGER ══════════════════════════════════════════════════════
Name:     {sub_name}
Type:     {trigger_type}
Expr:     {expr}
Severity: {severity:?}
Fired at: {triggered_at}
Context:  {why}

Observed values at trigger:
{observed}
{prediction_ledger}

═══ ALL SENTINEL CONDITIONS (situational awareness) ══════════════
These are all active conditions being monitored. The ones marked ★ or ↑
fired recently. The ones marked · did not — their silence is also signal.

{registry_section}

═══ REPORT JOURNAL (continuity across sessions) ══════════════════
The most recent research reports, from their YAML metadata.
You don't need to read them — this summary tells you what was being watched
and what the recommendation was. Your job is to surface what has CHANGED.

{journal_section}

═══ FOLLOW-UP DIRECTIVE FROM PRIOR RESEARCH ═════════════════════
This is the daemon-authored handoff instruction. Treat it as binding context
from the prior researcher. If it conflicts with the broader research framing
below, follow this directive first:

{follow_up_prompt}

═══ EXECUTION RULES ═════════════════════════════════════════════
1. Write the report to the exact path below.
2. Answer the user question first: WHY DO I CARE? Where should capital sit right now, and what changed?
3. If nothing material changed, say so directly and keep the note short. "No change" is acceptable; mush is not.
4. If one tool comes back stale, thin, or fallback-only, immediately pivot to other Eli tools. Do not center the note on tool failure.
5. Do not write a note whose main conclusion is "one ticker moved, others were stale." That is not actionable.
6. If the directive asks for a short note or smoke test, keep it tight, but still make it useful.
7. If an older report for this subscription exists, surface only the delta that matters.
8. Exit immediately after the file is written.

═══ RESEARCH STANDARD ═══════════════════════════════════════════
Use the three pillars when the situation calls for them:

1. native websearch for fresh narrative or breaking developments
2. prediction-market odds for market-implied probability shifts
3. current market, options, and historical data for relative-strength and positioning

Finance is relativity. The goal is not to summarize headlines. The goal is to find the signal and decide where to park capital.

If the report is about a market regime, use enough evidence to defend the allocation call. A good short alert usually answers:
- what changed
- why it matters
- where capital should sit now
- what would invalidate that view

═══ TOOL SELECTION RULES ════════════════════════════════════════
- Use only as many tools as needed, but if the first tool is stale or inconclusive, route around it.
- For event risk or regime shifts, prefer odds plus market data over a lone snapshot.
- For "still true?" timer alerts, do not emit a lazy heartbeat note. Re-check the thesis from a different angle.
- If options are thin, use timeseries, odds, macro, rates, FX, filings, or web context.
- If the trigger is generic, default to a capital-parking update, not a diary entry.

═══ WHAT TO AVOID ═══════════════════════════════════════════════
- no generic "delta brief"
- no article summary with no allocation call
- no single-tool note when other Eli tools can answer the question better
- no filler sections just because a template exists
- no hedged conclusion unless the evidence is genuinely mixed

═══ OUTPUT ══════════════════════════════════════════════════════
Write a Markdown file with YAML front matter. Save it to this exact path:
  {report_path_str}

The file MUST begin exactly like this (fill in the brackets):
---
title: "[concise report title]"
researcher: Codex
trigger: "{expr}"
severity: "{severity:?}"
triggered_at: "{triggered_at}"
sub_name: "{sub_name}"
tickers: []
markets: []
recommendation: "[one-line capital allocation call]"
---

Then write the report body in Markdown below the front matter.
Make it only as long as the directive requires.

Preferred body shape for useful alerts:

## Capital Now
One tight paragraph. Directly answer where capital should sit now.

## What Changed
Only the material delta.

## Evidence
2-5 bullets from the strongest tools you used.

## Invalidation
One short line: what would change your mind.

If the alert is truly minor and the answer did not change, collapse it to:
- Capital Now
- Why no change

Before you start researching, also look for any existing report for this subscription:
  ls {reports_dir_str}/*{sanitized_name}*.md
If one exists, read the most recent one and surface the delta — what changed.

Do not ask for confirmation. Write the file and exit.
"#,
        trigger_type = trigger_type,
        sub_name = triggering_sub.name,
        expr = triggering_sub.expr,
        severity = triggering_sub.severity,
        triggered_at = packet.triggered_at.format("%Y-%m-%dT%H:%M:%SZ"),
        why = packet.why_this_matters,
        observed = observed,
        prediction_ledger = prediction_ledger,
        registry_section = registry_section,
        journal_section = journal_section,
        follow_up_prompt = packet.follow_up_prompt,
        report_path_str = report_path_str,
        reports_dir_str = reports_dir_str,
        sanitized_name = sanitized_name,
    )
}

fn fire_and_forget_agent(ai_agent_cmd: &str, prompt_path: &std::path::Path) {
    // Spawn via login shell so PATH is fully resolved (claude/codex are npm-installed
    // into ~/.local/bin or similar, not in a bare daemon's PATH).
    // The prompt is read from the already-written audit file to avoid any escaping issues.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let cmd = format!(
        r#"{} "$(cat '{}')" </dev/null >/dev/null 2>/dev/null &"#,
        ai_agent_cmd,
        prompt_path.display()
    );
    let result = std::process::Command::new(&shell)
        .args(["-lc", &cmd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    if let Err(e) = result {
        eprintln!("[sentinel] failed to spawn agent via shell '{shell}': {e}");
    }
}

fn fix_prompt_for_connector(connector: &str, error: &str) -> String {
    let source_path = match connector {
        "polymarket" => "/Users/elifoltyn/Downloads/eli-code/eli/crates/eli-core/src/finance/providers/odds/polymarket.rs",
        "kalshi" => "/Users/elifoltyn/Downloads/eli-code/eli/crates/eli-core/src/finance/providers/odds/kalshi.rs",
        "pyth" => "/Users/elifoltyn/Downloads/eli-code/eli/crates/eli-core/src/finance/prices/fetch/service.rs",
        _ => "/Users/elifoltyn/Downloads/eli-code/eli/crates/eli-core/src/sentinel",
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

        let sentinel_cfg = load_sentinel_config();
        let reports_dir = resolve_reports_dir(&sentinel_cfg);

        let mut registry = load_registry(&paths)?;
        let mut registry_changed = false;
        let mut state = load_daemon_state(&paths)?;
        state.pid = Some(pid);
        state.heartbeat_at = Some(Utc::now());
        prune_spawn_history(&mut state.spawn_budget.codex_recent_spawns, Utc::now());
        prune_spawn_history(&mut state.spawn_budget.claude_recent_spawns, Utc::now());
        prune_spawn_history(&mut state.spawn_budget.gemini_recent_spawns, Utc::now());

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

                    let now_ts = Utc::now();
                    // Condition-based trigger.
                    // If this is a breakthrough prediction (has prediction text + deadline),
                    // it fires ONCE on first breach (no re-trigger after resolved).
                    // If it's a legacy watch daemon (no prediction), it re-triggers on cooldown.
                    let is_condition_hit = eval.triggered
                        && sub.fire_at.is_none()
                        && !sub.prediction_resolved
                        && cooldown_elapsed(sub.last_triggered_at, sub.cooldown_secs);
                    // Scheduled fire: fires once at fire_at time
                    let is_scheduled_fire = sub.fire_at
                        .map(|t| now_ts >= t)
                        .unwrap_or(false)
                        && !sub.prediction_resolved;
                    // Deadline miss for condition-based prediction daemons (no fire_at)
                    let is_deadline_miss = sub.prediction.is_some()
                        && !sub.prediction_resolved
                        && !eval.triggered
                        && sub.fire_at.is_none()
                        && sub.deadline.map(|d| now_ts >= d).unwrap_or(false);

                    if is_condition_hit || is_scheduled_fire || is_deadline_miss {
                        let prediction_result = if is_deadline_miss {
                            Some("MISS".to_string())
                        } else if is_scheduled_fire {
                            // At scheduled fire time, evaluate expr result for HIT/MISS
                            if sub.prediction.is_some() {
                                Some(if eval.triggered { "HIT".to_string() } else { "MISS".to_string() })
                            } else {
                                None // pure checkpoint, no prediction to resolve
                            }
                        } else {
                            // condition_hit
                            sub.prediction.as_ref().map(|_| "HIT".to_string())
                        };
                        let packet = build_alert_packet(
                            &paths,
                            sub,
                            &eval,
                            started.elapsed().as_millis() as u64,
                            prediction_result,
                        )?;
                        append_alert_packet(&paths, &packet)?;
                        sub.last_triggered_at = Some(now_ts);
                        // All prediction daemons resolve exactly once.
                        // Legacy watch daemons (no prediction, no fire_at) re-arm via cooldown.
                        if sub.prediction.is_some() || is_scheduled_fire {
                            sub.prediction_resolved = true;
                            sub.prediction_result = packet.prediction_result.clone();
                            sub.resolved_at = Some(now_ts);
                            sub.resolved_actual = packet.prediction_actual;
                        }
                        state.last_packet_id = Some(packet.packet_id.clone());
                        registry_changed = true;

                        // Spawn headless agent(s) using rolling hourly budgets per writer.
                        if sub.spawn_agent {
                            if let Err(e) = tokio::fs::create_dir_all(&reports_dir).await {
                                eprintln!("[sentinel] cannot create reports dir: {e}");
                            } else {
                                // Write prompt to spawn_prompts/ for audit trail
                                let spawn_prompts_dir = paths.root_dir.join("spawn_prompts");
                                let _ = tokio::fs::create_dir_all(&spawn_prompts_dir).await;
                                let ts_slug = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                                let safe_name = sanitize_name_for_path(&sub.name);
                                // Load full registry snapshot for situational awareness context
                                let registry_snapshot = load_registry(&paths).unwrap_or_default();
                                let spawn_targets = resolve_spawn_targets(
                                    &sub.spawn_target,
                                    &sentinel_cfg.default_spawn_target,
                                );
                                let mut spawned_any = false;
                                for writer in spawn_targets {
                                    let spawn_now = Utc::now();
                                    if spawn_budget_remaining(
                                        &mut state.spawn_budget,
                                        &sentinel_cfg,
                                        writer,
                                        spawn_now,
                                    ) == 0
                                    {
                                        eprintln!(
                                            "[sentinel] skipped {} spawn for '{}' (hourly budget exhausted)",
                                            writer.as_str(),
                                            sub.name
                                        );
                                        continue;
                                    }

                                    let report_filename =
                                        format!("{}-{}-{}.md", ts_slug, writer.as_str(), safe_name);
                                    let report_path = reports_dir.join(&report_filename);
                                    let prompt_path = spawn_prompts_dir.join(format!(
                                        "{}_{}_{}.txt",
                                        ts_slug,
                                        &sub.id,
                                        writer.as_str()
                                    ));
                                    let prompt = build_spawn_prompt(
                                        &packet,
                                        sub,
                                        &registry_snapshot,
                                        &report_path,
                                        &reports_dir,
                                    );
                                    let _ = tokio::fs::write(&prompt_path, &prompt).await;
                                    let ai_agent_cmd = command_for_writer(&sentinel_cfg, writer);
                                    fire_and_forget_agent(&ai_agent_cmd, &prompt_path);
                                    record_spawn(&mut state.spawn_budget, writer, spawn_now);
                                    sub.last_spawned_at = Some(spawn_now);
                                    spawned_any = true;
                                    eprintln!(
                                        "[sentinel] spawned {} for '{}' → {}",
                                        writer.as_str(),
                                        sub.name,
                                        report_path.display()
                                    );
                                }
                                if spawned_any {
                                    registry_changed = true;
                                }
                            }
                        }
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
