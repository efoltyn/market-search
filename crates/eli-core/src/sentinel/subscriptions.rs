use super::evaluator::extract_var_names;
use super::io::{load_registry, save_registry};
use super::{resolve_paths, Severity, SpawnTarget, SubscriptionRegistry, SubscriptionSpec};
use crate::Result;
use chrono::Utc;
use evalexpr::build_operator_tree;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddSubscriptionInput {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub source_report_title: Option<String>,
    #[serde(default)]
    pub source_report_date: Option<String>,
    #[serde(default)]
    pub source_report_file: Option<String>,
    #[serde(default)]
    pub source_evidence: Option<String>,
    pub expr: String,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub source_set: Vec<String>,
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
    #[serde(default)]
    pub severity: Option<Severity>,
    #[serde(default)]
    pub why_template: Option<String>,
    #[serde(default)]
    pub prompt_template: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub spawn_agent: bool,
    #[serde(default)]
    pub spawn_target: Option<SpawnTarget>,
    #[serde(default)]
    pub spawn_cooldown_secs: Option<u64>,
    #[serde(default)]
    pub prediction: Option<String>,
    #[serde(default)]
    pub target_var: Option<String>,
    #[serde(default)]
    pub target_value: Option<f64>,
    #[serde(default)]
    pub deadline: Option<chrono::DateTime<Utc>>,
    #[serde(default)]
    pub fire_at: Option<chrono::DateTime<Utc>>,
}

pub fn add_subscription(
    sentinel_dir: Option<PathBuf>,
    queue_file: Option<PathBuf>,
    packets_file: Option<PathBuf>,
    input: AddSubscriptionInput,
) -> Result<SubscriptionSpec> {
    let paths = resolve_paths(sentinel_dir, queue_file, packets_file)?;
    paths.ensure_dirs()?;

    // Fail fast if expression is invalid.
    let _ = build_operator_tree(&input.expr)
        .map_err(|e| crate::Error::InvalidInput(format!("invalid --expr '{}': {e}", input.expr)))?;

    let discovered_vars = extract_var_names(&input.expr);
    let mut vars = sanitize_subscription_vars(input.vars)?;
    for var in discovered_vars {
        vars.entry(var.clone())
            .or_insert_with(|| super::evaluator::default_var_spec(&var));
    }

    let clean_name = sanitize_subscription_text(&input.name, "name")?;
    let clean_expr = sanitize_subscription_text(&input.expr, "expr")?;
    let clean_why = sanitize_subscription_text(
        input
            .why_template
            .as_deref()
            .unwrap_or("Sentinel condition breached."),
        "why_template",
    )?;
    let clean_prompt = sanitize_subscription_text(
        input.prompt_template.as_deref().unwrap_or(
            "Look across the current research stack and answer the only question that matters: where should capital sit right now, and did that answer materially change? Use Eli tools freely. If one tool comes back stale or low-signal, pivot immediately to other Eli tools and web context. Write only the actionable delta.",
        ),
        "prompt_template",
    )?;

    let mut registry = load_registry(&paths)?;
    let spec = SubscriptionSpec {
        id: format!("sub_{}", Uuid::new_v4().simple()),
        name: clean_name,
        title: input.title,
        source_report_title: input.source_report_title,
        source_report_date: input.source_report_date,
        source_report_file: input.source_report_file,
        source_evidence: input.source_evidence,
        expr: clean_expr,
        vars,
        source_set: input.source_set,
        cooldown_secs: input.cooldown_secs.unwrap_or(300).max(1),
        severity: input.severity.unwrap_or_default(),
        why_template: clean_why,
        prompt_template: clean_prompt,
        enabled: input.enabled.unwrap_or(true),
        last_triggered_at: None,
        spawn_agent: input.spawn_agent,
        spawn_target: input.spawn_target.unwrap_or_default(),
        spawn_cooldown_secs: input.spawn_cooldown_secs.unwrap_or(14_400).max(60),
        last_spawned_at: None,
        prediction: input.prediction,
        target_var: input.target_var,
        target_value: input.target_value,
        deadline: input.deadline,
        fire_at: input.fire_at,
        created_at: Some(Utc::now()),
        prediction_resolved: false,
        prediction_result: None,
        resolved_actual: None,
        resolved_at: None,
    };
    registry.subscriptions.push(spec.clone());
    save_registry(&paths, &registry)?;
    Ok(spec)
}

fn sanitize_subscription_text(raw: &str, field: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(crate::Error::InvalidInput(format!(
            "sentinel {field} must be non-empty"
        )));
    }
    if trimmed.chars().any(|ch| ch == '\0') {
        return Err(crate::Error::InvalidInput(format!(
            "sentinel {field} cannot contain NUL bytes"
        )));
    }
    Ok(trimmed.to_string())
}

fn sanitize_subscription_vars(vars: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (name, spec) in vars {
        let clean_name = sanitize_subscription_text(&name, "var name")?;
        let clean_spec = sanitize_subscription_text(&spec, "var spec")?;
        out.insert(clean_name, clean_spec);
    }
    Ok(out)
}

pub fn remove_subscription(
    sentinel_dir: Option<PathBuf>,
    queue_file: Option<PathBuf>,
    packets_file: Option<PathBuf>,
    id_or_name: &str,
) -> Result<Option<SubscriptionSpec>> {
    let paths = resolve_paths(sentinel_dir, queue_file, packets_file)?;
    paths.ensure_dirs()?;
    let mut registry = load_registry(&paths)?;
    let needle = id_or_name.trim().to_ascii_lowercase();
    let before = registry.subscriptions.len();
    let mut removed: Option<SubscriptionSpec> = None;
    registry.subscriptions.retain(|s| {
        let matched = s.id.eq_ignore_ascii_case(&needle) || s.name.to_ascii_lowercase() == needle;
        if matched {
            removed = Some(s.clone());
            false
        } else {
            true
        }
    });
    if registry.subscriptions.len() != before {
        save_registry(&paths, &registry)?;
    }
    Ok(removed)
}

pub fn list_subscriptions(
    sentinel_dir: Option<PathBuf>,
    queue_file: Option<PathBuf>,
    packets_file: Option<PathBuf>,
) -> Result<SubscriptionRegistry> {
    let paths = resolve_paths(sentinel_dir, queue_file, packets_file)?;
    paths.ensure_dirs()?;
    let mut registry = load_registry(&paths)?;
    registry.subscriptions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(registry)
}

pub fn touch_last_trigger(
    registry: &mut SubscriptionRegistry,
    id: &str,
    at: chrono::DateTime<Utc>,
) {
    for sub in &mut registry.subscriptions {
        if sub.id == id {
            sub.last_triggered_at = Some(at);
            break;
        }
    }
}
