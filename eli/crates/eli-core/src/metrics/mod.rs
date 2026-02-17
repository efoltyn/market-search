use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchResult {
    pub name: String,
    pub duration_ms: u128,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl BenchResult {
    pub fn ok(name: impl Into<String>, duration: Duration) -> Self {
        Self {
            name: name.into(),
            duration_ms: duration.as_millis(),
            ok: true,
            note: None,
        }
    }

    pub fn err(name: impl Into<String>, duration: Duration, note: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            duration_ms: duration.as_millis(),
            ok: false,
            note: Some(note.into()),
        }
    }
}
