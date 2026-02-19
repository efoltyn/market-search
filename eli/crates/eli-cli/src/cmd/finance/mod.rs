async fn cmd_setup() -> Result<()> {
    use std::io::Write;
    let paths = Paths::discover().context("discover paths")?;
    paths.ensure_dirs().context("ensure config dirs")?;
    let mut cfg = config::load_or_default(&paths).context("load config")?;

    println!("=== Eli Setup ===\n");

    // Provider selection
    println!("Select provider:");
    println!("  1) anthropic  - Claude models (recommended)");
    println!("  2) openai     - GPT models");
    println!("  3) openrouter - Multiple providers via OpenRouter");
    println!("  4) ollama     - Local models (no API key needed)");
    print!("\nChoice [1-4]: ");
    std::io::stdout().flush().ok();

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("read provider choice")?;
    let provider = match input.trim() {
        "1" | "anthropic" => ProviderKind::Anthropic,
        "2" | "openai" => ProviderKind::OpenAI,
        "3" | "openrouter" => ProviderKind::OpenRouter,
        "4" | "ollama" => ProviderKind::Ollama,
        _ => {
            println!("Invalid choice, defaulting to anthropic");
            ProviderKind::Anthropic
        }
    };
    cfg.chat.provider = provider;

    // Model selection with smart defaults
    let default_model = match provider {
        ProviderKind::Anthropic => "claude-sonnet-4-20250514",
        ProviderKind::OpenAI => "gpt-4o",
        ProviderKind::OpenRouter => "mistralai/devstral-2512:free",
        ProviderKind::Ollama => "llama3.2",
        ProviderKind::Mock => "mock",
    };

    print!("\nModel [{}]: ", default_model);
    std::io::stdout().flush().ok();
    input.clear();
    std::io::stdin()
        .read_line(&mut input)
        .context("read model")?;
    let model = input.trim();
    cfg.chat.model = if model.is_empty() {
        default_model.to_string()
    } else {
        model.to_string()
    };

    // API key (skip for Ollama)
    if provider != ProviderKind::Ollama {
        print!("\nAPI Key: ");
        std::io::stdout().flush().ok();
        input.clear();
        std::io::stdin()
            .read_line(&mut input)
            .context("read api key")?;
        let key = input.trim().to_string();

        if !key.is_empty() {
            match provider {
                ProviderKind::Anthropic => cfg.chat.anthropic_api_key = Some(key),
                ProviderKind::OpenAI => cfg.chat.openai_api_key = Some(key),
                ProviderKind::OpenRouter => cfg.chat.openrouter_api_key = Some(key),
                _ => {} // Should not happen
            }
        }
    }

    // Save config
    config::save(&paths, &cfg).context("save config")?;

    println!("\n=== Configuration saved! ===");
    println!("Config file: {}", paths.config_file().display());
    println!("Provider: {}", cfg.chat.provider);
    println!("Model: {}", cfg.chat.model);
    println!("\nJust run 'eli' to start chatting!");

    Ok(())
}

async fn cmd_init() -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;
    let cfg = config::load_or_create(&paths).context("load/create config")?;
    println!("Config file: {}", paths.config_file().display());
    println!(
        "{}",
        toml::to_string_pretty(&cfg).context("serialize config")?
    );
    Ok(())
}

async fn cmd_config(set: Option<String>, value: Option<String>) -> Result<()> {
    let paths = Paths::discover().context("discover paths")?;

    // If setting a value
    if let Some(key) = set {
        let val = value.unwrap_or_default();
        let mut cfg = config::load_or_create(&paths).context("load config")?;

        match key.to_lowercase().as_str() {
            "provider" => {
                cfg.chat.provider = val
                    .parse::<ProviderKind>()
                    .map_err(|e| anyhow::anyhow!(e))
                    .context("invalid provider")?;
                println!("Set provider = {}", cfg.chat.provider);
            }
            "model" => {
                cfg.chat.model = val.clone();
                println!("Set model = {}", val);
            }
            "mem_steps" | "memory" | "mem" => {
                cfg.chat.mem_steps = val.parse::<usize>().context("mem_steps must be a number")?;
                println!("Set mem_steps = {}", cfg.chat.mem_steps);
            }
            "key" | "api_key" | "apikey" => {
                match cfg.chat.provider {
                    ProviderKind::Anthropic => cfg.chat.anthropic_api_key = Some(val.clone()),
                    ProviderKind::OpenAI => cfg.chat.openai_api_key = Some(val.clone()),
                    ProviderKind::OpenRouter => cfg.chat.openrouter_api_key = Some(val.clone()),
                    _ => {} // Should not happen
                }
                println!("Set API key for {}", cfg.chat.provider);
            }
            "anthropic_key" | "anthropic_api_key" => {
                cfg.chat.anthropic_api_key = Some(val.clone());
                println!("Set anthropic_api_key");
            }
            "openai_key" | "openai_api_key" => {
                cfg.chat.openai_api_key = Some(val.clone());
                println!("Set openai_api_key");
            }
            "openrouter_key" | "openrouter_api_key" => {
                cfg.chat.openrouter_api_key = Some(val.clone());
                println!("Set openrouter_api_key");
            }
            "sec_user_agent" | "sec_ua" => {
                cfg.chat.sec_user_agent = Some(val.clone());
                println!("Set sec_user_agent = {}", val);
            }
            "compact" => {
                cfg.chat.compact = parse_bool(&val)?;
                println!("Set compact = {}", cfg.chat.compact);
            }
            "compact_trigger" => {
                cfg.chat.compact_trigger = Some(
                    val.parse::<usize>()
                        .context("compact_trigger must be a number")?,
                );
                println!(
                    "Set compact_trigger = {}",
                    cfg.chat.compact_trigger.unwrap_or(0)
                );
            }
            "compact_keep" => {
                cfg.chat.compact_keep = Some(
                    val.parse::<usize>()
                        .context("compact_keep must be a number")?,
                );
                println!("Set compact_keep = {}", cfg.chat.compact_keep.unwrap_or(0));
            }
            "summary_model" => {
                cfg.chat.summary_model = if val.trim().is_empty() {
                    None
                } else {
                    Some(val.clone())
                };
                println!(
                    "Set summary_model = {}",
                    cfg.chat
                        .summary_model
                        .clone()
                        .unwrap_or_else(|| "none".to_string())
                );
            }
            "parallel_commands" | "parallel_cmds" => {
                cfg.chat.parallel_commands = val
                    .parse::<u32>()
                    .context("parallel_commands must be a number")?;
                println!("Set parallel_commands = {}", cfg.chat.parallel_commands);
            }
            "parallel_subagents" | "parallel_agents" => {
                cfg.chat.parallel_subagents = val
                    .parse::<u32>()
                    .context("parallel_subagents must be a number")?;
                println!("Set parallel_subagents = {}", cfg.chat.parallel_subagents);
            }
            "scrollback_max_lines" | "scrollback" => {
                cfg.chat.scrollback_max_lines = val
                    .parse::<usize>()
                    .context("scrollback_max_lines must be a number")?;
                println!(
                    "Set scrollback_max_lines = {}",
                    cfg.chat.scrollback_max_lines
                );
            }
            other => {
                anyhow::bail!("Unknown config key: {}. Valid keys: provider, model, mem_steps, key, anthropic_key, openai_key, openrouter_key, sec_user_agent, compact, compact_trigger, compact_keep, summary_model, parallel_commands, parallel_subagents, scrollback_max_lines", other);
            }
        }

        config::save(&paths, &cfg).context("save config")?;
        return Ok(());
    }

    // Otherwise, print current config
    let cfg = config::load_or_default(&paths).context("load config")?;
    println!("Config file: {}", paths.config_file().display());
    println!(
        "{}",
        toml::to_string_pretty(&cfg).context("serialize config")?
    );
    Ok(())
}

fn build_tool_info(path: &[String]) -> ToolInfoResponse {
    use clap::{ArgAction, ValueHint};

    let mut cmd = Cli::command();
    let mut full_path = vec![cmd.get_name().to_string()];
    let mut missing: Option<String> = None;

    for seg in path {
        let next = cmd
            .get_subcommands()
            .find(|c| c.get_name() == seg.as_str())
            .cloned();
        if let Some(sub) = next {
            cmd = sub;
            full_path.push(seg.clone());
        } else {
            missing = Some(seg.clone());
            break;
        }
    }

    let args: Vec<ToolInfoArg> = cmd
        .get_arguments()
        .map(|arg| {
            let num_args = arg.get_num_args().map(|range| ToolInfoArgCount {
                min: range.min_values(),
                max: range.max_values(),
            });

            let value_names = arg
                .get_value_names()
                .map(|names| names.iter().map(|n| n.to_string()).collect::<Vec<_>>());

            let possible_values = arg
                .get_value_parser()
                .possible_values()
                .map(|vals| vals.map(|v| v.get_name().to_string()).collect::<Vec<_>>());

            let default_values = arg
                .get_default_values()
                .iter()
                .map(|v| v.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            let default_values = if default_values.is_empty() {
                None
            } else {
                Some(default_values)
            };

            let action = arg.get_action();
            let mut value_type = if matches!(*action, ArgAction::SetTrue | ArgAction::SetFalse) {
                "bool".to_string()
            } else if matches!(*action, ArgAction::Count) {
                "count".to_string()
            } else if possible_values.is_some() {
                "enum".to_string()
            } else {
                "string".to_string()
            };

            let type_id = arg.get_value_parser().type_id();
            if value_type == "string" {
                if type_id == std::any::TypeId::of::<bool>() {
                    value_type = "bool".to_string();
                } else if type_id == std::any::TypeId::of::<std::path::PathBuf>() {
                    value_type = "path".to_string();
                } else if type_id == std::any::TypeId::of::<usize>()
                    || type_id == std::any::TypeId::of::<u64>()
                    || type_id == std::any::TypeId::of::<u32>()
                    || type_id == std::any::TypeId::of::<u16>()
                    || type_id == std::any::TypeId::of::<u8>()
                    || type_id == std::any::TypeId::of::<i64>()
                    || type_id == std::any::TypeId::of::<i32>()
                    || type_id == std::any::TypeId::of::<i16>()
                    || type_id == std::any::TypeId::of::<i8>()
                    || type_id == std::any::TypeId::of::<f64>()
                    || type_id == std::any::TypeId::of::<f32>()
                {
                    value_type = "number".to_string();
                }
            }

            if let ValueHint::FilePath | ValueHint::DirPath | ValueHint::ExecutablePath =
                arg.get_value_hint()
            {
                value_type = "path".to_string();
            }

            ToolInfoArg {
                name: arg.get_id().to_string(),
                long: arg.get_long().map(|s| s.to_string()),
                short: arg.get_short().map(|c| c.to_string()),
                help: arg.get_help().map(|s| s.to_string()),
                required: arg.is_required_set(),
                value_type,
                num_args,
                value_names,
                possible_values,
                default_values,
            }
        })
        .collect();

    let subcommands: Vec<ToolInfoSubcommand> = cmd
        .get_subcommands()
        .map(|sub| ToolInfoSubcommand {
            name: sub.get_name().to_string(),
            about: sub.get_about().map(|s| s.to_string()),
        })
        .collect();

    let (error, available_subcommands) = if let Some(missing) = missing {
        (
            Some(format!("unknown subcommand '{missing}'")),
            Some(subcommands.clone()),
        )
    } else {
        (None, None)
    };

    ToolInfoResponse {
        command: full_path.join(" "),
        about: cmd.get_about().map(|s| s.to_string()),
        args,
        subcommands,
        error,
        available_subcommands,
    }
}

fn cmd_tool_info(path: Vec<String>) -> Result<()> {
    let resp = build_tool_info(&path);

    let json = serde_json::to_string_pretty(&resp).context("serialize tool-info")?;
    println!("{json}");
    Ok(())
}

