use super::evaluator::Evaluation;
use super::{
    AlertPacket, ErrorPacket, GeoTarget, PacketFreshness, PacketProvenance, PacketRunMeta,
    SentinelPaths, Severity, SubscriptionSpec, UiHints,
};
use crate::Result;
use chrono::Utc;
use std::collections::BTreeMap;

fn infer_geo_targets(text: &str) -> Vec<GeoTarget> {
    let lower = text.to_ascii_lowercase();
    let mut targets = Vec::new();
    fn push_unique(targets: &mut Vec<GeoTarget>, country_id: &str, intensity: f64) {
        if targets.iter().any(|t: &GeoTarget| t.country_id == country_id) {
            return;
        }
        targets.push(GeoTarget {
            country_id: country_id.to_string(),
            intensity,
        });
    }

    if lower.contains("iran") || lower.contains("hormuz") {
        push_unique(&mut targets, "IRN", 0.95);
    }
    if lower.contains("israel") || lower.contains("gaza") {
        push_unique(&mut targets, "ISR", 0.85);
    }
    if lower.contains("ukraine") {
        push_unique(&mut targets, "UKR", 0.85);
    }
    if lower.contains("russia") {
        push_unique(&mut targets, "RUS", 0.75);
    }
    if lower.contains("taiwan") {
        push_unique(&mut targets, "TWN", 0.8);
    }
    if lower.contains("china") {
        push_unique(&mut targets, "CHN", 0.7);
    }
    if lower.contains("india") {
        push_unique(&mut targets, "IND", 0.7);
    }
    if lower.contains("pakistan") {
        push_unique(&mut targets, "PAK", 0.7);
    }
    if lower.contains("oil") || lower.contains("wti") {
        push_unique(&mut targets, "SAU", 0.6);
    }
    if targets.is_empty() {
        push_unique(&mut targets, "USA", 0.45);
    }
    targets
}

fn pulse_for_severity(severity: &Severity) -> UiHints {
    match severity {
        Severity::Low => UiHints {
            pulse_color: Some("#22c55e".to_string()),
            pulse_seconds: Some(1.5),
        },
        Severity::Medium => UiHints {
            pulse_color: Some("#f59e0b".to_string()),
            pulse_seconds: Some(2.0),
        },
        Severity::High => UiHints {
            pulse_color: Some("#ef4444".to_string()),
            pulse_seconds: Some(2.4),
        },
        Severity::Critical => UiHints {
            pulse_color: Some("#dc2626".to_string()),
            pulse_seconds: Some(3.0),
        },
    }
}

fn playbook_markdown(
    packet: &AlertPacket,
    subscription_name: &str,
    observed_vars: &BTreeMap<String, f64>,
) -> String {
    let mut vars = String::new();
    for (k, v) in observed_vars {
        vars.push_str(&format!("- `{k}` = `{v:.6}`\n"));
    }
    format!(
        "# Sentinel Alert Playbook\n\n\
         - Packet: `{}`\n\
         - Subscription: `{}`\n\
         - Triggered At (UTC): `{}`\n\
         - Severity: `{}`\n\
         - Source: `{}`\n\
         - Instrument: `{}`\n\
         - Expression: `{}`\n\n\
         ## Why This Matters\n\
         {}\n\n\
         ## Observed Variables\n\
         {}\n\
         ## Required Follow-up\n\
         {}\n",
        packet.packet_id,
        subscription_name,
        packet.triggered_at.to_rfc3339(),
        serde_json::to_string(&packet.severity).unwrap_or_else(|_| "\"medium\"".to_string()),
        packet.source,
        packet.instrument,
        packet.expr,
        packet.why_this_matters,
        vars,
        packet.follow_up_prompt
    )
}

pub fn build_alert_packet(
    paths: &SentinelPaths,
    sub: &SubscriptionSpec,
    eval: &Evaluation,
    latency_ms: u64,
) -> Result<AlertPacket> {
    let now = Utc::now();
    let packet_id = format!("pkt_{}", uuid::Uuid::new_v4().simple());
    let primary = eval
        .observations
        .iter()
        .next()
        .map(|(_, obs)| obs.clone())
        .unwrap_or_else(|| super::evaluator::VariableObservation {
            value: 0.0,
            source: "unknown".to_string(),
            instrument: "unknown".to_string(),
            endpoint: "unknown".to_string(),
            symbol_or_id: "unknown".to_string(),
        });

    let why = if sub.why_template.trim().is_empty() {
        "Sentinel condition breached.".to_string()
    } else {
        sub.why_template.clone()
    };
    let follow_up_prompt = if sub.prompt_template.trim().is_empty() {
        "Re-run macro analysis and update the projector packet summary.".to_string()
    } else {
        sub.prompt_template.clone()
    };
    let dedupe_key = format!("{}::{}", sub.id, sub.expr.trim());
    let packet = AlertPacket {
        schema_version: "sentinel.packet.v1".to_string(),
        packet_kind: "alert".to_string(),
        packet_id: packet_id.clone(),
        triggered_at: now,
        source: primary.source.clone(),
        instrument: primary.instrument.clone(),
        expr: sub.expr.clone(),
        observed_vars: eval.observed_vars.clone(),
        why_this_matters: why,
        follow_up_prompt,
        playbook_path: String::new(),
        freshness: PacketFreshness {
            observed_at: now,
            collected_at: now,
            age_seconds: 0,
            state: "live".to_string(),
            origin: "provider_timestamp".to_string(),
            quality: "exact".to_string(),
        },
        provenance: PacketProvenance {
            provider: primary.source,
            endpoint: primary.endpoint,
            symbol_or_id: primary.symbol_or_id,
        },
        severity: sub.severity.clone(),
        dedupe_key,
        run_meta: PacketRunMeta {
            latency_ms,
            stdout_chars: 0,
            stored_bytes: 0,
        },
        geo_targets: infer_geo_targets(&format!("{} {}", sub.name, sub.expr)),
        ui_hints: pulse_for_severity(&sub.severity),
    };

    let playbook_path = paths
        .playbooks_dir
        .join(format!("{}_{}.md", now.format("%Y%m%dT%H%M%SZ"), sub.id));
    std::fs::create_dir_all(&paths.playbooks_dir)?;
    std::fs::write(
        &playbook_path,
        playbook_markdown(&packet, &sub.name, &packet.observed_vars),
    )?;

    let mut packet = packet;
    packet.playbook_path = playbook_path.display().to_string();
    Ok(packet)
}

pub fn build_error_packet(
    paths: &SentinelPaths,
    connector: &str,
    failure_count: usize,
    last_error: &str,
    suggested_fix_prompt: String,
) -> Result<ErrorPacket> {
    let now = Utc::now();
    let base = AlertPacket {
        schema_version: "sentinel.packet.v1".to_string(),
        packet_kind: "error".to_string(),
        packet_id: format!("err_{}", uuid::Uuid::new_v4().simple()),
        triggered_at: now,
        source: connector.to_string(),
        instrument: connector.to_string(),
        expr: "connector_failure_count >= 3".to_string(),
        observed_vars: BTreeMap::from([("failure_count".to_string(), failure_count as f64)]),
        why_this_matters: format!("Connector `{connector}` failed {failure_count} consecutive times."),
        follow_up_prompt: suggested_fix_prompt.clone(),
        playbook_path: String::new(),
        freshness: PacketFreshness {
            observed_at: now,
            collected_at: now,
            age_seconds: 0,
            state: "live".to_string(),
            origin: "transport_received".to_string(),
            quality: "estimated".to_string(),
        },
        provenance: PacketProvenance {
            provider: connector.to_string(),
            endpoint: "sentinel".to_string(),
            symbol_or_id: connector.to_string(),
        },
        severity: Severity::High,
        dedupe_key: format!("error::{connector}"),
        run_meta: PacketRunMeta {
            latency_ms: 0,
            stdout_chars: 0,
            stored_bytes: 0,
        },
        geo_targets: Vec::new(),
        ui_hints: UiHints {
            pulse_color: Some("#f97316".to_string()),
            pulse_seconds: Some(2.0),
        },
    };
    let playbook_path = paths.playbooks_dir.join(format!(
        "{}_error_{}.md",
        now.format("%Y%m%dT%H%M%SZ"),
        connector
    ));
    std::fs::create_dir_all(&paths.playbooks_dir)?;
    std::fs::write(
        &playbook_path,
        format!(
            "# Sentinel Connector Error\n\n\
             - Connector: `{connector}`\n\
             - Failure Count: `{failure_count}`\n\
             - Triggered At: `{}`\n\
             - Last Error: `{last_error}`\n\n\
             ## Suggested Fix Prompt\n\
             {}\n",
            now.to_rfc3339(),
            suggested_fix_prompt
        ),
    )?;
    let mut base = base;
    base.playbook_path = playbook_path.display().to_string();
    Ok(ErrorPacket {
        base,
        connector: connector.to_string(),
        failure_count,
        last_error: last_error.to_string(),
        suggested_fix_prompt,
    })
}
