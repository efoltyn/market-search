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
            let stream = stream::iter(to_run)
                .map(|(idx, cmd)| async move { (idx, self.run_command(cmd).await) });
            let mut results: Vec<(usize, CommandResult)> =
                stream.buffer_unordered(parallelism).collect().await;
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

        let rewritten = rewrite_eli_invocation(command);
        let safe_command = quote_heredoc_delimiters(&rewritten);
        let mut cmd = shell_command(&safe_command);
        cmd.current_dir(&self.cwd);

        let output = tokio::time::timeout(timeout, cmd.output()).await;

        let duration_ms = start.elapsed().as_millis();
        match output {
            Ok(Ok(output)) => CommandResult {
                command: safe_command,
                returncode: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                duration_ms,
                allowed: true,
                deny_reason: None,
            },
            Ok(Err(e)) => CommandResult {
                command: safe_command,
                returncode: -1,
                stdout: String::new(),
                stderr: format!("error running command: {e}"),
                duration_ms,
                allowed: true,
                deny_reason: None,
            },
            Err(_) => CommandResult {
                command: safe_command,
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

fn rewrite_eli_invocation(command: &str) -> String {
    let trimmed = command.trim_start();
    if !trimmed.starts_with("eli") {
        return command.to_string();
    }

    let Some(after) = trimmed.strip_prefix("eli") else {
        return command.to_string();
    };
    if !(after.is_empty() || after.starts_with(char::is_whitespace)) {
        return command.to_string();
    }

    let normalized_after = normalize_legacy_eli_subcommands(after);

    let Ok(exe) = std::env::current_exe() else {
        return command.to_string();
    };
    let exe = exe.to_string_lossy().replace('\'', "'\\''");
    let prefix_len = command.len().saturating_sub(trimmed.len());
    let prefix = &command[..prefix_len];
    format!("{prefix}'{exe}'{normalized_after}")
}

fn normalize_legacy_eli_subcommands(after: &str) -> String {
    let trimmed = after.trim_start();
    let ws_len = after.len().saturating_sub(trimmed.len());
    let ws = &after[..ws_len];

    if trimmed == "odds"
        || trimmed.starts_with("odds ")
        || trimmed == "sync"
        || trimmed.starts_with("sync ")
    {
        return format!("{ws}finance {trimmed}");
    }

    after.to_string()
}

fn quote_heredoc_delimiters(command: &str) -> String {
    let mut out_lines: Vec<String> = Vec::new();
    for line in command.lines() {
        out_lines.push(quote_heredoc_delimiter_in_line(line));
    }
    out_lines.join("\n")
}

fn quote_heredoc_delimiter_in_line(line: &str) -> String {
    let heredoc_pos = match line.find("<<") {
        Some(pos) => pos,
        None => return line.to_string(),
    };

    // Avoid rewriting lines that are unlikely to be heredoc script blocks (e.g. bitshift ops).
    // This is a heuristic guardrail, not a full shell parser.
    let before = line[..heredoc_pos].to_ascii_lowercase();
    let looks_like_script = ["cat", "python", "python3", "node", "nodejs", "bash", "sh"]
        .iter()
        .any(|w| before.contains(w));
    if !looks_like_script {
        return line.to_string();
    }

    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' && i + 1 < bytes.len() && bytes[i + 1] == b'<' {
            // Ignore bash here-strings (`<<<`).
            if i + 2 < bytes.len() && bytes[i + 2] == b'<' {
                out.push_str("<<<");
                i += 3;
                continue;
            }

            out.push_str("<<");
            i += 2;

            // Preserve `<<-` (strip leading tabs in body).
            if i < bytes.len() && bytes[i] == b'-' {
                out.push('-');
                i += 1;
            }

            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                out.push(bytes[i] as char);
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }

            let start = i;
            let first = bytes[i];
            if first == b'\'' || first == b'"' {
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    out.push(bytes[i] as char);
                    i += 1;
                }
                continue;
            }

            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let delim = &line[start..i];
            if delim.chars().any(|c| c.is_ascii_alphabetic()) {
                out.push('\'');
                out.push_str(delim);
                out.push('\'');
            } else {
                out.push_str(delim);
            }
            continue;
        }

        out.push(bytes[i] as char);
        i += 1;
    }

    out
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
