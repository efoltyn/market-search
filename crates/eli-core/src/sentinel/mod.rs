use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub mod daemon;
pub mod evaluator;
pub mod io;
pub mod packets;
pub mod subscriptions;

pub const SUBSCRIPTIONS_FILE: &str = "subscriptions.toml";
pub const DAEMON_STATE_FILE: &str = "daemon_state.json";
pub const QUEUE_FILE: &str = "sentinel_queue.jsonl";
pub const PACKETS_FILE: &str = "intelligence_packets.jsonl";
pub const ERROR_PACKETS_FILE: &str = "error_packets.jsonl";
pub const STOP_FILE: &str = "stop.request";
pub const PID_FILE: &str = "daemon.pid";
pub const PLAYBOOKS_DIR: &str = "playbooks";
pub const LOG_FILE: &str = "daemon.log";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Default for Severity {
    fn default() -> Self {
        Self::Medium
    }
}

impl Severity {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" | "med" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "critical" | "crit" => Ok(Self::Critical),
            other => Err(Error::InvalidInput(format!(
                "invalid severity '{other}' (expected low|medium|high|critical)"
            ))),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpawnTarget {
    Default,
    Codex,
    Claude,
    Gemini,
    Both,
    All,
}

impl Default for SpawnTarget {
    fn default() -> Self {
        Self::Default
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PacketFreshness {
    pub observed_at: DateTime<Utc>,
    pub collected_at: DateTime<Utc>,
    pub age_seconds: i64,
    pub state: String,
    pub origin: String,
    pub quality: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PacketProvenance {
    pub provider: String,
    pub endpoint: String,
    pub symbol_or_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct PacketRunMeta {
    pub latency_ms: u64,
    pub stdout_chars: usize,
    pub stored_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GeoTarget {
    pub country_id: String,
    pub intensity: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiHints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pulse_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pulse_seconds: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertPacket {
    pub schema_version: String,
    pub packet_kind: String,
    pub packet_id: String,
    pub triggered_at: DateTime<Utc>,
    pub source: String,
    pub instrument: String,
    pub expr: String,
    pub observed_vars: BTreeMap<String, f64>,
    pub why_this_matters: String,
    pub follow_up_prompt: String,
    pub playbook_path: String,
    pub freshness: PacketFreshness,
    pub provenance: PacketProvenance,
    pub severity: Severity,
    pub dedupe_key: String,
    pub run_meta: PacketRunMeta,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub geo_targets: Vec<GeoTarget>,
    #[serde(default)]
    pub ui_hints: UiHints,
    /// "HIT" or "MISS" — only present when this packet resolves a prediction daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction_result: Option<String>,
    /// Copy of the prediction thesis text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction_text: Option<String>,
    /// Predicted numeric target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction_target: Option<f64>,
    /// Actual value of target_var at fire time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction_actual: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorPacket {
    #[serde(flatten)]
    pub base: AlertPacket,
    pub connector: String,
    pub failure_count: usize,
    pub last_error: String,
    pub suggested_fix_prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscriptionSpec {
    pub id: String,
    pub name: String,
    /// Human-readable prediction statement shown in the UI.
    /// Example: "Gold will reach $5,200 before Friday close"
    /// Falls back to name if not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Title of the report that authored this prediction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_report_title: Option<String>,
    /// Date the source report was written (ISO date string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_report_date: Option<String>,
    /// Exact filename of the source report (relative to reports_dir), for opening in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_report_file: Option<String>,
    /// Key evidence snippet or quote from the source report justifying this prediction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_evidence: Option<String>,
    pub expr: String,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub source_set: Vec<String>,
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub why_template: String,
    #[serde(default)]
    pub prompt_template: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub last_triggered_at: Option<DateTime<Utc>>,
    /// Spawn headless AI agent when this subscription triggers.
    #[serde(default)]
    pub spawn_agent: bool,
    /// Which headless writer(s) should fire when this subscription triggers.
    #[serde(default)]
    pub spawn_target: SpawnTarget,
    /// Legacy spawn cooldown retained for compatibility with older registries.
    /// Spawn routing now uses rolling hourly budgets per writer.
    #[serde(default = "default_spawn_cooldown_secs")]
    pub spawn_cooldown_secs: u64,
    /// Timestamp of last agent spawn.
    #[serde(default)]
    pub last_spawned_at: Option<DateTime<Utc>>,
    /// Human-readable prediction thesis this daemon encodes.
    /// When set, the daemon is a falsifiable prediction — fires on HIT (condition met)
    /// OR MISS (deadline elapsed without condition being met). Both outcomes spawn the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction: Option<String>,
    /// Which observed variable name to compare against target_value (e.g., "pyth_wti").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_var: Option<String>,
    /// Predicted numeric target for target_var (e.g., 90.0 for WTI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_value: Option<f64>,
    /// Prediction deadline — fires MISS if condition not met by this datetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    /// Scheduled fire time — daemon fires ONCE at this exact time regardless of expr.
    /// expr is still evaluated at fire time for HIT/MISS determination.
    /// If expr is "true" and no prediction text, this is a pure checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_at: Option<DateTime<Utc>>,
    /// When this prediction daemon was authored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// True once this prediction has resolved (HIT or MISS). Prevents double-fire.
    #[serde(default)]
    pub prediction_resolved: bool,
    /// Stored outcome once resolved: "HIT" or "MISS". Persisted so the UI can show it permanently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction_result: Option<String>,
    /// Actual observed value of target_var at resolution time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_actual: Option<f64>,
    /// Timestamp when prediction resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
}

fn default_true() -> bool {
    true
}

fn default_cooldown_secs() -> u64 {
    300
}

fn default_spawn_cooldown_secs() -> u64 {
    14_400 // 4 hours
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubscriptionRegistry {
    #[serde(default)]
    pub subscriptions: Vec<SubscriptionSpec>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConnectorState {
    pub ok: bool,
    pub failure_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_packet_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DaemonState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub connector_status: BTreeMap<String, ConnectorState>,
    #[serde(default)]
    pub queue_offsets: BTreeMap<String, u64>,
    #[serde(default)]
    pub spawn_budget: SpawnBudgetState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_packet_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpawnBudgetState {
    #[serde(default)]
    pub codex_recent_spawns: Vec<DateTime<Utc>>,
    #[serde(default)]
    pub claude_recent_spawns: Vec<DateTime<Utc>>,
    #[serde(default)]
    pub gemini_recent_spawns: Vec<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct SentinelPaths {
    pub root_dir: PathBuf,
    pub queue_file: PathBuf,
    pub packets_file: PathBuf,
    pub error_packets_file: PathBuf,
    pub subscriptions_file: PathBuf,
    pub daemon_state_file: PathBuf,
    pub stop_file: PathBuf,
    pub pid_file: PathBuf,
    pub log_file: PathBuf,
    pub playbooks_dir: PathBuf,
}

impl SentinelPaths {
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root_dir)?;
        std::fs::create_dir_all(&self.playbooks_dir)?;
        Ok(())
    }
}

pub fn default_sentinel_root_dir() -> Result<PathBuf> {
    let paths = crate::config::Paths::discover()?;
    Ok(paths.data_dir.join("sentinel"))
}

pub fn resolve_paths(
    sentinel_dir: Option<PathBuf>,
    queue_file: Option<PathBuf>,
    packets_file: Option<PathBuf>,
) -> Result<SentinelPaths> {
    let root_dir = sentinel_dir.unwrap_or(default_sentinel_root_dir()?);
    let queue_file = queue_file.unwrap_or_else(|| root_dir.join(QUEUE_FILE));
    let packets_file = packets_file.unwrap_or_else(|| root_dir.join(PACKETS_FILE));
    Ok(SentinelPaths {
        error_packets_file: root_dir.join(ERROR_PACKETS_FILE),
        subscriptions_file: root_dir.join(SUBSCRIPTIONS_FILE),
        daemon_state_file: root_dir.join(DAEMON_STATE_FILE),
        stop_file: root_dir.join(STOP_FILE),
        pid_file: root_dir.join(PID_FILE),
        log_file: root_dir.join(LOG_FILE),
        playbooks_dir: root_dir.join(PLAYBOOKS_DIR),
        root_dir,
        queue_file,
        packets_file,
    })
}
