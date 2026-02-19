pub fn validate_model_response(response_text: &str) -> Result<ModelResponse> {
    let (value, start, end) = extract_first_json_segment(response_text)
        .ok_or_else(|| Error::InvalidInput("no JSON object found in model response".to_string()))?;
    if !response_text[..start].trim().is_empty() || !response_text[end..].trim().is_empty() {
        return Err(Error::InvalidInput(
            "response must be strict JSON only (no extra text before/after)".to_string(),
        ));
    }

    let mut resp: ModelResponse = serde_json::from_value(value)?;
    resp.focus = clean_focus(&resp.focus);

    for (idx, cmd) in resp.commands.iter().enumerate() {
        if cmd.trim().is_empty() {
            return Err(Error::InvalidInput(format!("commands[{idx}] is empty")));
        }
    }

    for (i, diff) in resp.diffs.iter().enumerate() {
        if diff.path.trim().is_empty() {
            return Err(Error::InvalidInput(format!("diffs[{i}].path is empty")));
        }
        match diff.op {
            DiffOp::Patch => {
                if diff.patch.trim().is_empty() {
                    return Err(Error::InvalidInput(format!(
                        "diffs[{i}] patch op requires non-empty patch"
                    )));
                }
            }
            DiffOp::Create | DiffOp::Replace => {
                if diff.after_text.is_empty() {
                    return Err(Error::InvalidInput(format!(
                        "diffs[{i}] {:?} op requires after_text",
                        diff.op
                    )));
                }
            }
            DiffOp::Delete => {}
        }
    }

    for (i, task) in resp.subagents.iter().enumerate() {
        if task.name.trim().is_empty() {
            return Err(Error::InvalidInput(format!("subagents[{i}].name is empty")));
        }
        if task.task.trim().is_empty() {
            return Err(Error::InvalidInput(format!("subagents[{i}].task is empty")));
        }
    }

    if matches!(resp.status, StepStatus::KeepWorking) {
        if let Some(synthesis) = &resp.synthesis {
            if !synthesis.answer.trim().is_empty() {
                return Err(Error::InvalidInput(
                    "status KEEP_WORKING cannot include synthesis.answer; reserve final answer for DONE"
                        .to_string(),
                ));
            }
        }
    }

    Ok(resp)
}

fn clean_focus(value: &str) -> String {
    let s = value.trim();
    let mut chars = s.char_indices();
    let mut end_digits = None;
    while let Some((idx, ch)) = chars.next() {
        if ch.is_ascii_digit() {
            end_digits = Some(idx + ch.len_utf8());
            continue;
        }
        break;
    }

    let Some(end) = end_digits else {
        return s.to_string();
    };
    let tail = &s[end..];
    let tail = tail.strip_prefix('.').or_else(|| tail.strip_prefix(')'));
    let Some(tail) = tail else {
        return s.to_string();
    };
    tail.trim().to_string()
}

