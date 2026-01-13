use crate::types::ChatMessage;

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
        mem_steps.saturating_mul(6).max(24)
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
            out.push(ChatMessage::system(format!("Current Date and Time: {time_str}")));
        }
        if let Some(summary) = &self.summary {
            out.push(ChatMessage::system(format!("Memory summary:\n{summary}")));
        }
        out.extend(self.messages.iter().cloned());
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
