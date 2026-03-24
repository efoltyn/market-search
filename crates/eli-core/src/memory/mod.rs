use crate::types::{ChatMessage, Role};
use serde_json;

#[derive(Clone, Debug)]
pub struct Memory {
    max_messages: usize,
    system: Option<String>,
    summary: Option<String>,
    messages: Vec<ChatMessage>,
}

impl Memory {
    pub fn new(mem_steps: usize) -> Self {
        let max_messages = Self::max_messages_for(mem_steps);
        Self {
            max_messages,
            system: None,
            summary: None,
            messages: Vec::new(),
        }
    }

    pub fn max_messages_for(mem_steps: usize) -> usize {
        // "Infinite" memory: rely on compaction instead of hard-delete trim.
        // Use a very high cap when mem_steps is 0 to avoid trimming.
        if mem_steps == 0 {
            return 1_000_000;
        }
        // The compaction trigger (usually mem_steps * 5) will fire way before this.
        mem_steps.saturating_mul(200).max(2000)
    }

    pub fn set_system(&mut self, content: impl Into<String>) {
        self.system = Some(content.into());
    }

    pub fn set_summary(&mut self, summary: Option<String>) {
        self.summary = summary;
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    pub fn push(&mut self, msg: ChatMessage) {
        self.messages.push(msg);
        self.trim();
    }

    pub fn last_role(&self) -> Option<crate::types::Role> {
        self.messages.last().map(|m| m.role)
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn max_messages(&self) -> usize {
        self.max_messages
    }

    pub fn recent_messages(&self, max: usize) -> Vec<ChatMessage> {
        if max == 0 {
            return Vec::new();
        }
        let len = self.messages.len();
        if len <= max {
            return self.messages.clone();
        }
        self.messages[len - max..].to_vec()
    }

    pub fn older_messages(&self, keep_last: usize) -> Vec<ChatMessage> {
        let len = self.messages.len();
        if len <= keep_last {
            return Vec::new();
        }
        self.messages[..len - keep_last].to_vec()
    }

    pub fn drop_older(&mut self, keep_last: usize) {
        let len = self.messages.len();
        if len <= keep_last {
            return;
        }
        self.messages.drain(0..len - keep_last);
    }

    pub fn context(&self) -> Vec<ChatMessage> {
        let mut out = Vec::new();
        if let Some(system) = &self.system {
            out.push(ChatMessage::system(system.clone()));

            // Inject current date/time context
            let now = chrono::Local::now();
            let time_str = now.format("%A, %B %e, %Y %H:%M:%S").to_string();
            out.push(ChatMessage::system(format!(
                "Current Date and Time: {time_str}"
            )));
        }
        if let Some(summary) = &self.summary {
            out.push(ChatMessage::system(format!("Memory summary:\n{summary}")));
        }
        out.extend(self.messages.iter().cloned());
        out
    }

    /// Like `context()` but compresses messages older than `keep_full` from the end.
    /// Old tool observations get truncated. Old assistant JSON gets `diffs`/`screen`/`subagents`
    /// stripped and large text fields capped — can remove megabytes of stale file content
    /// that the model never needs to re-read.
    pub fn context_compressed(&self, keep_full: usize) -> Vec<ChatMessage> {
        let mut out = Vec::new();
        if let Some(system) = &self.system {
            out.push(ChatMessage::system(system.clone()));
            let now = chrono::Local::now();
            let time_str = now.format("%A, %B %e, %Y %H:%M:%S").to_string();
            out.push(ChatMessage::system(format!(
                "Current Date and Time: {time_str}"
            )));
        }
        if let Some(summary) = &self.summary {
            out.push(ChatMessage::system(format!("Memory summary:\n{summary}")));
        }
        let len = self.messages.len();
        let full_start = len.saturating_sub(keep_full);
        for (i, msg) in self.messages.iter().enumerate() {
            if i >= full_start {
                out.push(msg.clone());
            } else {
                out.push(compress_old_message(msg));
            }
        }
        out
    }

    fn trim(&mut self) {
        if self.messages.len() <= self.max_messages {
            return;
        }
        let drop = self.messages.len().saturating_sub(self.max_messages);
        self.messages.drain(0..drop);
    }
}

/// Compress a single old message to reduce token count.
/// Strips large fields from assistant JSON, truncates tool observations.
fn compress_old_message(msg: &ChatMessage) -> ChatMessage {
    match msg.role {
        Role::Tool => {
            // Tool observations can be huge — keep first 500 chars + byte count
            const TOOL_CAP: usize = 500;
            if msg.content.len() <= TOOL_CAP {
                return msg.clone();
            }
            let prefix: String = msg.content.chars().take(TOOL_CAP).collect();
            let suppressed = msg.content.len().saturating_sub(TOOL_CAP);
            let name = msg.name.as_deref().unwrap_or("eli");
            ChatMessage::tool(format!("{prefix}... [{suppressed} bytes suppressed]"), name)
        }

        Role::Assistant => {
            // JSON assistant turns: strip diffs/screen/subagents (huge), cap text fields.
            // diffs alone can hold entire file contents — useless once applied.
            if let Ok(mut map) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&msg.content)
            {
                map.remove("diffs");
                map.remove("screen");
                map.remove("subagents");

                cap_str_field(&mut map, "notes", 200);
                cap_str_field(&mut map, "plan", 150);
                cap_str_field(&mut map, "focus", 100);

                if let Some(synth) = map.get_mut("synthesis").and_then(|v| v.as_object_mut()) {
                    cap_str_field(synth, "answer", 200);
                }

                if let Ok(compressed) = serde_json::to_string(&map) {
                    return ChatMessage::assistant(compressed);
                }
            }
            // Fallback: plain-text truncation
            plain_truncate(msg, 600)
        }

        Role::User => {
            // User messages are usually short. Cap only extremely large ones (pasted files).
            plain_truncate(msg, 3000)
        }

        Role::System => msg.clone(),
    }
}

fn cap_str_field(map: &mut serde_json::Map<String, serde_json::Value>, key: &str, max: usize) {
    if let Some(val) = map.get_mut(key) {
        if let Some(s) = val.as_str() {
            if s.len() > max {
                let truncated: String = s.chars().take(max).collect();
                *val = serde_json::Value::String(format!("{truncated}…"));
            }
        }
    }
}

fn plain_truncate(msg: &ChatMessage, max_chars: usize) -> ChatMessage {
    if msg.content.len() <= max_chars {
        return msg.clone();
    }
    let prefix: String = msg.content.chars().take(max_chars).collect();
    let suppressed = msg.content.len().saturating_sub(max_chars);
    match msg.role {
        Role::Tool => {
            let name = msg.name.as_deref().unwrap_or("eli");
            ChatMessage::tool(format!("{prefix}... [{suppressed} bytes]"), name)
        }
        Role::Assistant => ChatMessage::assistant(format!("{prefix}... [{suppressed} bytes]")),
        Role::User => ChatMessage::user(format!("{prefix}... [{suppressed} bytes]")),
        Role::System => msg.clone(),
    }
}
