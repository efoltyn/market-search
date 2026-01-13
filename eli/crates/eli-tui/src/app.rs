use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use eli_core::agent::AgentEvent;
use eli_core::config::{ApprovalMode, RunMode};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
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
                                if !self.input.trim().is_empty() && !self.is_processing {
                                    let msg = self.input.drain(..).collect::<String>();
                                    self.cursor_pos = 0;
                                    // Rough token estimate: ~4 chars per token
                                    self.last_input_tokens = (msg.len() / 4).max(1);
                                    self.total_tokens += self.last_input_tokens;
                                    self.messages.push(UIMessage {
                                        role: "You".to_string(),
                                        content: msg.clone(),
                                    });
                                    self.is_processing = true;
                                    self.processing_start = Some(std::time::Instant::now());
                                    let _ = self.action_tx.send(Action::UserInput(msg)).await;
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
                    AgentEvent::MessageComplete(_) => {
                        self.is_processing = false;
                        self.processing_start = None;
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
                    }
                    AgentEvent::Error(e) => {
                        self.messages.push(UIMessage {
                            role: "Error".to_string(),
                            content: e,
                        });
                        self.is_processing = false;
                        self.processing_start = None;
                    }
                    AgentEvent::Done => {
                        self.is_processing = false;
                        self.processing_start = None;
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
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),  // Header (smaller)
                Constraint::Min(5),     // Body
                Constraint::Length(3),  // Input box
                Constraint::Length(1),  // Status footer
            ])
            .split(area);

        self.render_header(f, chunks[0]);

        let body = chunks[1];
        if body.width >= 100 {
            let body_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(75), Constraint::Percentage(25)])
                .split(body);
            self.render_chat(f, body_chunks[0]);
            self.render_sidebar(f, body_chunks[1]);
        } else {
            // Narrow terminal - stack vertically
            self.render_chat(f, body);
        }

        self.render_input(f, chunks[2]);
        self.render_footer(f, chunks[3]);
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let header_line = Line::from(vec![
            Span::styled(
                "eli",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}/{}", self.ui.provider, self.ui.model),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(" · ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("mode:{}", format_mode(self.ui.mode)),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let header = Paragraph::new(header_line);
        f.render_widget(header, area);
    }

    fn render_chat(&self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();

        for msg in &self.messages {
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
                }
                "Eli" => {
                    // AI messages: • prefix for main content, - for sub-items
                    for (i, content_line) in msg.content.lines().enumerate() {
                        let trimmed = content_line.trim();
                        if trimmed.is_empty() {
                            lines.push(Line::from(""));
                            continue;
                        }

                        // Check if it's a sub-item (starts with - or *)
                        let is_subitem = trimmed.starts_with('-') || trimmed.starts_with('*');

                        if is_subitem {
                            // Sub-item: indent with -
                            let content = trimmed.trim_start_matches('-').trim_start_matches('*').trim();
                            lines.push(Line::from(vec![
                                Span::styled("  - ", Style::default().fg(Color::DarkGray)),
                                Span::styled(content, Style::default().fg(Color::Gray)),
                            ]));
                        } else if i == 0 {
                            // First line: • bullet
                            lines.push(Line::from(vec![
                                Span::styled("• ", Style::default().fg(Color::Cyan)),
                                Span::styled(content_line, Style::default().fg(Color::White)),
                            ]));
                        } else {
                            // Continuation lines: indent to align
                            lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(content_line, Style::default().fg(Color::White)),
                            ]));
                        }
                    }
                }
                "Error" => {
                    lines.push(Line::from(vec![
                        Span::styled("✗ ", Style::default().fg(Color::Red)),
                        Span::styled(
                            msg.content.clone(),
                            Style::default().fg(Color::Red),
                        ),
                    ]));
                }
                "System" => {
                    lines.push(Line::from(vec![
                        Span::styled("· ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            msg.content.clone(),
                            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
                _ => {
                    lines.push(Line::from(Span::styled(
                        msg.content.clone(),
                        Style::default().fg(Color::Gray),
                    )));
                }
            }
            lines.push(Line::from("")); // Spacing between messages
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

    fn render_sidebar(&self, f: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        lines.push(Line::from(Span::styled(
            "Status",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!(
            "state: {}",
            if self.is_processing { "thinking" } else { "idle" }
        )));
        lines.push(Line::from(format!(
            "mode: {}",
            format_mode(self.ui.mode)
        )));
        lines.push(Line::from(format!(
            "approvals: {}",
            format_approvals(self.ui.approvals)
        )));
        lines.push(Line::from(format!("auto: {}", if self.ui.auto { "on" } else { "off" })));
        lines.push(Line::from(format!(
            "parallel: cmd {} / sub {}",
            self.ui.parallel_commands, self.ui.parallel_subagents
        )));
        lines.push(Line::from(""));

        if let Some(plan) = &self.plan {
            lines.push(Line::from(Span::styled(
                "Plan",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));
            if !plan.line1.is_empty() {
                lines.push(Line::from(plan.line1.clone()));
            }
            if !plan.line2.is_empty() {
                lines.push(Line::from(plan.line2.clone()));
            }
            if !plan.focus.is_empty() {
                lines.push(Line::from(format!("focus: {}", plan.focus)));
            }
            lines.push(Line::from(format!("status: {}", plan.status)));
            lines.push(Line::from(""));
        }

        if !self.tool_log.is_empty() {
            lines.push(Line::from(Span::styled(
                "Tools",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));
            for item in &self.tool_log {
                lines.push(Line::from(item.clone()));
            }
        }

        let sidebar = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Status"))
            .wrap(Wrap { trim: true });
        f.render_widget(sidebar, area);
    }

    fn render_input(&self, f: &mut Frame, area: Rect) {
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

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let mut parts: Vec<Span> = Vec::new();

        // Show working status with time if processing
        if self.is_processing {
            let elapsed = self.processing_start
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            parts.push(Span::styled(
                format!("• Working ({}s · esc to interrupt)", elapsed),
                Style::default().fg(Color::Yellow),
            ));
        } else {
            parts.push(Span::styled(
                "• Ready",
                Style::default().fg(Color::Green),
            ));
        }

        parts.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));

        // Token count display
        parts.push(Span::styled(
            format!("~{} tokens", self.total_tokens),
            Style::default().fg(Color::DarkGray),
        ));

        if self.last_input_tokens > 0 {
            parts.push(Span::styled(
                format!(" (+{})", self.last_input_tokens),
                Style::default().fg(Color::DarkGray),
            ));
        }

        parts.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        parts.push(Span::styled(
            "ctrl-c quit · ? help",
            Style::default().fg(Color::DarkGray),
        ));

        let footer = Paragraph::new(Line::from(parts));
        f.render_widget(footer, area);
    }
}

fn format_mode(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Read => "read",
        RunMode::Work => "work",
    }
}

fn format_approvals(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
    }
}
