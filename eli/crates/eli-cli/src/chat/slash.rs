#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptMode {
    Ask,
    Plan,
    Auto,
}

fn prompt_mode(state: &SessionState, chat: &eli_core::config::ChatConfig) -> PromptMode {
    let _ = (state, chat);
    PromptMode::Auto
}

fn print_history_block(lines: Vec<String>) {
    use std::io::Write;

    let out = format_indented_block(&lines);
    if !out.is_empty() {
        print!("{}", out);
        std::io::stdout().flush().ok();
    }
}

fn print_history_line(line: String) {
    print_history_block(vec![line]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentProfile {
    Coding,
    Research,
}

impl SessionState {
    fn new(cfg: &eli_core::config::ChatConfig) -> Self {
        Self {
            display_mode: cfg.display_mode,
            auto_mode: cfg.auto_mode,
            total_work_time: Duration::ZERO,
            step_count: 0,
            prompt_queue: Vec::new(),
            input_buffer: String::new(),
            cursor_pos: 0,
            prompt_history: Vec::new(),
            history_cursor: None,
            recent_research: Vec::new(),
            total_usage: eli_core::types::Usage::default(),
            last_usage: None,
        }
    }

    fn queue_prompt(&mut self, prompt: String) {
        self.prompt_queue.push(prompt);
    }

    fn next_prompt(&mut self) -> Option<String> {
        if self.prompt_queue.is_empty() {
            None
        } else {
            Some(self.prompt_queue.remove(0))
        }
    }

    fn queue_len(&self) -> usize {
        self.prompt_queue.len()
    }

    fn load_recent_research(&mut self, project_root: &Path, max_items: usize) {
        self.recent_research = discover_recent_research(project_root, max_items);
    }

    fn record_research_report(&mut self, artifact: ResearchArtifact, max_items: usize) {
        // Deduplicate by path and keep newest first.
        self.recent_research
            .retain(|a| a.rel_path != artifact.rel_path);
        self.recent_research.insert(0, artifact);
        if self.recent_research.len() > max_items {
            self.recent_research.truncate(max_items);
        }
    }

}

#[derive(Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    desc: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        desc: "show help",
    },
    SlashCommand {
        name: "/?",
        desc: "alias for /help",
    },
    SlashCommand {
        name: "/$",
        desc: "show cost/usage stats",
    },
    SlashCommand {
        name: "/brain",
        desc: "full output (tools, history, details)",
    },
    SlashCommand {
        name: "/debug",
        desc: "debug output (raw request/response + tool output + observation)",
    },
    SlashCommand {
        name: "/standard",
        desc: "brief output (recent stream, summary)",
    },
    SlashCommand {
        name: "/brief",
        desc: "alias for /standard",
    },
    SlashCommand {
        name: "/mode",
        desc: "set exec mode (read/work)",
    },
    SlashCommand {
        name: "/read",
        desc: "set exec mode to read",
    },
    SlashCommand {
        name: "/work",
        desc: "set exec mode to work",
    },
    SlashCommand {
        name: "/bot",
        desc: "work mode; cmds auto, diffs ask",
    },
    SlashCommand {
        name: "/yolo",
        desc: "work mode; auto approvals",
    },
    SlashCommand {
        name: "/model",
        desc: "set or show model for this session",
    },
    SlashCommand {
        name: "/models",
        desc: "show current model and usage",
    },
    SlashCommand {
        name: "/key",
        desc: "set API key for current provider",
    },
    SlashCommand {
        name: "/queue",
        desc: "show queued prompts",
    },
    SlashCommand {
        name: "/q",
        desc: "alias for /queue",
    },
    SlashCommand {
        name: "/clear-queue",
        desc: "clear queued prompts",
    },
    SlashCommand {
        name: "/cq",
        desc: "alias for /clear-queue",
    },
    SlashCommand {
        name: "/status",
        desc: "show current mode/stats",
    },
    SlashCommand {
        name: "/s",
        desc: "alias for /status",
    },
    SlashCommand {
        name: "/compact",
        desc: "summarize older context (reduce tokens)",
    },
    SlashCommand {
        name: "/memory compact",
        desc: "legacy alias for /compact",
    },
    SlashCommand {
        name: "/clear",
        desc: "clear conversation",
    },
    SlashCommand {
        name: "/reset",
        desc: "alias for /clear",
    },
    SlashCommand {
        name: "/new",
        desc: "alias for /clear",
    },
    SlashCommand {
        name: "/memory clear",
        desc: "legacy alias for /clear",
    },
    SlashCommand {
        name: "/tip",
        desc: "toggle tips (standard mode)",
    },
    SlashCommand {
        name: "/undo",
        desc: "undo last edit",
    },
    SlashCommand {
        name: "/exit",
        desc: "quit",
    },
    SlashCommand {
        name: "/quit",
        desc: "alias for /exit",
    },
];

#[derive(Clone, Default)]
struct SlashHelper {
    last_input_tokens: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Helper for SlashHelper {}
impl Highlighter for SlashHelper {}
impl Validator for SlashHelper {}

impl Hinter for SlashHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &RustyContext<'_>) -> Option<Self::Hint> {
        if pos < line.len() {
            return None;
        }

        if is_slash_command_context(line, pos) {
            let prefix = &line[..pos];
            if let Some(cmd) = SLASH_COMMANDS.iter().find(|c| c.name.starts_with(prefix)) {
                return Some(cmd.name[prefix.len()..].to_string());
            }
        }

        // Show token usage hint if present
        let tokens = self
            .last_input_tokens
            .load(std::sync::atomic::Ordering::Relaxed);
        if tokens > 0 {
            // "Input: ~X tokens"
            return Some(format!(
                "  {}Input: ~{} tokens{}",
                style::DARK_GRAY,
                tokens,
                style::RESET
            ));
        }

        None
    }
}

impl Completer for SlashHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &RustyContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        if !is_slash_command_context(line, pos) {
            return Ok((pos, Vec::new()));
        }
        let mut out = Vec::new();
        for cmd in SLASH_COMMANDS {
            if cmd.name.starts_with(before) {
                out.push(Pair {
                    display: format!("{:<14} {}", cmd.name, cmd.desc),
                    replacement: cmd.name.to_string(),
                });
            }
        }
        Ok((0, out))
    }
}

#[derive(Clone)]
struct SlashMenu {
    state: Arc<Mutex<SlashMenuState>>,
}

#[derive(Default)]
struct SlashMenuState {
    shown: bool,
}

impl SlashMenu {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SlashMenuState::default())),
        }
    }

    fn reset(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shown = false;
        }
    }

    fn show(&self) {
        let mut show = false;
        if let Ok(mut state) = self.state.lock() {
            if !state.shown {
                state.shown = true;
                show = true;
            }
        }
        if show {
            let lines = slash_menu_lines();
            let out = format_box_string(&lines);
            if !out.is_empty() {
                println!("{out}");
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SlashNav {
    Next,
    Prev,
}

#[derive(Clone)]
struct SlashMenuHandler {
    menu: SlashMenu,
}

impl SlashMenuHandler {
    fn new(menu: SlashMenu) -> Self {
        Self { menu }
    }
}

impl ConditionalEventHandler for SlashMenuHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: usize,
        _positive: bool,
        ctx: &rustyline::EventContext,
    ) -> Option<Cmd> {
        if ctx.pos() == 0 && ctx.line().trim().is_empty() {
            self.menu.show();
        }
        None
    }
}

#[derive(Clone)]
struct SlashNavHandler {
    menu: SlashMenu,
    dir: SlashNav,
}

impl SlashNavHandler {
    fn new(menu: SlashMenu, dir: SlashNav) -> Self {
        Self { menu, dir }
    }
}

impl ConditionalEventHandler for SlashNavHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: usize,
        _positive: bool,
        ctx: &rustyline::EventContext,
    ) -> Option<Cmd> {
        if !is_slash_command_context(ctx.line(), ctx.pos()) {
            return None;
        }
        self.menu.show();
        match self.dir {
            SlashNav::Next => Some(Cmd::Complete),
            SlashNav::Prev => Some(Cmd::CompleteBackward),
        }
    }
}
