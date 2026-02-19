fn synthesis_has_content(synthesis: &eli_core::contract::Synthesis) -> bool {
    !synthesis.summary.is_empty()
        || !synthesis.next_steps.is_empty()
        || !synthesis.answer.trim().is_empty()
}

fn format_synthesis_title(_user_message: &str) -> String {
    String::new()
}

fn print_markdown(text: &str) {
    let skin = MadSkin::default();
    skin.print_text(text);
}

fn print_synthesis_box(title: &str, synthesis: &eli_core::contract::Synthesis) {
    use style::*;

    let mut lines = Vec::new();
    // Header removed as per user request ("eli" name gone)
    if !title.trim().is_empty() {
        lines.push(format!("{}{}{}", GRAY, title, RESET));
    }

    let answer_text = synthesis.answer.trim();
    let mut seen = std::collections::HashSet::new();
    let summary: Vec<String> = synthesis
        .summary
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| !summary_repeats_answer(s, answer_text))
        .filter(|s| seen.insert(s.to_string()))
        .take(3)
        .map(|s| format!("{}•{} {}", GREEN, RESET, s))
        .collect();
    if !summary.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(summary);
    }

    if !answer_text.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("{}◆{} {}", CYAN, RESET, answer_text));
    }

    let next_steps: Vec<String> = synthesis
        .next_steps
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .take(3)
        .map(|s| s.to_string())
        .collect();
    if !next_steps.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("{}next steps:{}", PURPLE, RESET));
        for (idx, step) in next_steps.iter().enumerate() {
            lines.push(format!("{}{}. {}{}", BLUE, idx + 1, RESET, step));
        }
    }

    if lines.len() > 1 {
        let out = format_indented_block(&lines);
        println!("{}", out);
    }
}

fn normalize_for_dedupe(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn summary_repeats_answer(summary: &str, answer: &str) -> bool {
    if answer.trim().is_empty() {
        return false;
    }
    let s = normalize_for_dedupe(summary);
    let a = normalize_for_dedupe(answer);
    if s.len() < 16 || a.len() < 16 {
        return false;
    }
    a.contains(&s) || s.contains(&a)
}

fn build_fallback_synthesis(
    insights: &[String],
    answer: &str,
) -> Option<eli_core::contract::Synthesis> {
    let summary: Vec<String> = insights
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .take(5)
        .map(|s| s.to_string())
        .collect();
    let answer = answer.trim();
    if summary.is_empty() && answer.is_empty() {
        return None;
    }
    Some(eli_core::contract::Synthesis {
        summary,
        answer: answer.to_string(),
        next_steps: Vec::new(),
    })
}

fn print_subagent_results(results: &[SubagentResult]) {
    use style::*;

    if results.is_empty() {
        return;
    }
    let mut lines = Vec::new();
    lines.push(format!("{}{}subagents{}", BOLD, PURPLE, RESET));
    for result in results {
        if let Some(err) = &result.error {
            lines.push(format!(
                "{}✗{} {}: {}error{} {}",
                RED, RESET, result.name, RED, RESET, err
            ));
            continue;
        }
        if result.output.trim().is_empty() {
            lines.push(format!(
                "{}✓{} {}: {}(no output){}",
                GREEN, RESET, result.name, GRAY, RESET
            ));
            continue;
        }
        lines.push(format!("{}✓{} {}:{}", GREEN, RESET, result.name, RESET));
        for line in result.output.lines().take(6) {
            if !line.trim().is_empty() {
                lines.push(format!("  {}{}{}", GRAY, line.trim(), RESET));
            }
        }
    }
    let out = format_indented_block(&lines);
    println!("{}", out);
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

// ═══════════════════════════════════════════════════════════════════════════════
// STYLING CONSTANTS
// ═══════════════════════════════════════════════════════════════════════════════

#[allow(dead_code)]
mod style {
    // Box drawing chars (rounded)
    pub const TL: &str = "╭"; // top-left
    pub const TR: &str = "╮"; // top-right
    pub const BL: &str = "╰"; // bottom-left
    pub const BR: &str = "╯"; // bottom-right
    pub const H: &str = "─"; // horizontal
    pub const V: &str = "│"; // vertical

    // Colors (ANSI 256 / RGB where supported)
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";

    // Gradient palette for eli branding
    pub const CYAN: &str = "\x1b[38;5;51m"; // bright cyan
    pub const BLUE: &str = "\x1b[38;5;39m"; // bright blue
    pub const PURPLE: &str = "\x1b[38;5;141m"; // lavender
    pub const PINK: &str = "\x1b[38;5;213m"; // pink
    pub const GREEN: &str = "\x1b[38;5;120m"; // mint green
    pub const YELLOW: &str = "\x1b[38;5;227m"; // soft yellow
    pub const ORANGE: &str = "\x1b[38;5;215m"; // peach
    pub const RED: &str = "\x1b[38;5;203m"; // coral red
    pub const GRAY: &str = "\x1b[38;5;245m"; // medium gray
    pub const DARK_GRAY: &str = "\x1b[38;5;238m"; // dark gray
    pub const WHITE: &str = "\x1b[38;5;255m"; // bright white

    // Semantic colors
    pub const SUCCESS: &str = "\x1b[38;5;120m"; // mint
    pub const ERROR: &str = "\x1b[38;5;203m"; // coral
    pub const WARN: &str = "\x1b[38;5;215m"; // peach
    pub const INFO: &str = "\x1b[38;5;111m"; // soft blue
    pub const MUTED: &str = "\x1b[38;5;245m"; // gray

    // Spinner frames handled by indicatif (no manual frames here).
}

fn split_leading_spaces(s: &str) -> (String, &str) {
    let count = s.chars().take_while(|c| *c == ' ').count();
    let (indent, rest) = s.split_at(count);
    (indent.to_string(), rest)
}

fn split_bullet_prefix(s: &str) -> (String, String) {
    let candidates = ["- ", "* ", "• ", "=> ", "→ "];
    for cand in candidates {
        if s.starts_with(cand) {
            return (cand.to_string(), s[cand.len()..].to_string());
        }
    }
    if let Some(pos) = s.find(". ") {
        if s[..pos].chars().all(|c| c.is_ascii_digit()) {
            return (s[..pos + 2].to_string(), s[pos + 2..].to_string());
        }
    }
    (String::new(), s.to_string())
}

fn format_box_string(lines: &[String]) -> String {
    format_indented_block(lines)
}

fn format_indented_block(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let (term_width, _term_height) = terminal_size();
    if term_width < 20 {
        return lines.join("\n");
    }

    let term_width = term_width.min(140);
    let max_content_width = term_width.saturating_sub(1).max(1);
    let mut wrapped_lines = Vec::new();
    for line in lines {
        let clean = strip_ansi(line);
        if clean.trim().is_empty() {
            wrapped_lines.push(String::new());
            continue;
        }

        let (indent, rest) = split_leading_spaces(&clean);
        let (prefix, content) = split_bullet_prefix(rest);
        let full = format!("{prefix}{content}");
        let subsequent_indent = if prefix.is_empty() {
            indent.clone()
        } else {
            format!("{}{}", indent, " ".repeat(prefix.width()))
        };

        let options = WrapOptions::new(max_content_width)
            .break_words(true)
            .initial_indent(&indent)
            .subsequent_indent(&subsequent_indent);
        let wrapped = wrap(&full, &options);
        for line in wrapped {
            wrapped_lines.push(line.into_owned());
        }
    }

    let mut out = wrapped_lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

