use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;
use tokio::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub command: String,
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,

    #[serde(default)]
    pub allowed: bool,
    #[serde(default)]
    pub deny_reason: Option<String>,
}

pub struct CommandRunner {
    timeout_secs: u64,
    max_commands: Option<usize>,
    cwd: PathBuf,
    parallelism: usize,
}

impl CommandRunner {
    pub fn new(timeout_secs: u64, max_cmds: u32, parallel_commands: u32, cwd: PathBuf) -> Self {
        Self {
            timeout_secs: timeout_secs.max(1),
            max_commands: if max_cmds == 0 {
                None
            } else {
                Some(max_cmds as usize)
            },
            cwd,
            parallelism: parallel_commands.max(1) as usize,
        }
    }

    pub async fn run_commands(&self, commands: &[String]) -> Vec<CommandResult> {
        self.run_commands_with_parallelism(commands, self.parallelism)
            .await
    }

    pub async fn run_commands_with_parallelism(
        &self,
        commands: &[String],
        parallelism: usize,
    ) -> Vec<CommandResult> {
        let (to_run, truncated) = self.truncate_commands(commands);
        if to_run.is_empty() {
            return truncated.into_iter().collect();
        }

        let mut out = if parallelism <= 1 || to_run.len() <= 1 {
            let mut results = Vec::with_capacity(to_run.len());
            for (idx, cmd) in to_run {
                results.push((idx, self.run_command(cmd).await));
            }
            results
        } else {
            let stream = stream::iter(to_run).map(|(idx, cmd)| async move {
                (idx, self.run_command(cmd).await)
            });
            let mut results: Vec<(usize, CommandResult)> = stream
                .buffer_unordered(parallelism)
                .collect()
                .await;
            results.sort_by_key(|(idx, _)| *idx);
            results
        };

        out.sort_by_key(|(idx, _)| *idx);
        let mut results: Vec<CommandResult> = out.into_iter().map(|(_, r)| r).collect();
        if let Some(truncated) = truncated {
            results.push(truncated);
        }
        results
    }

    pub async fn run_command(&self, command: &str) -> CommandResult {
        let start = Instant::now();
        let timeout = std::time::Duration::from_secs(self.timeout_secs);

        let mut cmd = shell_command(command);
        cmd.current_dir(&self.cwd);

        let output = tokio::time::timeout(timeout, cmd.output()).await;

        let duration_ms = start.elapsed().as_millis();
        match output {
            Ok(Ok(output)) => CommandResult {
                command: command.to_string(),
                returncode: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                duration_ms,
                allowed: true,
                deny_reason: None,
            },
            Ok(Err(e)) => CommandResult {
                command: command.to_string(),
                returncode: -1,
                stdout: String::new(),
                stderr: format!("error running command: {e}"),
                duration_ms,
                allowed: true,
                deny_reason: None,
            },
            Err(_) => CommandResult {
                command: command.to_string(),
                returncode: -1,
                stdout: String::new(),
                stderr: format!("command timed out after {}s", self.timeout_secs),
                duration_ms,
                allowed: true,
                deny_reason: None,
            },
        }
    }

    fn truncate_commands<'a>(
        &self,
        commands: &'a [String],
    ) -> (Vec<(usize, &'a str)>, Option<CommandResult>) {
        if let Some(max) = self.max_commands {
            if commands.len() > max {
                let to_run = commands
                    .iter()
                    .take(max)
                    .enumerate()
                    .map(|(idx, cmd)| (idx, cmd.as_str()))
                    .collect();
                let truncated = CommandResult {
                    command: "[truncated]".to_string(),
                    returncode: -1,
                    stdout: String::new(),
                    stderr: format!("command list truncated at {max} commands"),
                    duration_ms: 0,
                    allowed: false,
                    deny_reason: Some(format!("exceeded max_cmds limit ({max})")),
                };
                return (to_run, Some(truncated));
            }
        }

        let to_run = commands
            .iter()
            .enumerate()
            .map(|(idx, cmd)| (idx, cmd.as_str()))
            .collect();
        (to_run, None)
    }
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-lc").arg(command);
    cmd
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    cmd
}
