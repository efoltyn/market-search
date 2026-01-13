use crate::{config::Paths, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEvent {
    pub ts: DateTime<Utc>,
    pub kind: EventKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    UserMessage { content: String },
    AssistantMessage { content: String },
    Note { content: String },
}

pub struct SessionStore {
    sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn new(paths: &Paths) -> Self {
        Self {
            sessions_dir: paths.sessions_dir(),
        }
    }

    pub async fn append(&self, session_id: &str, event: &SessionEvent) -> Result<()> {
        tokio::fs::create_dir_all(&self.sessions_dir).await?;
        let path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        let line = serde_json::to_string(event)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }

    pub async fn read_all(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        let path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        let file = tokio::fs::File::open(path).await?;
        let mut lines = BufReader::new(file).lines();
        let mut out = Vec::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            out.push(serde_json::from_str(&line)?);
        }
        Ok(out)
    }
}

