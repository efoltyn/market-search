#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;
use tokio::process::Command;
use tracing::debug;

#[derive(Debug, Error)]
pub enum ScreenError {
    #[error("unsupported on this platform: {0}")]
    Unsupported(&'static str),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("command failed (code={code:?}): {stderr}")]
    CommandFailed { code: Option<i32>, stderr: String },
}

pub type Result<T> = std::result::Result<T, ScreenError>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ScreenAction {
    Clipboard { text: String },
    FocusApp { name: String },
}

pub async fn run_action(action: ScreenAction) -> Result<()> {
    match action {
        ScreenAction::Clipboard { text } => clipboard_set(&text).await,
        ScreenAction::FocusApp { name } => focus_app(&name).await,
    }
}

pub async fn focus_app(name: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            r#"tell application "{name}" to activate"#,
            name = name.replace('"', "\\\"")
        );
        run_osascript(&script).await?;
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = name;
        Err(ScreenError::Unsupported("focus_app"))
    }
}

pub async fn clipboard_set(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut child = Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(text.as_bytes()).await?;
        }
        let status = child.wait().await?;
        if !status.success() {
            return Err(ScreenError::CommandFailed {
                code: status.code(),
                stderr: "pbcopy failed".to_string(),
            });
        }
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        Err(ScreenError::Unsupported("clipboard_set"))
    }
}

pub async fn screenshot_to(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        debug!(to = %path.display(), "capturing screenshot");
        let output = Command::new("screencapture")
            .arg("-x")
            .arg(path)
            .output()
            .await?;
        if !output.status.success() {
            return Err(ScreenError::CommandFailed {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Err(ScreenError::Unsupported("screenshot_to"))
    }
}

async fn run_osascript(script: &str) -> Result<String> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await?;
    if !output.status.success() {
        return Err(ScreenError::CommandFailed {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
