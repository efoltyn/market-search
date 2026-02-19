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
