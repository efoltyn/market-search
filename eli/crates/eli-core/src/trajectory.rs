use crate::types::{ChatMessage, Usage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrajectoryStep {
    pub session_id: String,
    pub step_index: usize,
    pub timestamp: DateTime<Utc>,
    
    /// The exact context messages passed to the LLM for this generation
    pub input_messages: Vec<ChatMessage>,
    
    /// The raw response text from the model
    pub model_output_raw: String,
    
    /// The resulting observation/tool output from the environment
    /// (Captured after the commands are executed)
    pub observation: Option<String>,

    /// Token usage for this step
    pub usage: Option<Usage>,
}

pub struct TrajectoryLogger {
    log_path: PathBuf,
}

impl TrajectoryLogger {
    pub fn new(data_dir: PathBuf) -> Self {
        let log_path = data_dir.join("trajectories.jsonl");
        Self { log_path }
    }

    pub async fn append(&self, step: &TrajectoryStep) -> anyhow::Result<()> {
        if let Some(parent) = self.log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .await?;
            
        let line = serde_json::to_string(step)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }
}

