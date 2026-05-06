impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            chat: ChatConfig::default(),
            finance: FinanceConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatConfig {
    #[serde(default = "default_model")]
    pub model: String,

    #[serde(default = "default_provider")]
    pub provider: ProviderKind,

    #[serde(default = "default_mem_steps")]
    pub mem_steps: usize,

    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    #[serde(default)]
    pub max_cmds: u32,

    #[serde(default, rename = "unsafe")]
    pub unsafe_mode: bool,

    #[serde(default = "default_auto")]
    pub auto: bool,

    #[serde(default = "default_max_auto")]
    pub max_auto: u32,

    #[serde(default)]
    pub mode: RunMode,

    #[serde(default)]
    pub approvals: ApprovalMode,

    /// Optional override: approvals for running commands (defaults to `approvals`).
    #[serde(default)]
    pub approvals_commands: Option<ApprovalMode>,

    /// Optional override: approvals for applying diffs (defaults to `approvals`).
    #[serde(default)]
    pub approvals_diffs: Option<ApprovalMode>,

    #[serde(default)]
    pub project_root: Option<PathBuf>,

    #[serde(default = "default_follow_cwd")]
    pub follow_cwd: bool,

    #[serde(default)]
    pub openrouter_api_key: Option<String>,

    #[serde(default)]
    pub openai_api_key: Option<String>,

    #[serde(default)]
    pub anthropic_api_key: Option<String>,

    #[serde(default)]
    pub openrouter_base_url: Option<String>,

    #[serde(default)]
    pub openai_base_url: Option<String>,

    #[serde(default)]
    pub anthropic_base_url: Option<String>,

    #[serde(default)]
    pub ollama_base_url: Option<String>,

    #[serde(default)]
    pub temperature: Option<f32>,

    #[serde(default)]
    pub max_tokens: Option<u32>,

    #[serde(default = "default_compact")]
    pub compact: bool,

    #[serde(default)]
    pub compact_trigger: Option<usize>,

    /// Optional token-based compaction trigger (estimated prompt tokens).
    /// When set, this takes precedence over `compact_trigger` (message-count trigger).
    #[serde(default = "default_compact_trigger_tokens")]
    pub compact_trigger_tokens: Option<usize>,

    #[serde(default)]
    pub compact_keep: Option<usize>,

    #[serde(default)]
    pub summary_model: Option<String>,

    #[serde(default = "default_parallel_commands")]
    pub parallel_commands: u32,

    #[serde(default = "default_parallel_subagents")]
    pub parallel_subagents: u32,

    #[serde(default = "default_scrollback_max_lines")]
    pub scrollback_max_lines: usize,

    #[serde(default)]
    pub display_mode: DisplayMode,

    #[serde(default)]
    pub auto_mode: AutoMode,

    #[serde(default)]
    pub sec_user_agent: Option<String>,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            provider: default_provider(),
            mem_steps: default_mem_steps(),
            timeout_secs: default_timeout_secs(),
            max_cmds: 0,
            unsafe_mode: false,
            auto: default_auto(),
            max_auto: default_max_auto(),
            mode: RunMode::default(),
            approvals: ApprovalMode::default(),
            approvals_commands: None,
            approvals_diffs: None,
            project_root: None,
            follow_cwd: default_follow_cwd(),
            openrouter_api_key: None,
            openai_api_key: None,
            anthropic_api_key: None,
            openrouter_base_url: None,
            openai_base_url: None,
            anthropic_base_url: None,
            ollama_base_url: None,
            temperature: None,
            max_tokens: None,
            compact: default_compact(),
            compact_trigger: None,
            compact_trigger_tokens: default_compact_trigger_tokens(),
            compact_keep: None,
            summary_model: None,
            parallel_commands: default_parallel_commands(),
            parallel_subagents: default_parallel_subagents(),
            scrollback_max_lines: default_scrollback_max_lines(),
            display_mode: DisplayMode::default(),
            auto_mode: AutoMode::default(),
            sec_user_agent: None,
        }
    }
}

impl ChatConfig {
    pub fn resolved_project_root(&self, cwd: &Path) -> Result<PathBuf> {
        if self.follow_cwd {
            return Ok(cwd.to_path_buf());
        }

        if let Some(root) = &self.project_root {
            return Ok(root.to_path_buf());
        }

        Ok(cwd.to_path_buf())
    }

    pub fn resolved_api_key(&self) -> Option<String> {
        match self.provider {
            ProviderKind::OpenRouter => self
                .openrouter_api_key
                .clone()
                .or_else(|| std::env::var("OPENROUTER_API_KEY").ok()),
            ProviderKind::OpenAI => self
                .openai_api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok()),
            ProviderKind::Anthropic => self
                .anthropic_api_key
                .clone()
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok()),
            ProviderKind::Ollama => None,
            ProviderKind::Mock => None,
        }
    }

    pub fn resolved_base_url(&self) -> Option<String> {
        match self.provider {
            ProviderKind::OpenRouter => self
                .openrouter_base_url
                .clone()
                .or_else(|| Some("https://openrouter.ai/api/v1".to_string())),
            ProviderKind::OpenAI => self
                .openai_base_url
                .clone()
                .or_else(|| Some("https://api.openai.com/v1".to_string())),
            ProviderKind::Anthropic => self
                .anthropic_base_url
                .clone()
                .or_else(|| Some("https://api.anthropic.com/v1".to_string())),
            ProviderKind::Ollama => self
                .ollama_base_url
                .clone()
                .or_else(|| Some("http://localhost:11434".to_string())),
            ProviderKind::Mock => None,
        }
    }

    pub fn resolved_summary_model(&self) -> &str {
        self.summary_model.as_deref().unwrap_or(&self.model)
    }

    pub fn resolved_command_approvals(&self) -> ApprovalMode {
        self.approvals_commands.unwrap_or(self.approvals)
    }

    pub fn resolved_diff_approvals(&self) -> ApprovalMode {
        self.approvals_diffs.unwrap_or(self.approvals)
    }

    pub fn resolved_compact_trigger(&self) -> usize {
        self.compact_trigger
            .unwrap_or(self.mem_steps.saturating_mul(5).max(30))
    }

    pub fn resolved_compact_trigger_tokens(&self) -> Option<usize> {
        self.compact_trigger_tokens
            .and_then(|v| if v == 0 { None } else { Some(v) })
    }

    pub fn resolved_compact_keep(&self) -> usize {
        self.compact_keep
            .unwrap_or(self.mem_steps.saturating_mul(2).max(12))
    }

    pub fn resolved_parallel_commands(&self) -> usize {
        self.parallel_commands.max(1) as usize
    }

    pub fn resolved_parallel_subagents(&self) -> usize {
        self.parallel_subagents.max(1) as usize
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Work,
    Read,
}

impl Default for RunMode {
    fn default() -> Self {
        RunMode::Read
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    Auto,
    Ask,
}

impl Default for ApprovalMode {
    fn default() -> Self {
        ApprovalMode::Auto
    }
}

/// Display verbosity mode
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode {
    /// Brief output: recent stream, recent tool, time summary
    Standard,
    /// Debug output: detailed logs and internal state
    Debug,
    /// Raw output: unprocessed streaming output
    Raw,
    /// Full output: all tools, full history, detailed logs
    Brain,
}

impl Default for DisplayMode {
    fn default() -> Self {
        DisplayMode::Standard
    }
}

/// Autonomous execution mode
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutoMode {
    /// Normal: human reviews plans
    Normal,
    /// Plan: requires human approval for plans
    Plan,
    /// Autonomous: AI self-reviews, loops until done
    Autonomous,
}

impl Default for AutoMode {
    fn default() -> Self {
        AutoMode::Autonomous
    }
}

pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}
