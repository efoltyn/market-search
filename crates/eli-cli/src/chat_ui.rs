//! Merged TUI+CLI chat interface using alternate screen
//!
//! Combines TUI's beautiful rendering with CLI's full functionality.

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};
use textwrap::{wrap, Options as WrapOptions};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A message in the chat history
#[derive(Clone)]
pub struct ChatMessage {
    pub role: String, // "You", "Eli", "System", "Tool", etc.
    pub content: String,
}

/// Prompt mode for the input
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptMode {
    Ask,
    Plan,
    Auto,
}

/// The merged chat UI state
pub struct ChatUi {
    // Messages and history
    pub messages: Vec<ChatMessage>,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,

    // Input state
    pub input: String,
    pub cursor_pos: usize,

    // UI state
    pub is_processing: bool,
    pub spinner_idx: usize,
    pub last_spinner: Instant,
    pub sources: Vec<String>,
    pub last_tool_ok: Option<bool>,
    pub last_tool_summary: Option<String>,

    // Stats
    pub total_tokens: u32,
    pub elapsed_secs: u64,
    pub request_start: Option<Instant>,
    pub last_request_tokens: u32,
    pub prompt_mode: PromptMode,
    pub queue_len: usize,
    pub show_tips: bool,
    pub show_tool_output: bool,
    pub interrupt_requested: bool,
    last_esc: Option<Instant>,

    // Control
    pub should_quit: bool,

    // Queued prompts (while processing)
    pub queue: Vec<String>,

    // Scrollback
    pub scroll_offset: usize,
    pub follow_tail: bool,
    pub last_total_lines: usize,
    pub last_view_height: usize,
    pub scrollback_max_lines: usize,
}

impl ChatUi {
    pub fn new() -> Self {
        Self {
            messages: vec![ChatMessage {
                role: "System".to_string(),
                content: "Welcome to ELI. Type your question or /help for commands (incl. /model)."
                    .to_string(),
            }],
            history: Vec::new(),
            history_cursor: None,
            input: String::new(),
            cursor_pos: 0,
            is_processing: false,
            spinner_idx: 0,
            last_spinner: Instant::now(),
            sources: Vec::new(),
            last_tool_ok: None,
            last_tool_summary: None,
            total_tokens: 0,
            elapsed_secs: 0,
            request_start: None,
            last_request_tokens: 0,
            prompt_mode: PromptMode::Auto,
            queue_len: 0,
            show_tips: true,
            show_tool_output: false,
            interrupt_requested: false,
            last_esc: None,
            should_quit: false,
            queue: Vec::new(),
            scroll_offset: 0,
            follow_tail: true,
            last_total_lines: 0,
            last_view_height: 0,
            scrollback_max_lines: 10_000,
        }
    }

    /// Add a message to the chat
    pub fn add_message(&mut self, role: &str, content: &str) {
        self.messages.push(ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        });
    }

    /// Remove all inline tool transcript messages.
    pub fn clear_tool_messages(&mut self) {
        self.messages.retain(|m| m.role != "Tool");
    }

    /// Add a source
    pub fn add_source(&mut self, source: &str) {
        let trimmed = source.trim();
        if !trimmed.is_empty() && !self.sources.iter().any(|s| s.eq_ignore_ascii_case(trimmed)) {
            self.sources.push(trimmed.to_string());
        }
    }

    /// Clear sources for new query
    pub fn clear_sources(&mut self) {
        self.sources.clear();
        self.last_tool_ok = None;
        self.last_tool_summary = None;
        self.clear_tool_messages();
    }

    /// Toggle whether full tool stdout/stderr is shown inline.
    pub fn toggle_tool_output(&mut self) -> bool {
        self.show_tool_output = !self.show_tool_output;
        self.show_tool_output
    }

    /// Queue a prompt to run after the current task
    pub fn queue_prompt(&mut self, prompt: String) {
        self.queue.push(prompt);
        self.queue_len = self.queue.len();
    }

    /// Pop the next queued prompt
    pub fn pop_queued(&mut self) -> Option<String> {
        if self.queue.is_empty() {
            None
        } else {
            let next = self.queue.remove(0);
            self.queue_len = self.queue.len();
            Some(next)
        }
    }

    /// Get submitted input and clear buffer
    pub fn submit_input(&mut self) -> Option<String> {
        let trimmed = self.input.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        self.follow_tail = true;
        self.history.push(trimmed.clone());
        self.history_cursor = None;
        self.input.clear();
        self.cursor_pos = 0;
        self.last_esc = None;
        Some(trimmed)
    }

    /// Handle a key event, returns Some(submitted_input) if Enter was pressed
    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        // Ctrl+C = clear input
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            self.history_cursor = None;
            self.input.clear();
            self.cursor_pos = 0;
            self.last_esc = None;
            return None;
        }

        // Ctrl+U = page up, Ctrl+D = page down (or quit if already at bottom)
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('u') {
            let page = self.page_size();
            self.scroll_by(-(page as isize));
            self.last_esc = None;
            return None;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('d') {
            if self.scroll_offset < self.max_scroll_offset() {
                let page = self.page_size();
                self.scroll_by(page as isize);
            } else {
                self.should_quit = true;
            }
            self.last_esc = None;
            return None;
        }

        match code {
            KeyCode::PageUp => {
                let page = self.page_size();
                self.scroll_by(-(page as isize));
                self.last_esc = None;
            }
            KeyCode::PageDown => {
                let page = self.page_size();
                self.scroll_by(page as isize);
                self.last_esc = None;
            }
            KeyCode::Enter => {
                return self.submit_input();
            }
            KeyCode::Char(c) => {
                self.history_cursor = None;
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
                self.last_esc = None;
            }
            KeyCode::Backspace => {
                self.history_cursor = None;
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.input.remove(self.cursor_pos);
                }
                self.last_esc = None;
            }
            KeyCode::Delete => {
                self.history_cursor = None;
                if self.cursor_pos < self.input.len() {
                    self.input.remove(self.cursor_pos);
                }
                self.last_esc = None;
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                }
                self.last_esc = None;
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += 1;
                }
                self.last_esc = None;
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
                self.last_esc = None;
            }
            KeyCode::End => {
                self.cursor_pos = self.input.len();
                self.last_esc = None;
            }
            KeyCode::Up => {
                if let Some(last_idx) = self.history.len().checked_sub(1) {
                    let next = match self.history_cursor {
                        None => Some(last_idx),
                        Some(idx) => idx.checked_sub(1),
                    };
                    if let Some(idx) = next {
                        self.history_cursor = Some(idx);
                        self.input = self.history[idx].clone();
                        self.cursor_pos = self.input.len();
                    }
                }
                self.last_esc = None;
            }
            KeyCode::Down => {
                if let Some(idx) = self.history_cursor {
                    let next = idx.saturating_add(1);
                    if next >= self.history.len() {
                        self.history_cursor = None;
                        self.input.clear();
                        self.cursor_pos = 0;
                    } else {
                        self.history_cursor = Some(next);
                        self.input = self.history[next].clone();
                        self.cursor_pos = self.input.len();
                    }
                }
                self.last_esc = None;
            }
            KeyCode::Esc => {
                if self.is_processing {
                    self.interrupt_requested = true;
                    return None;
                }
                let now = Instant::now();
                if let Some(last) = self.last_esc {
                    if now.duration_since(last) <= Duration::from_millis(800) {
                        self.history_cursor = None;
                        self.input.clear();
                        self.cursor_pos = 0;
                        self.last_esc = None;
                        return None;
                    }
                }
                self.last_esc = Some(now);
                return None;
            }
            KeyCode::BackTab => {
                self.last_esc = None;
            }
            _ => {}
        }
        None
    }

    pub fn page_size(&self) -> usize {
        self.last_view_height.saturating_sub(1).max(1)
    }

    pub fn max_scroll_offset(&self) -> usize {
        if self.last_total_lines <= self.last_view_height {
            0
        } else {
            self.last_total_lines.saturating_sub(self.last_view_height)
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max_offset = self.max_scroll_offset();
        if delta.is_negative() {
            let down = (-delta) as usize;
            self.scroll_offset = self.scroll_offset.saturating_sub(down);
        } else {
            self.scroll_offset = (self.scroll_offset + delta as usize).min(max_offset);
        }
        self.follow_tail = self.scroll_offset >= max_offset;
    }

    pub fn handle_paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.history_cursor = None;
        self.input.insert_str(self.cursor_pos, &normalized);
        self.cursor_pos += normalized.len();
        self.last_esc = None;
    }

    /// Update spinner animation
    pub fn tick_spinner(&mut self) {
        if self.last_spinner.elapsed() > Duration::from_millis(120) {
            self.spinner_idx = (self.spinner_idx + 1) % SPINNER.len();
            self.last_spinner = Instant::now();
        }
    }

    /// Render the UI
    pub fn ui(&mut self, f: &mut Frame) {
        let area = f.size();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),    // Chat body
                Constraint::Length(3), // Input box
                Constraint::Length(1), // Sources + hints footer
            ])
            .split(area);

        self.render_chat(f, chunks[0]);
        self.render_input(f, chunks[1]);
        self.render_sources(f, chunks[2]);
    }

    fn render_chat(&mut self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        let width = area.width.max(1) as usize;

        let push_wrapped = |lines: &mut Vec<Line>,
                            content: &str,
                            prefix_first: &str,
                            prefix_rest: &str,
                            prefix_style: Style,
                            content_style: Style| {
            let prefix_width = prefix_first.width();
            let content_width = width.saturating_sub(prefix_width).max(1);
            let options = WrapOptions::new(content_width).break_words(true);
            let wrapped = wrap(content, &options);
            for (idx, seg) in wrapped.iter().enumerate() {
                let prefix = if idx == 0 { prefix_first } else { prefix_rest };
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(seg.to_string(), content_style),
                ]));
            }
        };

        for msg in &self.messages {
            match msg.role.as_str() {
                "You" => {
                    push_wrapped(
                        &mut lines,
                        &msg.content,
                        "› ",
                        "  ",
                        Style::default().fg(Color::Cyan),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    );
                    lines.push(Line::from(""));
                }
                "Eli" => {
                    for (i, content_line) in msg.content.lines().enumerate() {
                        if content_line.trim().is_empty() {
                            lines.push(Line::from(""));
                            continue;
                        }
                        let prefix_first = if i == 0 { "• " } else { "  " };
                        push_wrapped(
                            &mut lines,
                            content_line,
                            prefix_first,
                            "  ",
                            Style::default().fg(Color::Cyan),
                            Style::default().fg(Color::White),
                        );
                    }
                    lines.push(Line::from(""));
                }
                "Tool" => {
                    for (i, content_line) in msg.content.lines().enumerate() {
                        if content_line.trim().is_empty() {
                            lines.push(Line::from(""));
                            continue;
                        }
                        let prefix_first = if i == 0 { "· " } else { "  " };
                        push_wrapped(
                            &mut lines,
                            content_line,
                            prefix_first,
                            "  ",
                            Style::default().fg(Color::DarkGray),
                            Style::default().fg(Color::DarkGray),
                        );
                    }
                    lines.push(Line::from(""));
                }
                "System" => {
                    push_wrapped(
                        &mut lines,
                        &msg.content,
                        "ℹ ",
                        "  ",
                        Style::default().fg(Color::Blue),
                        Style::default().fg(Color::DarkGray),
                    );
                    lines.push(Line::from(""));
                }
                "Error" => {
                    push_wrapped(
                        &mut lines,
                        &msg.content,
                        "✗ ",
                        "  ",
                        Style::default().fg(Color::Red),
                        Style::default().fg(Color::Red),
                    );
                    lines.push(Line::from(""));
                }
                "Progress" => {
                    for (i, content_line) in msg.content.lines().enumerate() {
                        let trimmed = content_line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let line = if trimmed.ends_with("...") {
                            trimmed.to_string()
                        } else {
                            format!("{trimmed}...")
                        };
                        let prefix_first = if i == 0 { "· " } else { "  " };
                        push_wrapped(
                            &mut lines,
                            &line,
                            prefix_first,
                            "  ",
                            Style::default().fg(Color::DarkGray),
                            Style::default().fg(Color::DarkGray),
                        );
                    }
                    lines.push(Line::from(""));
                }
                _ => {
                    push_wrapped(
                        &mut lines,
                        &msg.content,
                        "",
                        "",
                        Style::default(),
                        Style::default().fg(Color::White),
                    );
                }
            }
        }

        if self.scrollback_max_lines > 0 && lines.len() > self.scrollback_max_lines {
            let drop = lines.len().saturating_sub(self.scrollback_max_lines);
            lines = lines.split_off(drop);
            if self.scroll_offset > drop {
                self.scroll_offset -= drop;
            } else {
                self.scroll_offset = 0;
                self.follow_tail = true;
            }
        }

        let visible_height = area.height as usize;
        let total_lines = lines.len();
        let max_offset = if total_lines > visible_height {
            total_lines - visible_height
        } else {
            0
        };
        if self.follow_tail {
            self.scroll_offset = max_offset;
        } else if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
        self.last_total_lines = total_lines;
        self.last_view_height = visible_height;
        if max_offset == 0 {
            self.follow_tail = true;
            self.scroll_offset = 0;
        }

        let chat = Paragraph::new(lines)
            .scroll((self.scroll_offset as u16, 0))
            .wrap(Wrap { trim: false });
        f.render_widget(chat, area);
    }

    fn render_input(&self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);

        let cursor_pos = self.cursor_pos.min(self.input.len());
        let display = self.input.replace('\n', "↵");
        let cursor_idx = self.input[..cursor_pos].chars().count();
        let prompt = "› ";
        let width = area.width.max(1) as usize;
        let inner_width = width.saturating_sub(2);
        let content_width = inner_width.saturating_sub(prompt.width()).max(1);

        let window_with_cursor =
            |text: &str, cursor_idx: usize, max_width: usize| -> (String, usize) {
                let chars: Vec<char> = text.chars().collect();
                if chars.is_empty() {
                    return (String::new(), 0);
                }
                let widths: Vec<usize> = chars
                    .iter()
                    .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0).max(1))
                    .collect();
                let total_width: usize = widths.iter().sum();
                let cursor_idx = cursor_idx.min(chars.len());

                if total_width <= max_width {
                    return (text.to_string(), cursor_idx);
                }

                let mut end = cursor_idx.min(chars.len());
                if end < chars.len() {
                    end = end.saturating_add(1);
                }
                let mut start = end;
                let mut width_acc = 0usize;
                while start > 0 {
                    let w = widths[start - 1];
                    if width_acc + w > max_width {
                        break;
                    }
                    width_acc += w;
                    start -= 1;
                }
                let visible: String = chars[start..end].iter().collect();
                let cursor_in_window = cursor_idx.saturating_sub(start);
                (visible, cursor_in_window)
            };

        let (visible, cursor_in_window) = window_with_cursor(&display, cursor_idx, content_width);

        let input_spans = if self.is_processing {
            vec![
                Span::styled("› ", Style::default().fg(Color::DarkGray)),
                Span::styled(visible, Style::default().fg(Color::DarkGray)),
            ]
        } else {
            let visible_chars: Vec<char> = visible.chars().collect();
            let cursor_char = if cursor_in_window < visible_chars.len() {
                visible_chars[cursor_in_window]
            } else {
                ' '
            };
            let before_cursor = visible_chars
                .iter()
                .take(cursor_in_window)
                .collect::<String>();
            let after_cursor = if cursor_in_window < visible_chars.len() {
                visible_chars
                    .iter()
                    .skip(cursor_in_window + 1)
                    .collect::<String>()
            } else {
                String::new()
            };

            vec![
                Span::styled("› ", Style::default().fg(Color::Cyan)),
                Span::styled(before_cursor, Style::default().fg(Color::White)),
                Span::styled(
                    cursor_char.to_string(),
                    Style::default().fg(Color::Black).bg(Color::White),
                ),
                Span::styled(after_cursor, Style::default().fg(Color::White)),
            ]
        };

        let title = self.input_title();
        let border_color = if self.is_processing {
            Color::DarkGray
        } else {
            Color::Cyan
        };

        let input = Paragraph::new(Line::from(input_spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(title)
                .title_style(Style::default().fg(border_color)),
        );
        f.render_widget(input, area);
    }

    fn input_title(&self) -> String {
        String::new()
    }

    fn render_sources(&self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        let mut spans: Vec<Span> = Vec::new();
        // Sources
        if !self.sources.is_empty() {
            spans.push(Span::styled(
                self.sources.join(", "),
                Style::default().fg(Color::Gray),
            ));
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }

        // Status: mode, time, tokens
        let mode = "AUTO";
        let phase = if self.is_processing {
            "working"
        } else {
            "ready"
        };
        let spinner = if self.is_processing {
            format!("{} ", SPINNER[self.spinner_idx % SPINNER.len()])
        } else {
            String::new()
        };
        let queued = if self.queue_len > 0 {
            format!(" · {} queued", self.queue_len)
        } else {
            String::new()
        };
        spans.push(Span::styled(
            format!(
                "{}{} [{}] {}s · {} tokens{}",
                spinner, phase, mode, self.elapsed_secs, self.last_request_tokens, queued
            ),
            Style::default().fg(Color::DarkGray),
        ));

        let max_offset = self.max_scroll_offset();
        if max_offset > 0 && !self.follow_tail {
            let remaining = max_offset.saturating_sub(self.scroll_offset);
            spans.push(Span::styled(
                format!(" │ scroll: {} lines above", remaining),
                Style::default().fg(Color::DarkGray),
            ));
        }

        // Hints (only when not processing)
        if !self.is_processing && self.show_tips {
            spans.push(Span::styled(
                format!(
                    " │ /compact reduce tokens │ /clear clear tokens │ /tip hide tips │ Opt+O toggle output ({})",
                    if self.show_tool_output { "on" } else { "off" }
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }

        let footer = Paragraph::new(Line::from(spans));
        f.render_widget(footer, area);
    }
}

/// Terminal wrapper for alternate screen
pub struct ChatTerminal {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
}

impl ChatTerminal {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    pub fn draw(&mut self, ui: &mut ChatUi) -> Result<()> {
        self.terminal.draw(|f| ui.ui(f))?;
        Ok(())
    }

    /// Poll for events with timeout, returns Some(Event) if available
    pub fn poll_event(&self, timeout: Duration) -> Result<Option<Event>> {
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}

impl Drop for ChatTerminal {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        )
        .ok();
        self.terminal.show_cursor().ok();
    }
}
