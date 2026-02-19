pub struct OpenAiCompatibleAdapter {
    provider: ProviderKind,
    model: String,
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiCompatibleAdapter {
    pub fn new(
        provider: ProviderKind,
        model: String,
        base_url: String,
        api_key: String,
        timeout_secs: u64,
    ) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Avoid macOS SystemConfiguration proxy lookup crashes.
            // If proxy support is needed later, add explicit proxy config instead of system auto-detect.
            .no_proxy()
            .build()
            .map_err(|e| AdapterError::Http(e.to_string()))?;
        Ok(Self {
            provider,
            model,
            base_url,
            api_key: api_key.trim().to_string(),
            client,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn to_openai_message(msg: &ChatMessage) -> serde_json::Value {
        if !msg.images.is_empty() && msg.role == eli_core::types::Role::User {
            let mut content_parts = Vec::new();
            content_parts.push(json!({
                "type": "text",
                "text": msg.content
            }));

            for img in &msg.images {
                content_parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": img
                    }
                }));
            }

            let mut out = json!({
                "role": "user",
                "content": content_parts,
            });
            if let Some(name) = &msg.name {
                out["name"] = json!(name);
            }
            return out;
        }

        let (role, content) = match msg.role {
            eli_core::types::Role::System => ("system", msg.content.clone()),
            eli_core::types::Role::User => ("user", msg.content.clone()),
            eli_core::types::Role::Assistant => ("assistant", msg.content.clone()),
            eli_core::types::Role::Tool => ("user", format!("Observation: {}", msg.content)),
        };
        let mut out = json!({
            "role": role,
            "content": content,
        });
        if let Some(name) = &msg.name {
            out["name"] = json!(name);
        }
        out
    }

    fn openrouter_response_format() -> serde_json::Value {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "plan",
                "checklist",
                "focus",
                "status",
                "commands",
                "commands_parallel",
                "screen",
                "diffs",
                "notes",
                "subagents"
            ],
            "properties": {
                "plan": { "type": "string" },
                "checklist": { "type": "array", "items": { "type": "string" } },
                "focus": { "type": "string" },
                "status": { "type": "string", "enum": ["KEEP_WORKING", "DONE"] },
                "commands": { "type": "array", "items": { "type": "string" } },
                "commands_parallel": { "type": "boolean" },
                "screen": { "type": "array", "items": {} },
                "diffs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "path": { "type": "string" },
                            "op": { "type": "string", "enum": ["create", "replace", "patch", "delete"] },
                            "before_sha256": { "type": "string" },
                            "after_text": { "type": "string" },
                            "patch": { "type": "string" }
                        },
                        "oneOf": [
                            { "required": ["path", "op", "after_text"], "properties": { "op": { "const": "create" } } },
                            { "required": ["path", "op", "after_text"], "properties": { "op": { "const": "replace" } } },
                            { "required": ["path", "op", "patch"], "properties": { "op": { "const": "patch" } } },
                            { "required": ["path", "op"], "properties": { "op": { "const": "delete" } } }
                        ]
                    }
                },
                "notes": { "type": "string" },
                "synthesis": {
                    "type": ["object", "null"],
                    "additionalProperties": false,
                    "required": ["summary", "answer", "next_steps"],
                    "properties": {
                        "summary": { "type": "array", "items": { "type": "string" } },
                        "answer": { "type": "string" },
                        "next_steps": { "type": "array", "items": { "type": "string" } }
                    }
                },
                "ask_user": { "type": ["string", "null"] },
                "subagents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name", "task"],
                        "properties": {
                            "name": { "type": "string" },
                            "task": { "type": "string" },
                            "model": { "type": ["string", "null"] },
                            "temperature": { "type": ["number", "null"] },
                            "max_tokens": { "type": ["integer", "null"] }
                        }
                    }
                }
            }
        });

        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "eli_response",
                "description": "Eli tool contract response",
                "schema": schema,
                "strict": true
            }
        })
    }

    fn env_truthy(name: &str) -> bool {
        match std::env::var(name) {
            Ok(v) => matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ),
            Err(_) => false,
        }
    }

    fn openrouter_provider_preferences(&self) -> serde_json::Value {
        let sort = std::env::var("ELI_OPENROUTER_PROVIDER_SORT")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "price" | "throughput" | "latency"));
        let require_parameters = std::env::var("ELI_OPENROUTER_REQUIRE_PARAMETERS")
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);
        let allow_fallbacks = std::env::var("ELI_OPENROUTER_ALLOW_FALLBACKS")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
            .unwrap_or(true);
        let mut provider = serde_json::Map::new();
        provider.insert("allow_fallbacks".to_string(), json!(allow_fallbacks));
        provider.insert("require_parameters".to_string(), json!(require_parameters));
        if let Some(sort) = sort {
            provider.insert("sort".to_string(), json!(sort));
        }
        serde_json::Value::Object(provider)
    }

    fn openrouter_models_with_fallbacks(&self, primary_model: &str) -> Option<Vec<String>> {
        let raw = std::env::var("ELI_OPENROUTER_MODELS").ok()?;
        let mut models = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let force_free = Self::env_truthy("ELI_OPENROUTER_FORCE_FREE");
        if force_free && seen.insert("openrouter/free".to_string()) {
            models.push("openrouter/free".to_string());
        }
        let primary = primary_model.trim();
        if !primary.is_empty() {
            let primary = if force_free {
                if primary == "openrouter/free" || primary.ends_with(":free") {
                    primary.to_string()
                } else {
                    String::new()
                }
            } else {
                primary.to_string()
            };
            if !primary.is_empty() && seen.insert(primary.clone()) {
                models.push(primary);
            }
        }
        for token in raw.split(',') {
            let item = token.trim();
            if item.is_empty() {
                continue;
            }
            if force_free && item != "openrouter/free" && !item.ends_with(":free") {
                continue;
            }
            if seen.insert(item.to_string()) {
                models.push(item.to_string());
            }
        }
        if force_free {
            if models.is_empty() {
                Some(vec!["openrouter/free".to_string()])
            } else if models.len() > 1 {
                Some(models)
            } else {
                None
            }
        } else if models.len() > 1 {
            Some(models)
        } else {
            None
        }
    }

    fn collect_openai_message_content(value: &serde_json::Value) -> String {
        if let Some(s) = value
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
        {
            return s.to_string();
        }
        if let Some(obj) = value
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_object())
        {
            for key in ["text", "content", "value"] {
                if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                    if !s.trim().is_empty() {
                        return s.to_string();
                    }
                }
            }
        }
        if let Some(items) = value
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_array())
        {
            let mut out = String::new();
            for item in items {
                if let Some(s) = item.as_str() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(s);
                    continue;
                }
                let text = item
                    .get("text")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("content").and_then(|v| v.as_str()))
                    .or_else(|| item.get("value").and_then(|v| v.as_str()))
                    .or_else(|| item.pointer("/text/value").and_then(|v| v.as_str()));
                if let Some(s) = text {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(s);
                }
            }
            if !out.trim().is_empty() {
                return out;
            }
        }
        if let Some(s) = value.pointer("/choices/0/message/refusal").and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
        if let Some(s) = value.pointer("/choices/0/message/reasoning").and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
        if let Some(s) = value.pointer("/choices/0/text").and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
        if let Some(message) = value.pointer("/choices/0/message") {
            let mut fragments = Vec::new();
            Self::collect_text_fragments(message, &mut fragments, 0);
            let joined = fragments
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.trim().is_empty() {
                return joined;
            }
        }
        String::new()
    }

    fn collect_text_fragments(
        value: &serde_json::Value,
        out: &mut Vec<String>,
        depth: usize,
    ) {
        if depth > 8 {
            return;
        }
        match value {
            serde_json::Value::String(s) => {
                if !s.trim().is_empty() {
                    out.push(s.to_string());
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    Self::collect_text_fragments(item, out, depth + 1);
                }
            }
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    match k.as_str() {
                        "role" | "id" | "tool_calls" | "function_call" | "type" | "index"
                        | "usage" | "provider" | "model" | "finish_reason" => continue,
                        "content" | "text" | "reasoning" | "refusal" | "value"
                        | "output_text" => Self::collect_text_fragments(v, out, depth + 1),
                        _ => {}
                    }
                }
                if out.is_empty() {
                    for v in map.values() {
                        Self::collect_text_fragments(v, out, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }

    fn summarize_empty_assistant_payload(value: &serde_json::Value) -> String {
        let finish_reason = value
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let message_keys = value
            .pointer("/choices/0/message")
            .and_then(|v| v.as_object())
            .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_else(|| "<none>".to_string());
        let content_type = if value.pointer("/choices/0/message/content").is_some() {
            if value
                .pointer("/choices/0/message/content")
                .and_then(|v| v.as_str())
                .is_some()
            {
                "string"
            } else if value
                .pointer("/choices/0/message/content")
                .and_then(|v| v.as_array())
                .is_some()
            {
                "array"
            } else if value
                .pointer("/choices/0/message/content")
                .and_then(|v| v.as_object())
                .is_some()
            {
                "object"
            } else {
                "other"
            }
        } else {
            "missing"
        };
        let compact = serde_json::to_string(value).unwrap_or_default();
        let snippet = if compact.chars().count() > 480 {
            let tail = compact
                .chars()
                .rev()
                .take(480)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>();
            format!("...{tail}")
        } else {
            compact
        };
        format!(
            "finish_reason={finish_reason}; message_keys={message_keys}; content_type={content_type}; payload_tail={snippet}"
        )
    }
}
