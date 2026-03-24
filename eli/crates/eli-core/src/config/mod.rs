use crate::{types::ProviderKind, Error, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub chat: ChatConfig,
    #[serde(default)]
    pub finance: FinanceConfig,
    #[serde(default)]
    pub sentinel: SentinelConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct FinanceConfig {
    #[serde(default)]
    pub fred_api_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SentinelConfig {
    /// Legacy default command used when no explicit Codex/Claude command is configured.
    #[serde(default = "default_ai_agent_cmd")]
    pub ai_agent_cmd: String,

    /// Preferred default writer when a subscription does not override spawn target.
    #[serde(default = "default_spawn_target_preference")]
    pub default_spawn_target: crate::sentinel::SpawnTarget,

    /// Command used to spawn headless Codex on trigger.
    #[serde(default = "default_codex_agent_cmd")]
    pub codex_agent_cmd: String,

    /// Command used to spawn headless Claude on trigger.
    #[serde(default = "default_claude_agent_cmd")]
    pub claude_agent_cmd: String,

    /// Command used to spawn headless Gemini CLI on trigger.
    #[serde(default = "default_gemini_agent_cmd")]
    pub gemini_agent_cmd: String,

    /// Rolling maximum Codex spawns allowed per hour.
    #[serde(default = "default_codex_max_spawns_per_hour")]
    pub codex_max_spawns_per_hour: u32,

    /// Rolling maximum Claude spawns allowed per hour.
    #[serde(default = "default_claude_max_spawns_per_hour")]
    pub claude_max_spawns_per_hour: u32,

    /// Rolling maximum Gemini spawns allowed per hour.
    #[serde(default = "default_gemini_max_spawns_per_hour")]
    pub gemini_max_spawns_per_hour: u32,

    /// Directory where sentinel-spawned research reports are written.
    #[serde(default = "default_sentinel_reports_dir")]
    pub reports_dir: String,
}

impl Default for SentinelConfig {
    fn default() -> Self {
        Self {
            ai_agent_cmd: default_ai_agent_cmd(),
            default_spawn_target: default_spawn_target_preference(),
            codex_agent_cmd: default_codex_agent_cmd(),
            claude_agent_cmd: default_claude_agent_cmd(),
            gemini_agent_cmd: default_gemini_agent_cmd(),
            codex_max_spawns_per_hour: default_codex_max_spawns_per_hour(),
            claude_max_spawns_per_hour: default_claude_max_spawns_per_hour(),
            gemini_max_spawns_per_hour: default_gemini_max_spawns_per_hour(),
            reports_dir: default_sentinel_reports_dir(),
        }
    }
}

include!("model.rs");
include!("io.rs");
include!("defaults.rs");
