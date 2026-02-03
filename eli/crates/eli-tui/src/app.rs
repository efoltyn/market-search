use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use eli_core::agent::AgentEvent;
use eli_core::contract;
use eli_core::config::{ApprovalMode, RunMode};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::time::Duration;
use tokio::sync::mpsc;

pub enum Action {
    UserInput(String),
    Quit,
}

pub struct UiConfig {
    pub provider: String,
    pub model: String,
    pub mode: RunMode,
    pub approvals: ApprovalMode,
    pub auto: bool,
    pub mem_max: usize,
    pub compact: bool,
    pub parallel_commands: u32,
    pub parallel_subagents: u32,
}

pub struct App {
    pub input: String,
    pub cursor_pos: usize,
    pub messages: Vec<UIMessage>,
    pub is_processing: bool,
    pub processing_start: Option<std::time::Instant>,
    pub action_tx: mpsc::Sender<Action>,
    pub event_rx: mpsc::Receiver<AgentEvent>,
    pub should_quit: bool,
    pub ui: UiConfig,
    pub plan: Option<PlanState>,
    pub tool_log: Vec<String>,
    pub last_input_tokens: usize,
    pub total_tokens: usize,
    pub queued_inputs: Vec<String>,
    pub sources: Vec<String>,
    pub last_tool_ok: Option<bool>,
    pub last_run_secs: u64,
}

pub struct UIMessage {
    pub role: String,
    pub content: String,
}

pub struct PlanState {
    pub line1: String,
    pub line2: String,
    pub focus: String,
    pub status: String,
}

impl App {
    pub fn new(
        action_tx: mpsc::Sender<Action>,
        event_rx: mpsc::Receiver<AgentEvent>,
        ui: UiConfig,
    ) -> Self {
        Self {
            input: String::new(),
            cursor_pos: 0,
            messages: vec![UIMessage {
                role: "System".to_string(),
                content: "Welcome to ELI. Ask in natural language or run /help.".to_string(),
            }],
            is_processing: false,
            processing_start: None,
            action_tx,
            event_rx,
            should_quit: false,
            ui,
            plan: None,
            tool_log: Vec::new(),
            last_input_tokens: 0,
            total_tokens: 0,
            queued_inputs: Vec::new(),
            sources: Vec::new(),
            last_tool_ok: None,
            last_run_secs: 0,
        }
    }

    pub async fn run(
        mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| self.ui(f))?;

            // Event loop with timeout to allow UI updates from agent events
            if crossterm::event::poll(Duration::from_millis(10))? {
                if let Event::Key(key) = crossterm::event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let _ = self.action_tx.send(Action::Quit).await;
                                self.should_quit = true;
                            }
                            KeyCode::Enter => {
                                if !self.input.trim().is_empty() {
                                    let msg = self.input.drain(..).collect::<String>();
                                    self.cursor_pos = 0;
                                    // Rough token estimate: ~4 chars per token
                                    self.last_input_tokens = (msg.len() / 4).max(1);
                                    self.total_tokens += self.last_input_tokens;
                                    self.messages.push(UIMessage {
                                        role: "You".to_string(),
                                        content: msg.clone(),
                                    });
                                    if self.is_processing {
                                        self.queued_inputs.push(msg);
                                    } else {
                                        self.sources.clear();
                                        self.last_tool_ok = None;
                                        self.is_processing = true;
                                        self.processing_start = Some(std::time::Instant::now());
                                        let _ = self.action_tx.send(Action::UserInput(msg)).await;
                                    }
                                }
                            }
                            KeyCode::Char(c) => {
                                self.input.insert(self.cursor_pos, c);
                                self.cursor_pos += 1;
                            }
                            KeyCode::Backspace => {
                                if self.cursor_pos > 0 {
                                    self.cursor_pos -= 1;
                                    self.input.remove(self.cursor_pos);
                                }
                            }
                            KeyCode::Delete => {
                                if self.cursor_pos < self.input.len() {
                                    self.input.remove(self.cursor_pos);
                                }
                            }
                            KeyCode::Left => {
                                if self.cursor_pos > 0 {
                                    self.cursor_pos -= 1;
                                }
                            }
                            KeyCode::Right => {
                                if self.cursor_pos < self.input.len() {
                                    self.cursor_pos += 1;
                                }
                            }
                            KeyCode::Home => {
                                self.cursor_pos = 0;
                            }
                            KeyCode::End => {
                                self.cursor_pos = self.input.len();
                            }
                            KeyCode::Esc => {
                                if self.is_processing {
                                    // Could add interrupt logic here
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Process incoming agent events
            while let Ok(event) = self.event_rx.try_recv() {
                match event {
                    AgentEvent::Token(s) => {
                        if let Some(last) = self.messages.last_mut() {
                            if last.role == "Eli" {
                                last.content.push_str(&s);
                            } else {
                                self.messages.push(UIMessage {
                                    role: "Eli".to_string(),
                                    content: s,
                                });
                            }
                        } else {
                            self.messages.push(UIMessage {
                                role: "Eli".to_string(),
                                content: s,
                            });
                        }
                    }
                    AgentEvent::MessageComplete(raw) => {
                        if let Some(parsed) = render_model_response(&raw) {
                            if let Some(last) = self.messages.last_mut() {
                                if last.role == "Eli" {
                                    last.content = parsed;
                                }
                            }
                        }
                        if let Some(started) = self.processing_start {
                            self.last_run_secs = started.elapsed().as_secs();
                        }
                        self.is_processing = false;
                        self.processing_start = None;
                        if let Some(next) = self.queued_inputs.first().cloned() {
                            self.queued_inputs.remove(0);
                            self.sources.clear();
                            self.last_tool_ok = None;
                            self.is_processing = true;
                            self.processing_start = Some(std::time::Instant::now());
                            let _ = self.action_tx.send(Action::UserInput(next)).await;
                        }
                    }
                    AgentEvent::Plan { plan, focus, status } => {
                        let mut iter = plan.lines();
                        let line1 = iter.next().unwrap_or("").trim().to_string();
                        let line2 = iter.next().unwrap_or("").trim().to_string();
                        let status = match status {
                            eli_core::contract::StepStatus::KeepWorking => "keep_working",
                            eli_core::contract::StepStatus::Done => "done",
                        }
                        .to_string();
                        self.plan = Some(PlanState {
                            line1,
                            line2,
                            focus: focus.trim().to_string(),
                            status,
                        });
                    }
                    AgentEvent::ToolOutput { name, output } => {
                        self.tool_log.push(format!(
                            "{name}: {output}",
                            name = name,
                            output = output
                        ));
                        if self.tool_log.len() > 6 {
                            let drop = self.tool_log.len() - 6;
                            self.tool_log.drain(0..drop);
                        }
                        for src in infer_sources(&name, &output) {
                            self.add_source(&src);
                        }
                        if let Some(ok) = parse_tool_ok(&output) {
                            self.last_tool_ok = Some(ok);
                        }
                    }
                    AgentEvent::Error(e) => {
                        self.messages.push(UIMessage {
                            role: "Error".to_string(),
                            content: e,
                        });
                        if let Some(started) = self.processing_start {
                            self.last_run_secs = started.elapsed().as_secs();
                        }
                        self.is_processing = false;
                        self.processing_start = None;
                    }
                    AgentEvent::Done => {
                        if let Some(started) = self.processing_start {
                            self.last_run_secs = started.elapsed().as_secs();
                        }
                        self.is_processing = false;
                        self.processing_start = None;
                        if let Some(next) = self.queued_inputs.first().cloned() {
                            self.queued_inputs.remove(0);
                            self.sources.clear();
                            self.last_tool_ok = None;
                            self.is_processing = true;
                            self.processing_start = Some(std::time::Instant::now());
                            let _ = self.action_tx.send(Action::UserInput(next)).await;
                        }
                    }
                }
            }

            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    fn ui(&self, f: &mut Frame) {
        let area = f.size();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),     // Body
                Constraint::Length(3),  // Input box
                Constraint::Length(1),  // Status line
                Constraint::Length(1),  // Sources footer
            ])
            .split(area);

        let body = chunks[0];
        self.render_chat(f, body);
        self.render_input(f, chunks[1]);
        self.render_status(f, chunks[2]);
        self.render_sources(f, chunks[3]);
    }

    fn add_source(&mut self, source: &str) {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return;
        }
        if !self.sources.iter().any(|s| s.eq_ignore_ascii_case(trimmed)) {
            self.sources.push(trimmed.to_string());
        }
    }

    fn render_chat(&self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();

        for msg in &self.messages {
            let mut rendered = false;
            match msg.role.as_str() {
                "You" => {
                    // User messages: › prefix, bold
                    lines.push(Line::from(vec![
                        Span::styled("› ", Style::default().fg(Color::White)),
                        Span::styled(
                            msg.content.clone(),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    rendered = true;
                }
                "Eli" => {
                    for (i, content_line) in msg.content.lines().enumerate() {
                        let trimmed = content_line.trim();
                        if trimmed.is_empty() {
                            lines.push(Line::from(""));
                            continue;
                        }

                        if i == 0 {
                            lines.push(Line::from(vec![
                                Span::styled("• ", Style::default().fg(Color::Cyan)),
                                Span::styled(content_line, Style::default().fg(Color::White)),
                            ]));
                        } else {
                            lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(content_line, Style::default().fg(Color::White)),
                            ]));
                        }
                    }
                    rendered = true;
                }
                _ => {
                    // Only render user + AI in the main view.
                }
            }
            if rendered {
                lines.push(Line::from("")); // Spacing between messages
            }
        }

        // Auto-scroll: calculate offset to show latest messages
        let visible_height = area.height.saturating_sub(0) as usize;
        let total_lines = lines.len();
        let scroll_offset = if total_lines > visible_height {
            (total_lines - visible_height) as u16
        } else {
            0
        };

        let chat = Paragraph::new(lines)
            .scroll((scroll_offset, 0))
            .wrap(Wrap { trim: false });
        f.render_widget(chat, area);
    }

    fn render_input(&self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        // Build input display with cursor
        let (before_cursor, after_cursor) = self.input.split_at(self.cursor_pos.min(self.input.len()));

        let input_spans = if self.is_processing {
            // During processing: show dimmed input with no cursor
            vec![
                Span::styled("› ", Style::default().fg(Color::DarkGray)),
                Span::styled(self.input.clone(), Style::default().fg(Color::DarkGray)),
            ]
        } else {
            // Active: show input with cursor block
            let cursor_char = after_cursor.chars().next().unwrap_or(' ');
            let rest = if after_cursor.len() > 1 {
                &after_cursor[cursor_char.len_utf8()..]
            } else {
                ""
            };

            vec![
                Span::styled("› ", Style::default().fg(Color::Cyan)),
                Span::styled(before_cursor, Style::default().fg(Color::White)),
                Span::styled(
                    cursor_char.to_string(),
                    Style::default().fg(Color::Black).bg(Color::White),
                ),
                Span::styled(rest, Style::default().fg(Color::White)),
            ]
        };

        let input_line = Line::from(input_spans);
        let input = Paragraph::new(input_line)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if self.is_processing {
                        Color::DarkGray
                    } else {
                        Color::Cyan
                    }))
            );
        f.render_widget(input, area);
    }

    fn render_sources(&self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        let mut spans: Vec<Span> = Vec::new();
        if let Some(ok) = self.last_tool_ok {
            let (symbol, color) = if ok { ("✓", Color::Green) } else { ("✗", Color::Red) };
            spans.push(Span::styled(
                format!("{symbol} "),
                Style::default().fg(color),
            ));
        }

        spans.push(Span::styled(
            "Sources: ",
            Style::default().fg(Color::DarkGray),
        ));

        if self.sources.is_empty() {
            spans.push(Span::styled("—", Style::default().fg(Color::DarkGray)));
        } else {
            spans.push(Span::styled(
                self.sources.join(", "),
                Style::default().fg(Color::Gray),
            ));
        }

        let footer = Paragraph::new(Line::from(spans));
        f.render_widget(footer, area);
    }

    fn render_status(&self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        if self.is_processing {
            return;
        }

        let mode_chip = if self.ui.auto {
            "[AUTO]"
        } else if matches!(self.ui.approvals, ApprovalMode::Ask) {
            "[ASK]"
        } else {
            "[PLAN]"
        };

        let line = format!(
            "ready {mode_chip} [{}s] t:{} ────",
            self.last_run_secs, self.total_tokens
        );
        let status = Paragraph::new(Line::from(vec![Span::styled(
            line,
            Style::default().fg(Color::DarkGray),
        )]));
        f.render_widget(status, area);
    }
}

fn render_model_response(raw: &str) -> Option<String> {
    let model = match contract::validate_model_response(raw) {
        Ok(model) => model,
        Err(_) => return None,
    };

    if let Some(synthesis) = &model.synthesis {
        if !synthesis.answer.trim().is_empty() {
            return Some(synthesis.answer.trim().to_string());
        }
    }

    if !model.notes.trim().is_empty() {
        return Some(model.notes.trim().to_string());
    }

    if let Some(ask) = &model.ask_user {
        if !ask.trim().is_empty() {
            return Some(ask.trim().to_string());
        }
    }

    Some(String::new())
}

fn infer_sources(name: &str, output: &str) -> Vec<String> {
    let mut sources = Vec::new();
    if name == "shell" {
        if let Some(cmd) = output.split("Cmd: ").nth(1) {
            let cmd = cmd.split(" (code=").next().unwrap_or(cmd).trim();
            let cmd_lower = cmd.to_ascii_lowercase();

            if cmd_lower.contains("eli finance") {
                if cmd_lower.contains("--provider fred") {
                    sources.push("FRED".to_string());
                } else if cmd_lower.contains("--provider mock") {
                    sources.push("Mock".to_string());
                } else {
                    sources.push("Yahoo Finance".to_string());
                }
            }

            if cmd_lower.contains("python") {
                sources.push("Python".to_string());
            } else if cmd_lower.contains("node") || cmd_lower.contains("npm") {
                sources.push("Node".to_string());
            }
        }
    }

    sources
}

fn parse_tool_ok(output: &str) -> Option<bool> {
    if let Some(code_part) = output.split("code=").nth(1) {
        let code_str = code_part
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        if let Ok(code) = code_str.parse::<i32>() {
            return Some(code == 0);
        }
    }

    let lower = output.to_ascii_lowercase();
    if lower.contains("error:") || lower.contains("failed") {
        return Some(false);
    }

    None
}
