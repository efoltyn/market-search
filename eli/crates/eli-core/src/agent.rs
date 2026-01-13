use crate::adapter::LlmAdapter;
use crate::config::{ApprovalMode, ChatConfig, RunMode};
use crate::contract::{self, StepStatus};
use crate::diff::engine::{DiffEngine, DiffResult};
use crate::executor::command_runner::{CommandResult, CommandRunner};
use crate::memory::Memory;
use crate::orchestrator::{maybe_compact_memory, run_subagents, SubagentResult};
use crate::persistence::{EventKind, SessionEvent, SessionStore};
use crate::types::{ChatMessage, ChatRequest, ChatStreamEvent};
use std::sync::Arc;
use tokio::sync::mpsc;
use futures::StreamExt;

pub enum AgentEvent {
    Token(String),
    MessageComplete(String),
    Plan { plan: String, focus: String, status: StepStatus },
    ToolOutput { name: String, output: String },
    Error(String),
    Done,
}

pub struct Agent {
    cfg: ChatConfig,
    adapter: Arc<dyn LlmAdapter>,
    diff_engine: DiffEngine,
    command_runner: CommandRunner,
    store: SessionStore,
    session_id: String,
    memory: Memory,
}

impl Agent {
    pub fn new(
        cfg: ChatConfig,
        adapter: Arc<dyn LlmAdapter>,
        diff_engine: DiffEngine,
        command_runner: CommandRunner,
        store: SessionStore,
        session_id: String,
    ) -> Self {
        let memory = Memory::new(cfg.mem_steps);
        Self {
            cfg,
            adapter,
            diff_engine,
            command_runner,
            store,
            session_id,
            memory,
        }
    }

    pub fn set_system_prompt(&mut self, prompt: String) {
        self.memory.set_system(prompt);
    }

    pub async fn chat(&mut self, user_message: String, tx: mpsc::Sender<AgentEvent>) -> anyhow::Result<()> {
        self.memory.push(ChatMessage::user(user_message.clone()));
        let _ = self.store.append(&self.session_id, &SessionEvent {
            ts: chrono::Utc::now(),
            kind: EventKind::UserMessage { content: user_message.clone() },
        }).await;

        let max_iters = if self.cfg.auto { self.cfg.max_auto.max(1) } else { 1 };
        let mut current_iter = 0;

        loop {
            current_iter += 1;
            if current_iter > max_iters {
                break;
            }

            if let Ok(Some(compaction)) =
                maybe_compact_memory(self.adapter.clone(), &self.cfg, &mut self.memory).await
            {
                let note = format!(
                    "memory_compaction: dropped {} messages\n{}",
                    compaction.dropped, compaction.summary
                );
                let _ = self
                    .store
                    .append(
                        &self.session_id,
                        &SessionEvent {
                            ts: chrono::Utc::now(),
                            kind: EventKind::Note { content: note },
                        },
                    )
                    .await;
                let _ = tx
                    .send(AgentEvent::ToolOutput {
                        name: "memory".to_string(),
                        output: format!("compacted ({} msgs)", compaction.dropped),
                    })
                    .await;
            }

            let req = ChatRequest {
                model: self.cfg.model.clone(),
                messages: self.memory.context(),
                temperature: self.cfg.temperature,
                max_tokens: self.cfg.max_tokens,
                stream: true,
            };

            let mut full_response = String::new();
            match self.adapter.chat_stream(req).await {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(ChatStreamEvent::Delta(s)) => {
                                full_response.push_str(&s);
                                let _ = tx.send(AgentEvent::Token(s)).await;
                            }
                            Ok(ChatStreamEvent::Usage(_)) => {}
                            Ok(ChatStreamEvent::Done) => break,
                            Err(e) => {
                                let _ = tx.send(AgentEvent::Error(e.to_string())).await;
                                return Ok(());
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(AgentEvent::Error(e.to_string())).await;
                    return Ok(());
                }
            }

            if full_response.trim().is_empty() {
                break;
            }

            self.memory.push(ChatMessage::assistant(full_response.clone()));
            let _ = self.store.append(&self.session_id, &SessionEvent {
                ts: chrono::Utc::now(),
                kind: EventKind::AssistantMessage { content: full_response.clone() },
            }).await;
            
            let _ = tx.send(AgentEvent::MessageComplete(full_response.clone())).await;

            // Parse and Execute
            let model = match contract::validate_model_response(&full_response) {
                Ok(m) => m,
                Err(_) => break, // If parsing fails, we stop for now
            };

            let _ = tx
                .send(AgentEvent::Plan {
                    plan: model.plan.clone(),
                    focus: model.focus.clone(),
                    status: model.status,
                })
                .await;

            let (plan_mode, _) = self.parse_plan_controls(&model.plan);
            let read_mode = matches!(self.cfg.mode, RunMode::Read) || matches!(plan_mode, Some(RunMode::Read));
            let wants_user_input = model
                .ask_user
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            
            // In TUI, we default to AUTO for now as we don't have interactive confirmation implemented yet
            let apply = !read_mode; 
            let dry_run = !apply;

            let mut diff_results = Vec::new();
            let mut command_results = Vec::new();
            if !wants_user_input {
                if !model.diffs.is_empty() {
                    diff_results = self.diff_engine.apply_diffs(&model.diffs, dry_run);
                    for r in &diff_results {
                        let msg = format!("Diff: {} {}", r.op, r.path);
                        let _ = tx
                            .send(AgentEvent::ToolOutput {
                                name: "diff".to_string(),
                                output: msg,
                            })
                            .await;
                    }
                }

                if !model.commands.is_empty() {
                    if apply {
                        let parallelism = if model.commands_parallel {
                            self.cfg.resolved_parallel_commands()
                        } else {
                            1
                        };
                        command_results = self
                            .command_runner
                            .run_commands_with_parallelism(&model.commands, parallelism)
                            .await;
                    } else {
                        // simulate skipped
                    }
                    for r in &command_results {
                        let msg = format!("Cmd: {} (code={})", r.command, r.returncode);
                        let _ = tx
                            .send(AgentEvent::ToolOutput {
                                name: "shell".to_string(),
                                output: msg,
                            })
                            .await;
                    }
                }

                if !diff_results.is_empty() || !command_results.is_empty() {
                    let observation =
                        self.build_observation(read_mode, false, &diff_results, &command_results);
                    self.memory.push(ChatMessage::tool(observation.clone(), "eli"));
                    let _ = self
                        .store
                        .append(
                            &self.session_id,
                            &SessionEvent {
                                ts: chrono::Utc::now(),
                                kind: EventKind::Note { content: observation },
                            },
                        )
                        .await;
                }

                if !model.subagents.is_empty() {
                    let results = run_subagents(
                        self.adapter.clone(),
                        &self.cfg,
                        &self.memory,
                        &model.subagents,
                    )
                    .await;
                    if !results.is_empty() {
                        let observation = build_subagent_observation(&results);
                        self.memory.push(ChatMessage::tool(observation.clone(), "eli.subagents"));
                        let _ = self
                            .store
                            .append(
                                &self.session_id,
                                &SessionEvent {
                                    ts: chrono::Utc::now(),
                                    kind: EventKind::Note { content: observation },
                                },
                            )
                            .await;
                        for result in results {
                            let label = if let Some(err) = result.error {
                                format!("error: {err}")
                            } else if result.output.trim().is_empty() {
                                "(no output)".to_string()
                            } else {
                                truncate_result(&result.output, 200)
                            };
                            let _ = tx
                                .send(AgentEvent::ToolOutput {
                                    name: format!("subagent:{}", result.name),
                                    output: label,
                                })
                                .await;
                        }
                    }
                }
            }

            if matches!(model.status, StepStatus::Done) {
                break;
            }
        }

        let _ = tx.send(AgentEvent::Done).await;
        Ok(())
    }

    fn parse_plan_controls(&self, plan: &str) -> (Option<RunMode>, Option<ApprovalMode>) {
        let line = plan.lines().next().unwrap_or("");
        let mut mode = None;
        let mut approvals = None;
    
        for part in line.split('|').map(|p| p.trim()) {
            let lower = part.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("mode:") {
                let v = rest.trim();
                mode = match v {
                    "read" => Some(RunMode::Read),
                    "work" => Some(RunMode::Work),
                    _ => None,
                };
            } else if let Some(rest) = lower.strip_prefix("approvals:") {
                let v = rest.trim();
                approvals = match v {
                    "ask" => Some(ApprovalMode::Ask),
                    "auto" => Some(ApprovalMode::Auto),
                    _ => None,
                };
            }
        }
    
        (mode, approvals)
    }

    fn build_observation(
        &self,
        read_mode: bool,
        approvals_ask: bool,
        diffs: &[DiffResult],
        commands: &[CommandResult],
    ) -> String {
        let mode = if read_mode { "read" } else { "work" };
        let approvals = if approvals_ask { "ask" } else { "auto" };
    
        let mut out = String::new();
        out.push_str(&format!("mode={}, approvals={}\n", mode, approvals));
    
        if !diffs.is_empty() {
            out.push_str("diffs:\n");
            for r in diffs {
                out.push_str(&format!(
                    "- {op} {path}: {status} {msg}\n",
                    op = r.op,
                    path = r.path,
                    status = if r.success { "OK" } else { "ERR" },
                    msg = r.message
                ));
            }
        }
    
        if !commands.is_empty() {
            out.push_str("commands:\n");
            for r in commands {
                out.push_str(&format!(
                    "- `{cmd}` => {code} ({ms}ms)\n",
                    cmd = r.command,
                    code = r.returncode,
                    ms = r.duration_ms
                ));
                if !r.stdout.trim().is_empty() {
                    out.push_str(&format!("  stdout:\n{}\n", self.truncate(&r.stdout, 400000)));
                }
                if !r.stderr.trim().is_empty() {
                    out.push_str(&format!("  stderr:\n{}\n", self.truncate(&r.stderr, 400000)));
                }
            }
        }
    
        out
    }

    fn truncate(&self, s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut out = String::new();
        for (idx, ch) in s.char_indices() {
            if idx >= max {
                break;
            }
            out.push(ch);
        }
        let remaining = s.len().saturating_sub(out.len());
        format!("{out}...\n[truncated {remaining} bytes]")
    }
}

fn build_subagent_observation(results: &[SubagentResult]) -> String {
    let mut out = String::from("subagents:\n");
    for result in results {
        out.push_str(&format!("- {}\n", result.name));
        if let Some(err) = &result.error {
            out.push_str(&format!("  error: {err}\n"));
            continue;
        }
        if result.output.trim().is_empty() {
            out.push_str("  (no output)\n");
            continue;
        }
        for line in result.output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("  {line}\n", line = line.trim()));
        }
    }
    out
}

fn truncate_result(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in input.char_indices() {
        if idx >= max {
            break;
        }
        out.push(ch);
    }
    let remaining = input.len().saturating_sub(out.len());
    format!("{out}... [truncated {remaining} bytes]")
}
