async fn cmd_tui(provider: Option<String>, model: Option<String>) -> Result<()> {
    // Keep `eli tui` as an explicit entrypoint, but route to the same UI as `eli`/`eli chat`.
    cmd_chat(provider, model, None).await
}

fn apply_overrides(
    cfg: &mut ConfigFile,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    if let Some(provider) = provider {
        cfg.chat.provider = provider
            .parse::<ProviderKind>()
            .map_err(|e| anyhow::anyhow!(e))
            .context("parse provider")?;
    }
    if let Some(model) = model {
        cfg.chat.model = model;
    }
    Ok(())
}

use base64::Engine;

fn ensure_tui_default_model(chat: &mut eli_core::config::ChatConfig) {
    let model = chat.model.trim();
    if model.is_empty() || model.eq_ignore_ascii_case("test") {
        chat.model = config::DEFAULT_OPENROUTER_MODEL.to_string();
    }
}

fn debug_print_request(req: &ChatRequest) {
    println!("\n=== REQUEST ===");
    match serde_json::to_string_pretty(req) {
        Ok(json) => println!("{json}"),
        Err(err) => println!("(failed to serialize request: {err})"),
    }
    println!("\n=== END REQUEST ===");
}

fn process_input_for_images(input: &str) -> (String, Vec<String>) {
    let mut clean_words = Vec::new();
    let mut images = Vec::new();

    for word in input.split_whitespace() {
        let path = Path::new(word);
        if path.exists() && path.is_file() {
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                let ext = ext.to_lowercase();
                if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
                    if let Ok(bytes) = std::fs::read(path) {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        let mime = match ext.as_str() {
                            "png" => "image/png",
                            "jpg" | "jpeg" => "image/jpeg",
                            "webp" => "image/webp",
                            "gif" => "image/gif",
                            _ => "application/octet-stream",
                        };
                        images.push(format!("data:{};base64,{}", mime, b64));
                        continue; // Consumed as image
                    }
                }
            }
        }
        clean_words.push(word);
    }

    (clean_words.join(" "), images)
}

