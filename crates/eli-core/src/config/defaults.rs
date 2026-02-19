fn default_model() -> String {
    DEFAULT_OPENROUTER_MODEL.to_string()
}

pub const DEFAULT_OPENROUTER_MODEL: &str = "arcee-ai/trinity-large-preview:free";

fn default_provider() -> ProviderKind {
    ProviderKind::OpenRouter
}

fn default_mem_steps() -> usize {
    0
}

fn default_timeout_secs() -> u64 {
    120
}

fn default_auto() -> bool {
    true // dynamic steps - model decides when done
}

fn default_max_auto() -> u32 {
    50 // safety limit
}

fn default_follow_cwd() -> bool {
    true
}

fn default_compact() -> bool {
    true
}

fn default_compact_trigger_tokens() -> Option<usize> {
    Some(100_000)
}

fn default_parallel_commands() -> u32 {
    50
}

fn default_parallel_subagents() -> u32 {
    50
}

fn default_scrollback_max_lines() -> usize {
    10_000
}
