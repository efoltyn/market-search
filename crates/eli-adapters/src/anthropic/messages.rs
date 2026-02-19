/// Merge consecutive messages with the same role (Anthropic requirement)
fn merge_consecutive_roles(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");

        if let Some(last) = result.last_mut() {
            let last_role = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if last_role == role {
                // Merge content
                let last_content = last.get("content").and_then(|c| c.as_str()).unwrap_or("");
                *last = json!({
                    "role": role,
                    "content": format!("{}\n\n{}", last_content, content)
                });
                continue;
            }
        }
        result.push(msg);
    }

    result
}
