use anyhow::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use eli_adapters::LlmAdapter;
use eli_core::agent::{Agent, AgentEvent};
use eli_core::memory::Memory;
use eli_core::config::ChatConfig;
use eli_core::diff::engine::DiffEngine;
use eli_core::executor::command_runner::CommandRunner;
use eli_core::persistence::SessionStore;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::LocalSet;

mod app;
use app::{Action, App, UiConfig};

pub async fn run(
    cfg: ChatConfig,
    adapter: Arc<dyn LlmAdapter>,
    diff_engine: DiffEngine,
    command_runner: CommandRunner,
    store: SessionStore,
    session_id: String,
) -> Result<()> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend).context("create terminal")?;

    let (action_tx, mut action_rx) = mpsc::channel::<Action>(10);
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(100);

    let ui = UiConfig {
        provider: cfg.provider.to_string(),
        model: cfg.model.clone(),
        mode: cfg.mode,
        approvals: cfg.approvals,
        auto: cfg.auto,
        mem_max: Memory::max_messages_for(cfg.mem_steps),
        compact: cfg.compact,
        parallel_commands: cfg.parallel_commands,
        parallel_subagents: cfg.parallel_subagents,
    };

    let mut agent = Agent::new(cfg, adapter, diff_engine, command_runner, store, session_id);
    agent.set_system_prompt(eli_core::contract::system_prompt());

    // Spawn agent loop (local task so we don't require Send across awaits)
    let local = LocalSet::new();
    local.spawn_local(async move {
        while let Some(action) = action_rx.recv().await {
            match action {
                Action::UserInput(msg) => {
                    let _ = agent.chat(msg, event_tx.clone()).await;
                }
                Action::Quit => break,
            }
        }
    });

    let app = App::new(action_tx, event_rx, ui);
    let res = local.run_until(async { app.run(&mut terminal).await }).await;

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    res
}
