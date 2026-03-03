use super::evaluator::extract_var_names;
use super::io::{load_registry, save_registry};
use super::{resolve_paths, Severity, SubscriptionRegistry, SubscriptionSpec};
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
    let _ = build_operator_tree(&input.expr).map_err(|e| {
        crate::Error::InvalidInput(format!("invalid --expr '{}': {e}", input.expr))
    })?;

    let discovered_vars = extract_var_names(&input.expr);
    let mut vars = input.vars;
    for var in discovered_vars {
        vars.entry(var.clone())
            .or_insert_with(|| super::evaluator::default_var_spec(&var));
    }

    let mut registry = load_registry(&paths)?;
    let spec = SubscriptionSpec {
        id: format!("sub_{}", Uuid::new_v4().simple()),
        name: input.name.trim().to_string(),
        expr: input.expr.trim().to_string(),
        vars,
        source_set: input.source_set,
        cooldown_secs: input.cooldown_secs.unwrap_or(300).max(1),
        severity: input.severity.unwrap_or_default(),
        why_template: input
            .why_template
            .unwrap_or_else(|| "Sentinel condition breached.".to_string()),
        prompt_template: input.prompt_template.unwrap_or_else(|| {
            "Sentinel alert fired. Re-run macro analysis, update projector, and produce a delta brief."
                .to_string()
        }),
        enabled: input.enabled.unwrap_or(true),
        last_triggered_at: None,
    };
    registry.subscriptions.push(spec.clone());
    save_registry(&paths, &registry)?;
    Ok(spec)
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
        let matched =
            s.id.eq_ignore_ascii_case(&needle) || s.name.to_ascii_lowercase() == needle;
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

