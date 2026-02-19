pub struct AnthropicAdapter {
    model: String,
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicAdapter {
    pub fn new(
        model: String,
        base_url: String,
        api_key: String,
        timeout_secs: u64,
    ) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Avoid macOS SystemConfiguration proxy lookup crashes.
            .no_proxy()
            .build()
            .map_err(|e| AdapterError::Http(e.to_string()))?;
        Ok(Self {
            model,
            base_url,
            api_key,
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

    /// Convert messages to Anthropic format.
    /// Returns (system_prompt, messages) tuple since Anthropic separates system from messages.
    fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<serde_json::Value>) {
        let mut system_prompt = None;
        let mut converted = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    // Anthropic wants system as a separate top-level parameter
                    system_prompt = Some(msg.content.clone());
                }
                Role::User => {
                    converted.push(json!({
                        "role": "user",
                        "content": msg.content
                    }));
                }
                Role::Assistant => {
                    converted.push(json!({
                        "role": "assistant",
                        "content": msg.content
                    }));
                }
                Role::Tool => {
                    // Tool results are sent as user messages with observation prefix
                    converted.push(json!({
                        "role": "user",
                        "content": format!("Observation: {}", msg.content)
                    }));
                }
            }
        }

        // Anthropic requires alternating user/assistant messages
        // Merge consecutive messages of the same role
        let merged = merge_consecutive_roles(converted);

        (system_prompt, merged)
    }
}
