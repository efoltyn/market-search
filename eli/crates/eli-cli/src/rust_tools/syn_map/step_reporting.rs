fn print_banner(chat: &eli_core::config::ChatConfig, project_root: &Path, _state: &SessionState) {
    use style::*;

    let model = truncate_middle(&chat.model, 60);
    let root = format_root_path(project_root);
    // ASCII art logo with monochrome gradient (white → gray)
    println!(
        r#"
{W1}{BOLD}  ███████╗██╗     ██╗{RESET}
{W2}{BOLD}  ██╔════╝██║     ██║{RESET}     {WHITE}financial coding agent{RESET}
{W3}{BOLD}  █████╗  ██║     ██║{RESET}     {GRAY}v0.1.0{RESET}
{W4}{BOLD}  ██╔══╝  ██║     ██║{RESET}
{W5}{BOLD}  ███████╗███████╗██║{RESET}
{W6}{BOLD}  ╚══════╝╚══════╝╚═╝{RESET}
"#,
        W1 = "\x1b[38;5;255m", // bright white
        W2 = "\x1b[38;5;252m", // light gray
        W3 = "\x1b[38;5;249m", // medium light
        W4 = "\x1b[38;5;246m", // medium gray
        W5 = "\x1b[38;5;243m", // darker gray
        W6 = "\x1b[38;5;240m", // dark gray
    );

    println!("{}({} / {}){}", GRAY, chat.provider, model, RESET);
    println!("{}cwd{} {}", GRAY, RESET, root);
    println!("{}Auto mode. /help for commands.{}", DARK_GRAY, RESET);
    println!();
}

fn print_step_summary(step: u32, model: &eli_core::contract::ModelResponse) {
    use style::*;

    let mut lines = Vec::new();
    if !model.notes.trim().is_empty() {
        lines.push(format!(
            "{}eli[{}]{} {}",
            CYAN,
            step,
            RESET,
            model.notes.trim()
        ));
    }

    let mut plan_lines = model.plan.lines();
    if let Some(first) = plan_lines.next() {
        if !first.trim().is_empty() {
            lines.push(format!("{}→{} plan: {}", PURPLE, RESET, first.trim()));
        }
    }
    if let Some(second) = plan_lines.next() {
        if !second.trim().is_empty() {
            lines.push(format!("{}→{} next: {}", BLUE, RESET, second.trim()));
        }
    }

    if !model.focus.trim().is_empty() {
        lines.push(format!(
            "{}◆{} focus: {}",
            YELLOW,
            RESET,
            model.focus.trim()
        ));
    }

    if !model.checklist.is_empty() {
        lines.push(format!("{}checklist:{}", GRAY, RESET));
        for item in model.checklist.iter().take(4) {
            if !item.trim().is_empty() {
                lines.push(format!("  {}•{} {}", GREEN, RESET, item.trim()));
            }
        }
        if model.checklist.len() > 4 {
            lines.push(format!(
                "  {}... +{} more{}",
                DARK_GRAY,
                model.checklist.len() - 4,
                RESET
            ));
        }
    }

    let status = match model.status {
        StepStatus::KeepWorking => format!("{}● keep_working{}", YELLOW, RESET),
        StepStatus::Done => format!("{}✓ done{}", GREEN, RESET),
    };
    lines.push(format!("status: {}", status));

    let out = format_indented_block(&lines);
    println!("{}", out);
}

/// Brief step summary for standard mode - one line
fn print_step_summary_brief(
    _step: u32,
    elapsed: Duration,
    model: &eli_core::contract::ModelResponse,
) {
    let _ = elapsed;
    match model.status {
        StepStatus::KeepWorking => {
            // Show focus/plan when still working
            let focus = if model.focus.trim().is_empty() {
                model.notes.lines().next().unwrap_or("").trim()
            } else {
                model.focus.trim()
            };
            if focus.is_empty() {
                return;
            }
            print_history_line(format!("→ {}", focus));
        }
        StepStatus::Done => {
            // Show the actual response/answer unboxed
            let answer = model
                .synthesis
                .as_ref()
                .map(|s| s.answer.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| model.notes.trim());
            if answer.is_empty() {
                return;
            }

            print_history_line(String::new());
            print_markdown(answer);
        }
    };
}

fn extract_insight(
    command_results: &[CommandResult],
    diff_results: &[DiffResult],
) -> Option<String> {
    for result in command_results {
        if let Some(line) = result.stdout.lines().find(|l| !l.trim().is_empty()) {
            return Some(truncate_line(line.trim(), 120));
        }
    }

    if let Some(diff) = diff_results.first() {
        let detail = format!("{} {}", diff.op, diff.path);
        return Some(truncate_line(&detail, 120));
    }

    None
}

