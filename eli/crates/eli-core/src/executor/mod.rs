use crate::{config::ChatConfig, memory::Memory, types::ChatMessage};
use uuid::Uuid;

pub mod command_runner;

#[derive(Debug)]
pub struct Session {
    pub id: Uuid,
    pub config: ChatConfig,
    pub memory: Memory,
}

impl Session {
    pub fn new(config: ChatConfig) -> Self {
        let memory = Memory::new(config.mem_steps);
        Self {
            id: Uuid::new_v4(),
            config,
            memory,
        }
    }

    pub fn push_user(&mut self, content: impl Into<String>) {
        self.memory.push(ChatMessage::user(content));
    }

    pub fn push_assistant(&mut self, content: impl Into<String>) {
        self.memory.push(ChatMessage::assistant(content));
    }
}
