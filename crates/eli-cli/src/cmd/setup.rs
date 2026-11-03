// `market-search setup`: connect market-search to the AI apps on this machine.
//
// Finds every supported MCP client (its app, CLI, or config dir exists) and
// writes one stdio entry named "market-search" (command = absolute binary
// path, args = ["mcp"]) into its config. Edits are surgical: other keys and
// servers stay put and in file order, the first change leaves a one-time
// `<file>.market-search.bak`, writes are atomic (temp file + rename), and a
// file that doesn't parse is never overwritten (setup prints the lines to
// paste instead). Re-running is idempotent; a moved binary gets re-pointed.
//
// Where each client keeps its MCP servers (checked against its docs, 2026-11):
//   Claude Desktop  macOS ~/Library/Application Support/Claude/claude_desktop_config.json,
//                   Windows %APPDATA%\Claude\ (the Store/MSIX build reads
//                   %LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\ first
//                   when that file exists: anthropics/claude-code#26073), Linux ~/.config/Claude/.
//                   {"mcpServers": {name: {command, args}}}
//                   modelcontextprotocol.io/docs/develop/connect-local-servers
//   Claude Code     `claude mcp add --scope user <name> -- <cmd> <args>`. User scope is the
//                   top-level "mcpServers" of ~/.claude.json ($CLAUDE_CONFIG_DIR/.claude.json),
//                   entries {"type":"stdio", command, args, "env":{}}. code.claude.com/docs/en/mcp
//   Cursor          ~/.cursor/mcp.json, {"mcpServers": {name: {"type":"stdio", command, args}}}
//                   (its docs mark type as required). cursor.com/docs/mcp
//   VS Code         <User settings dir>/mcp.json, {"servers": {name: {"type":"stdio", ...}}}
//                   code.visualstudio.com/docs/agents/reference/mcp-configuration
//   Windsurf        renamed Devin Desktop (2026-06): ~/.config/devin/mcp_config.json,
//                   %APPDATA%\devin\ on Windows; pre-rename installs keep
//                   ~/.codeium/windsurf/mcp_config.json. {"mcpServers"}, no type.
//                   docs.devin.ai/cli/extensibility/mcp/configuration (docs.windsurf.com redirects)
//   Codex CLI       ~/.codex/config.toml ($CODEX_HOME), [mcp_servers.<name>] command + args.
//                   learn.chatgpt.com/docs/extend/mcp
//   Gemini CLI      ~/.gemini/settings.json ($GEMINI_CLI_HOME/.gemini), {"mcpServers"}.
//                   github.com/google-gemini/gemini-cli docs/tools/mcp-server.md
//   LM Studio       ~/.lmstudio/mcp.json, Cursor's {"mcpServers"} notation. lmstudio.ai/blog/mcp
//   Zed             ~/.config/zed/settings.json (%APPDATA%\Zed), JSON with comments;
//                   {"context_servers": {name: {command, args}}}. "source": "custom" stopped
//                   being required in v0.214 and Zed migrates it away. zed.dev/docs/ai/mcp
//
// Every path derives from `SetupEnv` (home, APPDATA, PATH, ...), so tests run
// the whole flow inside a temp dir.

const SETUP_SERVER_NAME: &str = "market-search";
const SETUP_DIALOG_TITLE: &str = "Market Search";
const SETUP_CLAUDE_DOWNLOAD_URL: &str = "https://claude.ai/download";
/// `claude mcp add` returns in about a second; a wedged install shouldn't hang setup.
const SETUP_CLI_TIMEOUT: Duration = Duration::from_secs(60);

// ── environment ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupOs {
    Mac,
    Linux,
    Windows,
}

impl SetupOs {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            SetupOs::Mac
        } else if cfg!(windows) {
            SetupOs::Windows
        } else {
            SetupOs::Linux
        }
    }
}

/// The bases every setup path derives from. `from_process` reads the real
/// environment; tests build one rooted in a temp dir.
#[derive(Clone, Debug)]
struct SetupEnv {
    os: SetupOs,
    home: PathBuf,
    /// %APPDATA% (Windows roaming profile).
    appdata: PathBuf,
    /// %LOCALAPPDATA% (Windows).
    local_appdata: PathBuf,
    /// $XDG_CONFIG_HOME, else ~/.config.
    xdg_config: PathBuf,
    /// Where macOS apps live: /Applications and ~/Applications.
    app_dirs: Vec<PathBuf>,
    /// Directories searched for CLIs (`claude`, `codex`, `gemini`).
    path_dirs: Vec<PathBuf>,
    /// $CLAUDE_CONFIG_DIR: Claude Code keeps .claude.json there when set.
    claude_config_dir: Option<PathBuf>,
    /// $CODEX_HOME: Codex's config dir when set (default ~/.codex).
    codex_home: Option<PathBuf>,
    /// $GEMINI_CLI_HOME: Gemini CLI uses $GEMINI_CLI_HOME/.gemini when set.
    gemini_cli_home: Option<PathBuf>,
    /// A binary under any of these is about to disappear.
    temp_dirs: Vec<PathBuf>,
}

impl SetupEnv {
    fn from_process() -> Result<Self> {
        let os = SetupOs::current();
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = match os {
            SetupOs::Windows => var("USERPROFILE").or_else(|| var("HOME")),
            _ => var("HOME"),
        }
        .context("could not find your home directory (HOME is not set)")?;

        let mut path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        // A double-clicked app inherits launchd's bare PATH (/usr/bin:/bin:...),
        // so also look where Homebrew and npm put CLIs.
        if os == SetupOs::Mac {
            for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
                let p = PathBuf::from(extra);
                if !path_dirs.contains(&p) {
                    path_dirs.push(p);
                }
            }
        }

        let mut temp_dirs = vec![std::env::temp_dir()];
        if os != SetupOs::Windows {
            for t in [
                "/tmp",
                "/private/tmp",
                "/var/tmp",
                "/private/var/tmp",
                "/var/folders",
                "/private/var/folders",
            ] {
                temp_dirs.push(PathBuf::from(t));
            }
        }

        Ok(Self {
            os,
            appdata: var("APPDATA").unwrap_or_else(|| home.join("AppData").join("Roaming")),
            local_appdata: var("LOCALAPPDATA")
                .unwrap_or_else(|| home.join("AppData").join("Local")),
            xdg_config: var("XDG_CONFIG_HOME")
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(".config")),
            app_dirs: vec![PathBuf::from("/Applications"), home.join("Applications")],
            path_dirs,
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            codex_home: var("CODEX_HOME"),
            gemini_cli_home: var("GEMINI_CLI_HOME"),
            temp_dirs,
            home,
        })
    }

    /// `path` with the home dir shown as `~` (Windows paths print as-is).
    fn show(&self, path: &Path) -> String {
        if self.os != SetupOs::Windows {
            if let Ok(rest) = path.strip_prefix(&self.home) {
                return if rest.as_os_str().is_empty() {
                    "~".to_string()
                } else {
                    format!("~/{}", rest.display())
                };
            }
        }
        path.display().to_string()
    }

    /// Whether `path` is inside `root`; case-insensitive on macOS and Windows,
    /// whose default filesystems are.
    fn is_under(&self, path: &Path, root: &Path) -> bool {
        if self.os == SetupOs::Linux {
            return path.starts_with(root);
        }
        let fold = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        fold(path).starts_with(fold(root))
    }
}

// ── clients ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupClientId {
    ClaudeDesktop,
    ClaudeCode,
    Cursor,
    VsCode,
    VsCodeInsiders,
    Windsurf,
    Codex,
    Gemini,
    LmStudio,
    Zed,
}

const SETUP_CLIENTS: [SetupClientId; 10] = [
    SetupClientId::ClaudeDesktop,
    SetupClientId::ClaudeCode,
    SetupClientId::Cursor,
    SetupClientId::VsCode,
    SetupClientId::VsCodeInsiders,
    SetupClientId::Windsurf,
    SetupClientId::Codex,
    SetupClientId::Gemini,
    SetupClientId::LmStudio,
    SetupClientId::Zed,
];

impl SetupClientId {
    fn name(self) -> &'static str {
        match self {
            SetupClientId::ClaudeDesktop => "Claude Desktop",
            SetupClientId::ClaudeCode => "Claude Code",
            SetupClientId::Cursor => "Cursor",
            SetupClientId::VsCode => "VS Code",
            SetupClientId::VsCodeInsiders => "VS Code Insiders",
            SetupClientId::Windsurf => "Windsurf",
            SetupClientId::Codex => "Codex",
            SetupClientId::Gemini => "Gemini CLI",
            SetupClientId::LmStudio => "LM Studio",
            SetupClientId::Zed => "Zed",
        }
    }

    /// Stable id: the `--client` spelling and the JSON "id".
    fn key(self) -> &'static str {
        match self {
            SetupClientId::ClaudeDesktop => "claude-desktop",
            SetupClientId::ClaudeCode => "claude-code",
            SetupClientId::Cursor => "cursor",
            SetupClientId::VsCode => "vscode",
            SetupClientId::VsCodeInsiders => "vscode-insiders",
            SetupClientId::Windsurf => "windsurf",
            SetupClientId::Codex => "codex",
            SetupClientId::Gemini => "gemini-cli",
            SetupClientId::LmStudio => "lm-studio",
            SetupClientId::Zed => "zed",
        }
    }

    /// `--client` value → client. Case, dashes and spaces don't matter.
    fn from_key(raw: &str) -> Option<Self> {
        let k: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        Some(match k.as_str() {
            "claudedesktop" | "desktop" => SetupClientId::ClaudeDesktop,
            "claudecode" => SetupClientId::ClaudeCode,
            "cursor" => SetupClientId::Cursor,
            "vscode" | "code" => SetupClientId::VsCode,
            "vscodeinsiders" | "codeinsiders" | "insiders" => SetupClientId::VsCodeInsiders,
            "windsurf" | "devin" | "devindesktop" => SetupClientId::Windsurf,
            "codex" | "codexcli" => SetupClientId::Codex,
            "gemini" | "geminicli" => SetupClientId::Gemini,
            "lmstudio" => SetupClientId::LmStudio,
            "zed" => SetupClientId::Zed,
            _ => return None,
        })
    }

    /// Whether the app has to restart (or start a new session) to load a
    /// config change. VS Code and Zed watch their files.
    fn restart_required(self) -> bool {
        !matches!(
            self,
            SetupClientId::VsCode | SetupClientId::VsCodeInsiders | SetupClientId::Zed
        )
    }
}

/// How a client stores its MCP servers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupFormat {
    /// `{"mcpServers": {name: {"command", "args"}}}`: Claude Desktop,
    /// Windsurf/Devin, Gemini CLI, LM Studio.
    McpServers,
    /// Cursor: the same plus `"type": "stdio"`.
    McpServersTyped,
    /// Claude Code's ~/.claude.json, user scope: `mcpServers` entries carry
    /// `"type": "stdio"` and an `env` object (what `claude mcp add` writes).
    ClaudeCodeJson,
    /// VS Code: `{"servers": {name: {"type": "stdio", "command", "args"}}}`.
    VsCodeServers,
    /// Zed: `{"context_servers": {name: {"command", "args"}}}`.
    ZedContextServers,
    /// Codex: a `[mcp_servers.<name>]` table in config.toml.
    CodexToml,
}

#[derive(Clone, Debug)]
struct SetupClient {
    id: SetupClientId,
    /// Display name: the id's name, except Windsurf shows as Devin Desktop
    /// once renamed.
    name: &'static str,
    /// The file our entry goes into.
    config: PathBuf,
    format: SetupFormat,
    /// What showed the client is installed (app, CLI, or config dir).
    found: Option<PathBuf>,
    /// Everything checked, for "not found" messages.
    probes: Vec<PathBuf>,
    /// Claude Code: the `claude` CLI, preferred over editing ~/.claude.json.
    cli: Option<PathBuf>,
}

fn setup_detect_clients(env: &SetupEnv) -> Vec<SetupClient> {
    SETUP_CLIENTS
        .iter()
        .map(|&id| setup_detect_client(env, id))
        .collect()
}

fn setup_detect_client(env: &SetupEnv, id: SetupClientId) -> SetupClient {
    let home = &env.home;
    let apps = |bundle: &str| -> Vec<PathBuf> {
        if env.os == SetupOs::Mac {
            env.app_dirs.iter().map(|d| d.join(bundle)).collect()
        } else {
            Vec::new()
        }
    };
    // Default per-user install dir of Windows installers (Squirrel, Electron).
    let win_program = |dir: &str| -> Vec<PathBuf> {
        if env.os == SetupOs::Windows {
            vec![env.local_appdata.join("Programs").join(dir)]
        } else {
            Vec::new()
        }
    };
    let mut name = id.name();
    let mut cli = None;

    let (config, format, probes) = match id {
        SetupClientId::ClaudeDesktop => {
            let (config, mut probes) = match env.os {
                SetupOs::Mac => {
                    let dir = home
                        .join("Library")
                        .join("Application Support")
                        .join("Claude");
                    (dir.join("claude_desktop_config.json"), vec![dir])
                }
                SetupOs::Linux => {
                    let dir = env.xdg_config.join("Claude");
                    (dir.join("claude_desktop_config.json"), vec![dir])
                }
                SetupOs::Windows => {
                    let dir = env.appdata.join("Claude");
                    let mut config = dir.join("claude_desktop_config.json");
                    let mut probes = vec![dir, env.local_appdata.join("AnthropicClaude")];
                    // The Store (MSIX) build opens its per-package copy first when
                    // one exists, and only falls back to the real %APPDATA%.
                    if let Some(pkg) = setup_find_msix_package(&env.local_appdata, "Claude_") {
                        let virtualized = pkg
                            .join("LocalCache")
                            .join("Roaming")
                            .join("Claude")
                            .join("claude_desktop_config.json");
                        if virtualized.is_file() {
                            config = virtualized;
                        }
                        probes.push(pkg);
                    }
                    (config, probes)
                }
            };
            probes.extend(apps("Claude.app"));
            (config, SetupFormat::McpServers, probes)
        }
        SetupClientId::ClaudeCode => {
            let config = setup_claude_json_path(env);
            cli = setup_find_claude_cli(env);
            let mut probes: Vec<PathBuf> = cli.iter().cloned().collect();
            probes.push(config.clone());
            (config, SetupFormat::ClaudeCodeJson, probes)
        }
        SetupClientId::Cursor => {
            let dir = home.join(".cursor");
            let mut probes = vec![dir.clone()];
            probes.extend(apps("Cursor.app"));
            probes.extend(win_program("cursor"));
            (dir.join("mcp.json"), SetupFormat::McpServersTyped, probes)
        }
        SetupClientId::VsCode | SetupClientId::VsCodeInsiders => {
            let (product, bundle, win_dir) = if id == SetupClientId::VsCode {
                ("Code", "Visual Studio Code.app", "Microsoft VS Code")
            } else {
                (
                    "Code - Insiders",
                    "Visual Studio Code - Insiders.app",
                    "Microsoft VS Code Insiders",
                )
            };
            // The default profile's User dir. Other profiles keep their own
            // mcp.json under User/profiles/<id>/ and aren't touched.
            let user = match env.os {
                SetupOs::Mac => home
                    .join("Library")
                    .join("Application Support")
                    .join(product)
                    .join("User"),
                SetupOs::Windows => env.appdata.join(product).join("User"),
                SetupOs::Linux => env.xdg_config.join(product).join("User"),
            };
            let mut probes = vec![user.clone()];
            probes.extend(apps(bundle));
            probes.extend(win_program(win_dir));
            (user.join("mcp.json"), SetupFormat::VsCodeServers, probes)
        }
        SetupClientId::Windsurf => {
            let devin_dir = match env.os {
                SetupOs::Windows => env.appdata.join("devin"),
                _ => env.xdg_config.join("devin"),
            };
            let mut devin = vec![devin_dir.clone()];
            devin.extend(apps("Devin.app"));
            let legacy_dir = home.join(".codeium").join("windsurf");
            let mut legacy = vec![legacy_dir.clone()];
            legacy.extend(apps("Windsurf.app"));
            legacy.extend(win_program("Windsurf"));
            if !devin.iter().any(|p| p.exists()) && legacy.iter().any(|p| p.exists()) {
                // A pre-rename install.
                (
                    legacy_dir.join("mcp_config.json"),
                    SetupFormat::McpServers,
                    legacy,
                )
            } else {
                if devin.iter().any(|p| p.exists()) {
                    name = "Devin Desktop";
                }
                devin.extend(legacy);
                (
                    devin_dir.join("mcp_config.json"),
                    SetupFormat::McpServers,
                    devin,
                )
            }
        }
        SetupClientId::Codex => {
            let dir = env
                .codex_home
                .clone()
                .unwrap_or_else(|| home.join(".codex"));
            let mut probes = vec![dir.clone()];
            probes.extend(setup_find_cli(env, "codex"));
            (dir.join("config.toml"), SetupFormat::CodexToml, probes)
        }
        SetupClientId::Gemini => {
            let dir = env.gemini_cli_home.as_ref().unwrap_or(home).join(".gemini");
            let mut probes = vec![dir.clone()];
            probes.extend(setup_find_cli(env, "gemini"));
            (dir.join("settings.json"), SetupFormat::McpServers, probes)
        }
        SetupClientId::LmStudio => {
            let dir = setup_lmstudio_home(env);
            let mut probes = vec![dir.clone()];
            probes.extend(apps("LM Studio.app"));
            probes.extend(win_program("LM Studio"));
            (dir.join("mcp.json"), SetupFormat::McpServers, probes)
        }
        SetupClientId::Zed => {
            // Zed reads ~/.config/zed on macOS too, not ~/Library.
            let dir = match env.os {
                SetupOs::Mac => home.join(".config").join("zed"),
                SetupOs::Windows => env.appdata.join("Zed"),
                SetupOs::Linux => env.xdg_config.join("zed"),
            };
            let mut probes = vec![dir.clone()];
            probes.extend(apps("Zed.app"));
            probes.extend(apps("Zed Preview.app"));
            if env.os == SetupOs::Linux {
                // Where zed.dev/install.sh puts the app.
                probes.push(home.join(".local").join("zed.app"));
            }
            (
                dir.join("settings.json"),
                SetupFormat::ZedContextServers,
                probes,
            )
        }
    };

    let found = probes.iter().find(|p| p.exists()).cloned();
    SetupClient {
        id,
        name,
        config,
        format,
        found,
        probes,
        cli,
    }
}

/// Detected clients, narrowed to `only` (in canonical order) when given.
fn setup_select_clients(env: &SetupEnv, only: Option<&[SetupClientId]>) -> Vec<SetupClient> {
    setup_detect_clients(env)
        .into_iter()
        .filter(|c| only.is_none_or(|o| o.contains(&c.id)))
        .collect()
}

fn setup_parse_clients(names: &[String]) -> Result<Option<Vec<SetupClientId>>> {
    if names.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for raw in names {
        if raw.trim().is_empty() {
            continue;
        }
        let id = SetupClientId::from_key(raw).with_context(|| {
            format!(
                "unknown client '{}'. Choose from: {}",
                raw.trim(),
                SETUP_CLIENTS.iter().map(|c| c.key()).join(", ")
            )
        })?;
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(Some(out))
}

fn setup_find_msix_package(local_appdata: &Path, prefix: &str) -> Option<PathBuf> {
    std::fs::read_dir(local_appdata.join("Packages"))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
}

/// Claude Code's global config: $CLAUDE_CONFIG_DIR/.claude.json or
/// ~/.claude.json, except that a non-empty legacy `<config dir>/.config.json`
/// still wins when it exists.
fn setup_claude_json_path(env: &SetupEnv) -> PathBuf {
    let config_dir = env
        .claude_config_dir
        .clone()
        .unwrap_or_else(|| env.home.join(".claude"));
    let legacy = config_dir.join(".config.json");
    if std::fs::metadata(&legacy).is_ok_and(|m| m.is_file() && m.len() > 0) {
        return legacy;
    }
    match &env.claude_config_dir {
        Some(dir) => dir.join(".claude.json"),
        None => env.home.join(".claude.json"),
    }
}

/// LM Studio's home: what ~/.lmstudio-home-pointer names, else the older
/// ~/.cache/lm-studio, else ~/.lmstudio (the order LM Studio itself checks).
fn setup_lmstudio_home(env: &SetupEnv) -> PathBuf {
    if let Ok(text) = std::fs::read_to_string(env.home.join(".lmstudio-home-pointer")) {
        let p = PathBuf::from(text.trim());
        if p.is_absolute() && p.is_dir() {
            return p;
        }
    }
    let older = env.home.join(".cache").join("lm-studio");
    if older.is_dir() {
        return older;
    }
    env.home.join(".lmstudio")
}

/// The `claude` CLI: on the search path, else the native installer's spots.
fn setup_find_claude_cli(env: &SetupEnv) -> Option<PathBuf> {
    setup_find_cli(env, "claude").or_else(|| {
        let exe = if env.os == SetupOs::Windows {
            "claude.exe"
        } else {
            "claude"
        };
        [
            env.home.join(".claude").join("local").join(exe),
            env.home.join(".local").join("bin").join(exe),
        ]
        .into_iter()
        .find(|p| setup_is_executable(p))
    })
}

/// First `name` executable on the search path (plus Windows shim extensions).
fn setup_find_cli(env: &SetupEnv, name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if env.os == SetupOs::Windows {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    env.path_dirs.iter().find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .find(|p| setup_is_executable(p))
    })
}

fn setup_is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

// ── the binary configs point at ─────────────────────────────────────────────

#[derive(Clone, Debug)]
struct SetupBinary {
    /// The executable running now.
    current: PathBuf,
    /// The path written into every config.
    target: PathBuf,
    /// Set when `current` won't be there tomorrow: it gets copied to `target`.
    copy_reason: Option<&'static str>,
}

fn setup_resolve_binary(
    env: &SetupEnv,
    current_exe: &Path,
    explicit: Option<&Path>,
) -> Result<SetupBinary> {
    if let Some(p) = explicit {
        let abs = std::path::absolute(p).with_context(|| format!("--binary {}", p.display()))?;
        if !abs.is_file() {
            anyhow::bail!("--binary {}: no such file", abs.display());
        }
        return Ok(SetupBinary {
            current: abs.clone(),
            target: abs,
            copy_reason: None,
        });
    }
    let current = std::path::absolute(current_exe).context("resolve the running binary's path")?;
    // Homebrew on Linux reports the versioned Cellar path; configs should name
    // the <prefix>/bin symlink, which survives `brew upgrade`.
    let current = setup_unversion_cellar(&current).unwrap_or(current);
    Ok(match setup_ephemeral_reason(env, &current) {
        Some(reason) => SetupBinary {
            target: setup_stable_binary_path(env),
            current,
            copy_reason: Some(reason),
        },
        None => SetupBinary {
            target: current.clone(),
            current,
            copy_reason: None,
        },
    })
}

/// Why a binary at `exe` can't be what AI apps launch tomorrow (None = fine).
fn setup_ephemeral_reason(env: &SetupEnv, exe: &Path) -> Option<&'static str> {
    let parts: Vec<String> = exe
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    let has = |name: &str| parts.iter().any(|p| p == name);

    // npx/npm, pnpm dlx and bunx unpack packages into caches they prune.
    if has("_npx")
        || has(".npm")
        || has("npm-cache")
        || parts.windows(2).any(|w| w[0] == "pnpm" && w[1] == "dlx")
        || parts.iter().any(|p| p.starts_with("bunx-"))
    {
        return Some("an npx/npm package cache");
    }
    // Gatekeeper runs a quarantined app from a random read-only mount.
    if has("apptranslocation") {
        return Some("a Gatekeeper App Translocation folder");
    }
    if env.os == SetupOs::Mac && env.is_under(exe, Path::new("/Volumes")) {
        return Some("a mounted disk image");
    }
    if env.temp_dirs.iter().any(|t| env.is_under(exe, t)) {
        return Some("a temporary folder");
    }
    if env.is_under(exe, &env.home.join("Downloads")) {
        return Some("the Downloads folder");
    }
    // macOS asks before an app may read Desktop/Documents, so a server there
    // fails to launch until someone clicks Allow in Claude's name.
    if env.os == SetupOs::Mac
        && (env.is_under(exe, &env.home.join("Desktop"))
            || env.is_under(exe, &env.home.join("Documents")))
    {
        return Some("a folder macOS protects (Desktop/Documents)");
    }
    if let Some(bundle) = setup_app_bundle(exe) {
        if !env.app_dirs.iter().any(|d| env.is_under(&bundle, d)) {
            return Some("an app outside the Applications folder");
        }
    }
    None
}

/// The `.app` bundle containing `exe` (main executable or an embedded helper).
fn setup_app_bundle(exe: &Path) -> Option<PathBuf> {
    let s = exe.to_string_lossy();
    let i = s.find(".app/Contents/")?;
    Some(PathBuf::from(&s[..i + ".app".len()]))
}

/// `<prefix>/Cellar/market-search/<ver>/bin/x` → `<prefix>/bin/x`, when that
/// symlink exists and points at the same file.
fn setup_unversion_cellar(exe: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = exe.components().collect();
    let idx = comps.iter().position(|c| c.as_os_str() == "Cellar")?;
    let prefix: PathBuf = comps[..idx].iter().collect();
    let link = prefix.join("bin").join(exe.file_name()?);
    let same = std::fs::canonicalize(&link).ok()? == std::fs::canonicalize(exe).ok()?;
    same.then_some(link)
}

/// Where an ephemeral binary gets copied. Never Desktop/Documents/Downloads:
/// macOS gates those behind a permission prompt.
fn setup_stable_binary_path(env: &SetupEnv) -> PathBuf {
    match env.os {
        SetupOs::Windows => env
            .local_appdata
            .join("market-search")
            .join("market-search.exe"),
        _ => env.home.join(".local").join("bin").join("market-search"),
    }
}

fn setup_same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {
            matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
        }
        _ => false,
    }
}

/// Copy the binary before any config points at it: when it runs from
/// somewhere temporary, some app will use it, and the stable copy differs.
fn setup_needs_copy(binary: &SetupBinary, op: SetupOp, plan: &[SetupRow]) -> bool {
    op == SetupOp::Connect
        && binary.copy_reason.is_some()
        && plan.iter().any(|r| {
            matches!(
                r.status,
                SetupStatus::Added | SetupStatus::Updated(_) | SetupStatus::Current
            )
        })
        && !setup_same_contents(&binary.current, &binary.target)
}

/// Said when the copy would overwrite a different binary at the stable path.
fn setup_replacing_note(binary: &SetupBinary) -> &'static str {
    if binary.target.exists() {
        " (replacing the different binary there)"
    } else {
        ""
    }
}

/// Copy the running binary to its stable home (temp file + rename, so a copy
/// that's running right now keeps its inode). Returns false when an identical
/// copy is already there.
fn setup_install_binary(env: &SetupEnv, from: &Path, to: &Path) -> Result<bool> {
    if setup_same_contents(from, to) {
        return Ok(false);
    }
    let dir = to
        .parent()
        .context("the stable binary path has no parent dir")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = setup_suffixed(to, &format!(".{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(from, &tmp)
        .with_context(|| format!("copy {} to {}", from.display(), tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod +x {}", tmp.display()))?;
    }
    if env.os == SetupOs::Mac {
        // std::fs::copy carries extended attributes over, including a
        // download's quarantine flag; Claude Desktop or launchd spawning a
        // quarantined copy is fragile. Best effort.
        let _ = std::process::Command::new("/usr/bin/xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&tmp)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // A running .exe can't be replaced, but it can be renamed out of the way.
    let old = to.with_extension("old.exe");
    let moved_aside = env.os == SetupOs::Windows && to.exists() && {
        let _ = std::fs::remove_file(&old);
        std::fs::rename(to, &old).is_ok()
    };
    if let Err(e) = std::fs::rename(&tmp, to) {
        let _ = std::fs::remove_file(&tmp);
        if moved_aside {
            let _ = std::fs::rename(&old, to);
        }
        return Err(e).with_context(|| format!("move the binary into {}", to.display()));
    }
    Ok(true)
}

// ── the entry ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
struct SetupEntry {
    command: String,
    args: Vec<String>,
}

impl SetupEntry {
    fn for_binary(path: &Path) -> Self {
        Self {
            command: path.to_string_lossy().into_owned(),
            args: vec!["mcp".to_string()],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupOp {
    Connect,
    Remove,
}

/// What a config edit changes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SetupChange {
    /// Connect: already points at this binary. Remove: no entry to remove.
    Nothing,
    Add,
    /// The entry existed with another command or args (the old command).
    Update(String),
    Remove,
}

/// One config edit, before and after, as text.
#[derive(Clone, Debug)]
struct SetupEdit {
    change: SetupChange,
    /// Our existing entry, rendered (None = there is none).
    before: Option<String>,
    /// Our entry after the edit, rendered (None = gone).
    after: Option<String>,
    /// The whole new file, when it needs writing.
    new_text: Option<String>,
}

impl SetupEdit {
    fn unchanged(before: Option<String>, after: Option<String>) -> Self {
        Self {
            change: SetupChange::Nothing,
            before,
            after,
            new_text: None,
        }
    }
}

// ── JSON configs ────────────────────────────────────────────────────────────

/// A JSON value that keeps object keys in file order. serde_json's Value sorts
/// them (this workspace doesn't enable `preserve_order`), which would reshuffle
/// every config file setup touches.
#[derive(Clone, Debug, PartialEq)]
enum SetupJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<SetupJson>),
    Object(indexmap::IndexMap<String, SetupJson>),
}

impl<'de> Deserialize<'de> for SetupJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = SetupJson;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_bool<E>(self, v: bool) -> std::result::Result<SetupJson, E> {
                Ok(SetupJson::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> std::result::Result<SetupJson, E> {
                Ok(SetupJson::Number(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> std::result::Result<SetupJson, E> {
                Ok(SetupJson::Number(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<SetupJson, E> {
                serde_json::Number::from_f64(v)
                    .map(SetupJson::Number)
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E>(self, v: &str) -> std::result::Result<SetupJson, E> {
                Ok(SetupJson::String(v.to_string()))
            }
            fn visit_string<E>(self, v: String) -> std::result::Result<SetupJson, E> {
                Ok(SetupJson::String(v))
            }
            fn visit_unit<E>(self) -> std::result::Result<SetupJson, E> {
                Ok(SetupJson::Null)
            }
            fn visit_none<E>(self) -> std::result::Result<SetupJson, E> {
                Ok(SetupJson::Null)
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> std::result::Result<SetupJson, D2::Error> {
                SetupJson::deserialize(d)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<SetupJson, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element()? {
                    out.push(v);
                }
                Ok(SetupJson::Array(out))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<SetupJson, A::Error> {
                let mut out = indexmap::IndexMap::new();
                while let Some((k, v)) = map.next_entry::<String, SetupJson>()? {
                    out.insert(k, v);
                }
                Ok(SetupJson::Object(out))
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for SetupJson {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        match self {
            SetupJson::Null => s.serialize_unit(),
            SetupJson::Bool(b) => s.serialize_bool(*b),
            SetupJson::Number(n) => n.serialize(s),
            SetupJson::String(v) => s.serialize_str(v),
            SetupJson::Array(items) => items.serialize(s),
            SetupJson::Object(m) => {
                let mut map = s.serialize_map(Some(m.len()))?;
                for (k, v) in m {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

/// Parse a config file's text. A UTF-8 BOM is tolerated; an empty file reads
/// as `{}`. Anything that isn't a strict JSON object is an error, comments
/// and trailing commas included, since rewriting the file would drop them.
fn setup_json_parse(text: &str) -> std::result::Result<SetupJson, String> {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    if t.trim().is_empty() {
        return Ok(SetupJson::Object(indexmap::IndexMap::new()));
    }
    match serde_json::from_str::<SetupJson>(t) {
        Ok(v @ SetupJson::Object(_)) => Ok(v),
        Ok(_) => Err("the file isn't a JSON object".to_string()),
        Err(e) if t.contains("//") || t.contains("/*") => {
            Err(format!("the file has comments or isn't strict JSON ({e})"))
        }
        Err(e) => Err(format!("the file isn't valid JSON ({e})")),
    }
}

/// 2-space pretty JSON, keeping the original's CRLF line endings and final
/// newline (new files end with one).
fn setup_json_render(
    doc: &SetupJson,
    original: Option<&str>,
) -> std::result::Result<String, String> {
    let mut out = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    if original.is_none_or(|t| t.ends_with('\n') || t.trim().is_empty()) {
        out.push('\n');
    }
    if original.is_some_and(|t| t.contains("\r\n")) {
        out = out.replace('\n', "\r\n");
    }
    Ok(out)
}

/// One-line JSON with spaces after `:` and `,`, for showing an entry.
fn setup_json_inline(v: &SetupJson) -> String {
    match v {
        SetupJson::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!(
                    "{}: {}",
                    serde_json::to_string(k).unwrap_or_default(),
                    setup_json_inline(v)
                ))
                .join(", ")
        ),
        SetupJson::Array(items) => format!("[{}]", items.iter().map(setup_json_inline).join(", ")),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// The object that holds MCP servers in each JSON format.
fn setup_json_key(format: SetupFormat) -> &'static str {
    match format {
        SetupFormat::VsCodeServers => "servers",
        SetupFormat::ZedContextServers => "context_servers",
        _ => "mcpServers",
    }
}

fn setup_json_entry_value(format: SetupFormat, entry: &SetupEntry) -> SetupJson {
    let s = |v: &str| SetupJson::String(v.to_string());
    let mut m = indexmap::IndexMap::new();
    if matches!(
        format,
        SetupFormat::McpServersTyped | SetupFormat::ClaudeCodeJson | SetupFormat::VsCodeServers
    ) {
        m.insert("type".to_string(), s("stdio"));
    }
    m.insert("command".to_string(), s(&entry.command));
    m.insert(
        "args".to_string(),
        SetupJson::Array(entry.args.iter().map(|a| s(a)).collect()),
    );
    if format == SetupFormat::ClaudeCodeJson {
        m.insert(
            "env".to_string(),
            SetupJson::Object(indexmap::IndexMap::new()),
        );
    }
    SetupJson::Object(m)
}

/// Our entry in `doc`, if any. Errors when the servers key holds a non-object.
fn setup_json_existing(
    doc: &SetupJson,
    key: &str,
) -> std::result::Result<Option<SetupJson>, String> {
    let SetupJson::Object(root) = doc else {
        return Err("the file isn't a JSON object".to_string());
    };
    match root.get(key) {
        None => Ok(None),
        Some(SetupJson::Object(servers)) => Ok(servers.get(SETUP_SERVER_NAME).cloned()),
        Some(_) => Err(format!("\"{key}\" isn't a JSON object")),
    }
}

/// Same command and args. A `type` we write may be missing from a
/// hand-written entry, but when present it must be ours; keys we don't write
/// (Zed's retired "source") are ignored.
fn setup_json_matches(existing: &SetupJson, format: SetupFormat, entry: &SetupEntry) -> bool {
    let (SetupJson::Object(have), SetupJson::Object(want)) =
        (existing, setup_json_entry_value(format, entry))
    else {
        return false;
    };
    let same = |k: &str| have.get(k) == want.get(k);
    let type_ok = !want.contains_key("type") || !have.contains_key("type") || same("type");
    same("command") && same("args") && type_ok
}

fn setup_json_command(entry: &SetupJson) -> String {
    match entry {
        SetupJson::Object(m) => match m.get("command") {
            Some(SetupJson::String(c)) => c.clone(),
            Some(other) => setup_json_inline(other),
            None => "(no command)".to_string(),
        },
        other => setup_json_inline(other),
    }
}

fn setup_json_edit(
    text: Option<&str>,
    format: SetupFormat,
    op: SetupOp,
    entry: &SetupEntry,
) -> std::result::Result<SetupEdit, String> {
    let key = setup_json_key(format);
    let mut doc = match text {
        Some(t) => setup_json_parse(t)?,
        None => SetupJson::Object(indexmap::IndexMap::new()),
    };
    let existing = setup_json_existing(&doc, key)?;
    let before = existing.as_ref().map(setup_json_inline);

    match op {
        SetupOp::Connect => {
            if let Some(ex) = &existing {
                if setup_json_matches(ex, format, entry) {
                    return Ok(SetupEdit::unchanged(before.clone(), before));
                }
            }
            // Keep fields the user added to our entry (env, disabled, timeouts);
            // ours win where both exist, and an existing env is left alone.
            let merged = match (&existing, setup_json_entry_value(format, entry)) {
                (Some(SetupJson::Object(old)), SetupJson::Object(new)) => {
                    let mut m = old.clone();
                    for (k, v) in new {
                        if k == "env" && m.contains_key("env") {
                            continue;
                        }
                        m.insert(k, v);
                    }
                    SetupJson::Object(m)
                }
                (_, new) => new,
            };
            let after = setup_json_inline(&merged);
            if let SetupJson::Object(root) = &mut doc {
                let servers = root
                    .entry(key.to_string())
                    .or_insert_with(|| SetupJson::Object(indexmap::IndexMap::new()));
                if let SetupJson::Object(servers) = servers {
                    servers.insert(SETUP_SERVER_NAME.to_string(), merged);
                }
            }
            let change = match &existing {
                Some(ex) => SetupChange::Update(setup_json_command(ex)),
                None => SetupChange::Add,
            };
            Ok(SetupEdit {
                change,
                before,
                after: Some(after),
                new_text: Some(setup_json_render(&doc, text)?),
            })
        }
        SetupOp::Remove => {
            if existing.is_none() {
                return Ok(SetupEdit::unchanged(None, None));
            }
            if let SetupJson::Object(root) = &mut doc {
                if let Some(SetupJson::Object(servers)) = root.get_mut(key) {
                    servers.shift_remove(SETUP_SERVER_NAME);
                }
            }
            Ok(SetupEdit {
                change: SetupChange::Remove,
                before,
                after: None,
                new_text: Some(setup_json_render(&doc, text)?),
            })
        }
    }
}

// ── Codex TOML ──────────────────────────────────────────────────────────────

fn setup_toml_args(entry: &SetupEntry) -> toml_edit::Array {
    let mut args = toml_edit::Array::new();
    for a in &entry.args {
        args.push(a.as_str());
    }
    args
}

/// Codex's config.toml through toml_edit, which keeps comments, ordering and
/// formatting of everything we don't touch.
fn setup_toml_edit(
    text: Option<&str>,
    op: SetupOp,
    entry: &SetupEntry,
) -> std::result::Result<SetupEdit, String> {
    use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

    let mut doc: DocumentMut = text
        .unwrap_or("")
        .parse()
        .map_err(|e: toml_edit::TomlError| {
            format!(
                "the file isn't valid TOML ({})",
                e.to_string().lines().next().unwrap_or("parse error").trim()
            )
        })?;
    let existing: Option<Item> = match doc.get("mcp_servers") {
        None => None,
        Some(item) => match item.as_table_like() {
            Some(servers) => servers.get(SETUP_SERVER_NAME).cloned(),
            None => return Err("\"mcp_servers\" isn't a table".to_string()),
        },
    };
    let before = existing.as_ref().map(setup_toml_render_entry);

    match op {
        SetupOp::Connect => {
            if let Some(ex) = &existing {
                if setup_toml_matches(ex, entry) {
                    return Ok(SetupEdit::unchanged(before.clone(), before));
                }
            }
            if doc.get("mcp_servers").is_none() {
                // Implicit: renders as [mcp_servers.market-search] alone, with
                // no empty [mcp_servers] header above it.
                let mut t = Table::new();
                t.set_implicit(true);
                doc.insert("mcp_servers", Item::Table(t));
            }
            let servers_inline = doc.get("mcp_servers").is_some_and(|i| i.is_inline_table());
            let servers = doc
                .get_mut("mcp_servers")
                .and_then(|i| i.as_table_like_mut())
                .ok_or("\"mcp_servers\" isn't a table")?;
            match servers
                .get_mut(SETUP_SERVER_NAME)
                .and_then(|i| i.as_table_like_mut())
            {
                // Update in place: keeps env, startup_timeout_sec, comments.
                Some(ours) => {
                    ours.insert("command", toml_edit::value(entry.command.as_str()));
                    ours.insert("args", toml_edit::value(setup_toml_args(entry)));
                }
                None if servers_inline => {
                    let mut t = InlineTable::new();
                    t.insert("command", Value::from(entry.command.as_str()));
                    t.insert("args", Value::Array(setup_toml_args(entry)));
                    servers.insert(SETUP_SERVER_NAME, Item::Value(Value::InlineTable(t)));
                }
                None => {
                    let mut t = Table::new();
                    t.insert("command", toml_edit::value(entry.command.as_str()));
                    t.insert("args", toml_edit::value(setup_toml_args(entry)));
                    servers.insert(SETUP_SERVER_NAME, Item::Table(t));
                }
            }
            let after = servers.get(SETUP_SERVER_NAME).map(setup_toml_render_entry);
            let change = match &existing {
                Some(ex) => SetupChange::Update(setup_toml_command(ex)),
                None => SetupChange::Add,
            };
            Ok(SetupEdit {
                change,
                before,
                after,
                new_text: Some(doc.to_string()),
            })
        }
        SetupOp::Remove => {
            if existing.is_none() {
                return Ok(SetupEdit::unchanged(None, None));
            }
            if let Some(servers) = doc
                .get_mut("mcp_servers")
                .and_then(|i| i.as_table_like_mut())
            {
                servers.remove(SETUP_SERVER_NAME);
            }
            Ok(SetupEdit {
                change: SetupChange::Remove,
                before,
                after: None,
                new_text: Some(doc.to_string()),
            })
        }
    }
}

fn setup_toml_matches(item: &toml_edit::Item, entry: &SetupEntry) -> bool {
    let Some(t) = item.as_table_like() else {
        return false;
    };
    let command_ok = t.get("command").and_then(|v| v.as_str()) == Some(entry.command.as_str());
    let args_ok = match t.get("args").and_then(|v| v.as_array()) {
        Some(a) => {
            a.len() == entry.args.len()
                && a.iter()
                    .zip(&entry.args)
                    .all(|(x, y)| x.as_str() == Some(y.as_str()))
        }
        None => entry.args.is_empty(),
    };
    command_ok && args_ok
}

fn setup_toml_command(item: &toml_edit::Item) -> String {
    item.as_table_like()
        .and_then(|t| t.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or("(no command)")
        .to_string()
}

/// A `[mcp_servers.market-search]` table as it reads in config.toml.
fn setup_toml_render_table(entry: toml_edit::Table) -> String {
    let mut servers = toml_edit::Table::new();
    servers.set_implicit(true);
    servers.insert(SETUP_SERVER_NAME, toml_edit::Item::Table(entry));
    let mut doc = toml_edit::DocumentMut::new();
    doc.insert("mcp_servers", toml_edit::Item::Table(servers));
    doc.to_string().trim().to_string()
}

fn setup_toml_render_entry(item: &toml_edit::Item) -> String {
    let mut entry = toml_edit::Table::new();
    if let Some(t) = item.as_table_like() {
        for (k, v) in t.iter() {
            let mut v = v.clone();
            if let Some(val) = v.as_value_mut() {
                val.decor_mut().clear();
            }
            entry.insert(k, v);
        }
    }
    setup_toml_render_table(entry)
}

/// The edit for any file-backed format.
fn setup_edit(
    format: SetupFormat,
    text: Option<&str>,
    op: SetupOp,
    entry: &SetupEntry,
) -> std::result::Result<SetupEdit, String> {
    match format {
        SetupFormat::CodexToml => setup_toml_edit(text, op, entry),
        json => setup_json_edit(text, json, op, entry),
    }
}

/// The command our entry points at in `client`'s config, if connected.
fn setup_client_state(client: &SetupClient) -> std::result::Result<Option<String>, String> {
    let Some(text) = setup_read_config(&client.config)? else {
        return Ok(None);
    };
    if client.format == SetupFormat::CodexToml {
        let doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|_| "the file isn't valid TOML".to_string())?;
        return Ok(doc
            .get("mcp_servers")
            .and_then(|i| i.as_table_like())
            .and_then(|t| t.get(SETUP_SERVER_NAME))
            .map(setup_toml_command));
    }
    let doc = setup_json_parse(&text)?;
    Ok(setup_json_existing(&doc, setup_json_key(client.format))?
        .as_ref()
        .map(setup_json_command))
}

// ── applying edits ──────────────────────────────────────────────────────────

/// One client's outcome: planned (nothing written) or applied.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SetupStatus {
    NotFound,
    /// Connect: already points at this binary.
    Current,
    /// Remove: no entry to remove.
    Absent,
    Added,
    /// Re-pointed; holds the old command.
    Updated(String),
    Removed,
    /// Setup won't touch the file; the row carries a snippet to paste.
    Manual(String),
    Failed(String),
}

#[derive(Clone, Debug)]
struct SetupRow {
    client: SetupClient,
    status: SetupStatus,
    before: Option<String>,
    after: Option<String>,
    /// The command that applies the change when it isn't a file edit.
    via: Option<String>,
    /// What to paste by hand (Manual / Failed).
    snippet: Option<String>,
    /// The one-time backup this run created.
    backup: Option<PathBuf>,
    note: Option<String>,
}

impl SetupRow {
    fn new(client: &SetupClient) -> Self {
        Self {
            client: client.clone(),
            status: SetupStatus::NotFound,
            before: None,
            after: None,
            via: None,
            snippet: None,
            backup: None,
            note: None,
        }
    }

    fn changed(&self) -> bool {
        matches!(
            self.status,
            SetupStatus::Added | SetupStatus::Updated(_) | SetupStatus::Removed
        )
    }
}

fn setup_status_for(change: &SetupChange, op: SetupOp) -> SetupStatus {
    match (change, op) {
        (SetupChange::Nothing, SetupOp::Connect) => SetupStatus::Current,
        (SetupChange::Nothing, SetupOp::Remove) => SetupStatus::Absent,
        (SetupChange::Add, _) => SetupStatus::Added,
        (SetupChange::Update(from), _) => SetupStatus::Updated(from.clone()),
        (SetupChange::Remove, _) => SetupStatus::Removed,
    }
}

fn setup_read_config(path: &Path) -> std::result::Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            Err("the file isn't UTF-8 text".to_string())
        }
        Err(e) => Err(format!("it can't be read ({e})")),
    }
}

/// Plan (`write` = false) or apply one client's change. Applying re-reads the
/// file, so an app that rewrote its config since the plan isn't clobbered.
fn setup_client_run(
    env: &SetupEnv,
    client: &SetupClient,
    op: SetupOp,
    entry: &SetupEntry,
    write: bool,
) -> SetupRow {
    let mut row = SetupRow::new(client);
    if client.found.is_none() {
        return row;
    }
    if client.format == SetupFormat::ClaudeCodeJson {
        return setup_claude_code_run(env, row, op, entry, write);
    }
    let edit = setup_read_config(&client.config)
        .and_then(|text| setup_edit(client.format, text.as_deref(), op, entry));
    match edit {
        Ok(edit) => setup_apply_edit(row, edit, op, write),
        Err(why) => {
            row.status = SetupStatus::Manual(why);
            row.snippet = Some(setup_snippet(env, client, op, entry));
            row
        }
    }
}

fn setup_apply_edit(mut row: SetupRow, edit: SetupEdit, op: SetupOp, write: bool) -> SetupRow {
    row.status = setup_status_for(&edit.change, op);
    row.before = edit.before;
    row.after = edit.after;
    if let (true, Some(text)) = (write, edit.new_text) {
        match setup_write_config(&row.client.config, &text) {
            Ok(backup) => row.backup = backup,
            Err(e) => row.status = SetupStatus::Failed(format!("{e:#}")),
        }
    }
    row
}

/// `path` + `suffix`, on the raw OsStr (no UTF-8 assumption).
fn setup_suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn setup_backup_path(path: &Path) -> PathBuf {
    setup_suffixed(path, ".market-search.bak")
}

/// Replace `path` with `contents`: one-time backup of the original, then a
/// temp file + rename so a crash never leaves half a config. The original's
/// permissions carry over (~/.claude.json is 0600). A symlinked config
/// (dotfile managers) is written through to its target and stays a link.
/// Returns the backup this call created, if any.
fn setup_write_config(path: &Path, contents: &str) -> Result<Option<PathBuf>> {
    let is_link = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    let target = if is_link {
        std::fs::canonicalize(path)
            .with_context(|| format!("resolve symlink {}", path.display()))?
    } else {
        path.to_path_buf()
    };
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .context("config path has no parent dir")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let original = std::fs::metadata(&target).ok();
    let mut backup = None;
    if original.is_some() {
        let bak = setup_backup_path(path);
        if !bak.exists() {
            std::fs::copy(&target, &bak)
                .with_context(|| format!("back up {} to {}", target.display(), bak.display()))?;
            backup = Some(bak);
        }
    }

    let tmp = setup_suffixed(&target, ".market-search.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        // Create with the final mode so a 0600 file is never briefly readable.
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        opts.mode(
            original
                .as_ref()
                .map_or(0o644, |m| m.permissions().mode() & 0o777),
        );
    }
    let written = (|| -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        drop(f);
        if let Some(m) = &original {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        std::fs::rename(&tmp, &target)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", target.display()));
    }
    Ok(backup)
}

// ── Claude Code ─────────────────────────────────────────────────────────────

/// One `claude` invocation. `may_fail`: a cleanup step whose failure is fine.
struct SetupCliStep {
    args: Vec<String>,
    may_fail: bool,
}

fn setup_claude_add_step(entry: &SetupEntry) -> SetupCliStep {
    let mut args: Vec<String> = ["mcp", "add", "--scope", "user", SETUP_SERVER_NAME, "--"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.push(entry.command.clone());
    args.extend(entry.args.iter().cloned());
    SetupCliStep {
        args,
        may_fail: false,
    }
}

fn setup_claude_remove_step(may_fail: bool) -> SetupCliStep {
    SetupCliStep {
        args: ["mcp", "remove", "--scope", "user", SETUP_SERVER_NAME]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        may_fail,
    }
}

/// `claude <args>` as a shell line, for showing what runs.
fn setup_claude_line(step: &SetupCliStep) -> String {
    std::iter::once("claude".to_string())
        .chain(step.args.iter().map(|a| setup_shell_word(a)))
        .join(" ")
}

/// Claude Code: the `claude` CLI owns ~/.claude.json, which the running app
/// rewrites constantly, so prefer `claude mcp add/remove`. State is read from
/// ~/.claude.json (read-only) so re-runs are idempotent. Without the CLI, or
/// when it fails, edit the user-scope "mcpServers" in ~/.claude.json directly.
fn setup_claude_code_run(
    env: &SetupEnv,
    mut row: SetupRow,
    op: SetupOp,
    entry: &SetupEntry,
    write: bool,
) -> SetupRow {
    let path = row.client.config.clone();
    let shown = env.show(&path);
    let edit = setup_read_config(&path)
        .and_then(|text| setup_json_edit(text.as_deref(), SetupFormat::ClaudeCodeJson, op, entry));

    let Some(cli) = row.client.cli.clone() else {
        // Detected by ~/.claude.json alone.
        return match edit {
            Ok(edit) => {
                let mut r = setup_apply_edit(row, edit, op, write);
                if r.changed() {
                    r.note = Some(format!(
                        "no `claude` CLI found, so {shown} is edited directly"
                    ));
                }
                r
            }
            Err(why) => {
                row.status = SetupStatus::Manual(why);
                row.snippet = Some(setup_snippet(env, &row.client, op, entry));
                row
            }
        };
    };

    let steps: Vec<SetupCliStep> = match &edit {
        Ok(e) => {
            row.status = setup_status_for(&e.change, op);
            row.before = e.before.clone();
            row.after = e.after.clone();
            match (&e.change, op) {
                (SetupChange::Nothing, _) => return row,
                // `claude mcp add` refuses a name that exists: remove, then add.
                (SetupChange::Update(_), _) => {
                    vec![
                        setup_claude_remove_step(false),
                        setup_claude_add_step(entry),
                    ]
                }
                (_, SetupOp::Connect) => vec![setup_claude_add_step(entry)],
                (_, SetupOp::Remove) => vec![setup_claude_remove_step(false)],
            }
        }
        // Can't see the current state; let the CLI sort it out.
        Err(why) => {
            row.note = Some(format!("couldn't read {shown}: {why}"));
            match op {
                SetupOp::Connect => {
                    row.status = SetupStatus::Added;
                    vec![setup_claude_remove_step(true), setup_claude_add_step(entry)]
                }
                SetupOp::Remove => {
                    row.status = SetupStatus::Removed;
                    vec![setup_claude_remove_step(false)]
                }
            }
        }
    };
    row.via = Some(steps.iter().map(setup_claude_line).join(" && "));
    if !write {
        return row;
    }

    let mut cli_error = None;
    for step in &steps {
        let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
        if let Err(e) = setup_run_cli(&cli, &args, SETUP_CLI_TIMEOUT) {
            if !step.may_fail {
                cli_error = Some(e);
                break;
            }
        }
    }
    let Some(cli_error) = cli_error else {
        return row;
    };
    // The CLI failed (say, an npm stub a GUI app can't exec): fall back to the
    // file when it parses.
    match edit {
        Ok(e) if e.new_text.is_some() => {
            let mut r = setup_apply_edit(row, e, op, true);
            if !matches!(r.status, SetupStatus::Failed(_)) {
                r.note = Some(format!(
                    "`claude mcp` failed ({cli_error}), so {shown} was edited directly"
                ));
            }
            r
        }
        _ => {
            row.status = SetupStatus::Failed(cli_error);
            row.snippet = Some(setup_snippet(env, &row.client, op, entry));
            row
        }
    }
}

/// Run a CLI with stdin closed and a deadline. The program's own dir goes
/// first on PATH: a double-clicked app inherits a bare PATH, and a Node-based
/// CLI finds `node` next to itself.
fn setup_run_cli(
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> std::result::Result<(), String> {
    use std::io::Read;

    let mut dirs: Vec<PathBuf> = program
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    if cfg!(target_os = "macos") {
        for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
            let p = PathBuf::from(extra);
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Ok(joined) = std::env::join_paths(&dirs) {
        cmd.env("PATH", joined);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't run {}: {e}", program.display()))?;

    // Drain both pipes on threads so a chatty child can't block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_string(&mut s);
            }
            s
        })
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let detail = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_string();
    Err(format!("exit {}: {detail}", status.code().unwrap_or(-1)))
}

// ── messages ────────────────────────────────────────────────────────────────

/// A word for a shell line: quoted only when it needs to be.
fn setup_shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// What to paste when setup won't edit a file itself.
fn setup_snippet(env: &SetupEnv, client: &SetupClient, op: SetupOp, entry: &SetupEntry) -> String {
    let file = env.show(&client.config);
    if op == SetupOp::Remove {
        return match client.format {
            SetupFormat::ClaudeCodeJson => {
                format!(
                    "Run: {}",
                    setup_claude_line(&setup_claude_remove_step(false))
                )
            }
            SetupFormat::CodexToml => {
                format!("Delete the [mcp_servers.{SETUP_SERVER_NAME}] table from {file}.")
            }
            f => format!(
                "Delete the \"{SETUP_SERVER_NAME}\" entry under \"{}\" in {file}.",
                setup_json_key(f)
            ),
        };
    }
    match client.format {
        SetupFormat::ClaudeCodeJson => {
            format!("Run: {}", setup_claude_line(&setup_claude_add_step(entry)))
        }
        SetupFormat::CodexToml => {
            let mut t = toml_edit::Table::new();
            t.insert("command", toml_edit::value(entry.command.as_str()));
            t.insert("args", toml_edit::value(setup_toml_args(entry)));
            format!("Add to {file}:\n\n{}", setup_toml_render_table(t))
        }
        f => {
            let key = setup_json_key(f);
            let mut servers = indexmap::IndexMap::new();
            servers.insert(
                SETUP_SERVER_NAME.to_string(),
                setup_json_entry_value(f, entry),
            );
            let fragment =
                serde_json::to_string_pretty(&SetupJson::Object(servers)).unwrap_or_default();
            format!(
                "Add inside the top-level {{ }} of {file} (merge into \"{key}\" if it's already there):\n\n\"{key}\": {fragment}"
            )
        }
    }
}

/// The plain JSON block most MCP clients accept, for when no app was found.
fn setup_generic_snippet(entry: &SetupEntry) -> String {
    let mut servers = indexmap::IndexMap::new();
    servers.insert(
        SETUP_SERVER_NAME.to_string(),
        setup_json_entry_value(SetupFormat::McpServers, entry),
    );
    let mut root = indexmap::IndexMap::new();
    root.insert("mcpServers".to_string(), SetupJson::Object(servers));
    serde_json::to_string_pretty(&SetupJson::Object(root)).unwrap_or_default()
}

fn setup_planned(status: &SetupStatus) -> String {
    match status {
        SetupStatus::NotFound => "not installed".to_string(),
        SetupStatus::Current => "already connected".to_string(),
        SetupStatus::Absent => "not connected".to_string(),
        SetupStatus::Added => "connect".to_string(),
        SetupStatus::Updated(from) => format!("re-point (now {from})"),
        SetupStatus::Removed => "remove".to_string(),
        SetupStatus::Manual(why) => format!("needs a manual step: {why}"),
        SetupStatus::Failed(e) => format!("can't: {e}"),
    }
}

fn setup_done(status: &SetupStatus) -> String {
    match status {
        SetupStatus::NotFound => "not installed".to_string(),
        SetupStatus::Current => "already connected".to_string(),
        SetupStatus::Absent => "not connected".to_string(),
        SetupStatus::Added => "connected".to_string(),
        SetupStatus::Updated(from) => format!("re-pointed (was {from})"),
        SetupStatus::Removed => "removed".to_string(),
        SetupStatus::Manual(why) => format!("left alone: {why}"),
        SetupStatus::Failed(e) => format!("failed: {e}"),
    }
}

/// How to get a client to load a config change, as a clause.
fn setup_restart_phrase(env: &SetupEnv, client: &SetupClient) -> String {
    match client.id {
        SetupClientId::ClaudeDesktop => match env.os {
            SetupOs::Mac => "quit Claude Desktop fully (Cmd+Q) and reopen it".to_string(),
            SetupOs::Windows => "quit Claude Desktop from its tray icon and reopen it".to_string(),
            SetupOs::Linux => "quit Claude Desktop fully and reopen it".to_string(),
        },
        SetupClientId::ClaudeCode | SetupClientId::Codex | SetupClientId::Gemini => {
            format!("start a new {} session", client.name)
        }
        _ => format!("restart {}", client.name),
    }
}

/// One plain sentence about a row, for people (the JSON "message").
fn setup_row_message(env: &SetupEnv, row: &SetupRow, dry_run: bool) -> String {
    let name = row.client.name;
    let restart = row.client.id.restart_required();
    let load = |done: String| {
        if restart {
            format!(
                "{done}; {} to load market-search.",
                setup_restart_phrase(env, &row.client)
            )
        } else {
            format!("{done}; {name} picks it up without a restart.")
        }
    };
    match (&row.status, dry_run) {
        (SetupStatus::NotFound, _) => format!("{name} isn't installed on this machine."),
        (SetupStatus::Current, _) => "Already connected to this binary.".to_string(),
        (SetupStatus::Absent, _) => "Not connected, so there is nothing to remove.".to_string(),
        (SetupStatus::Added, true) => "Would connect market-search.".to_string(),
        (SetupStatus::Added, false) => load("Connected".to_string()),
        (SetupStatus::Updated(from), true) => {
            format!("Would re-point it from {from} to this binary.")
        }
        (SetupStatus::Updated(from), false) => load(format!("Re-pointed from {from}")),
        (SetupStatus::Removed, true) => "Would remove market-search.".to_string(),
        (SetupStatus::Removed, false) if restart => format!(
            "Removed; {} to unload it.",
            setup_restart_phrase(env, &row.client)
        ),
        (SetupStatus::Removed, false) => "Removed.".to_string(),
        (SetupStatus::Manual(why), _) => {
            format!("Left alone because {why}; add the entry by hand.")
        }
        (SetupStatus::Failed(e), _) => format!("Failed: {e}"),
    }
}

/// Indent every line of `text` after the first by `pad` spaces.
fn setup_hang(text: &str, pad: usize) -> String {
    text.lines().join(&format!("\n{}", " ".repeat(pad)))
}

fn setup_not_found_line(env: &SetupEnv, client: &SetupClient) -> String {
    format!(
        "  {:<18}not installed (looked for {})",
        client.name,
        client.probes.iter().map(|p| env.show(p)).join(", ")
    )
}

/// "A", "A and B", "A, B and C".
fn setup_and_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Whether Claude Desktop is running (macOS only; elsewhere unknown = false).
fn setup_claude_desktop_running(env: &SetupEnv) -> bool {
    env.os == SetupOs::Mac
        && cfg!(target_os = "macos")
        && std::process::Command::new("/usr/bin/pgrep")
            .args(["-x", "Claude"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

fn setup_restart_hints(env: &SetupEnv, rows: &[SetupRow], op: SetupOp) -> Vec<String> {
    let mut hints = Vec::new();
    let mut later: Vec<&str> = Vec::new();
    for r in rows.iter().filter(|r| r.changed()) {
        match r.client.id {
            SetupClientId::ClaudeDesktop => {
                let mut h = format!("Claude Desktop: {}.", setup_restart_phrase(env, &r.client));
                if op == SetupOp::Connect && setup_claude_desktop_running(env) {
                    h.push_str(" If market-search is missing after that, run setup again while it's closed.");
                }
                hints.push(h);
            }
            SetupClientId::ClaudeCode | SetupClientId::Codex | SetupClientId::Gemini => {
                hints.push(format!("{}: new sessions see the change.", r.client.name));
            }
            id if id.restart_required() => later.push(r.client.name),
            _ => {}
        }
    }
    if !later.is_empty() {
        hints.push(format!(
            "If {} {} open, restart {}.",
            setup_and_list(&later),
            if later.len() == 1 { "is" } else { "are" },
            if later.len() == 1 { "it" } else { "them" }
        ));
    }
    hints
}

// ── SEC contact ─────────────────────────────────────────────────────────────

/// `--sec-contact`: "Name email" is kept as given; a bare email becomes
/// "Market Search <email>". SEC EDGAR wants a contact email in the User-Agent.
fn setup_sec_contact_value(raw: &str) -> Result<String> {
    let words: Vec<&str> = raw.split_whitespace().collect();
    if !words.iter().any(|w| setup_looks_like_email(w)) {
        anyhow::bail!(
            "the SEC contact needs an email address, e.g. \"jane@example.com\" or \"Jane Doe jane@example.com\""
        );
    }
    Ok(match words.as_slice() {
        [email] => format!("Market Search {email}"),
        _ => words.join(" "),
    })
}

fn setup_looks_like_email(s: &str) -> bool {
    let Some((user, domain)) = s.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !s.chars().any(char::is_whitespace)
}

/// Set by ELI_SEC_USER_AGENT or the `sec_user_agent` config key.
fn setup_sec_contact_is_set() -> bool {
    std::env::var("ELI_SEC_USER_AGENT").is_ok_and(|v| !v.trim().is_empty())
        || Paths::discover()
            .ok()
            .and_then(|p| config::load_or_default(&p).ok())
            .and_then(|c| c.chat.sec_user_agent)
            .is_some_and(|v| !v.trim().is_empty())
}

/// Store the contact as the `sec_user_agent` config key (what
/// `market-search config --set sec_user_agent` writes).
fn setup_store_sec_contact(value: &str) -> Result<()> {
    let paths = Paths::discover().context("find the market-search config")?;
    // A config that doesn't parse stops here rather than being overwritten.
    let mut cfg = config::load_or_default(&paths).context("read the market-search config")?;
    cfg.chat.sec_user_agent = Some(value.to_string());
    config::save(&paths, &cfg).context("save the market-search config")?;
    Ok(())
}

/// The one optional interactive question: the SEC EDGAR contact that
/// finance_filings and finance_insider need.
fn setup_offer_sec_contact() {
    let Ok(paths) = Paths::discover() else {
        return;
    };
    let Ok(cfg) = config::load_or_default(&paths) else {
        return;
    };
    if cfg
        .chat
        .sec_user_agent
        .as_deref()
        .is_some_and(|v| !v.trim().is_empty())
    {
        return;
    }
    let from_shell = std::env::var("ELI_SEC_USER_AGENT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let value = match from_shell {
        // AI apps launch the server without this shell's environment.
        Some(v) => {
            let question = format!(
                "ELI_SEC_USER_AGENT is set in this shell, but AI apps don't see shell variables. Save \"{v}\" for them too?"
            );
            if !setup_confirm(&question) {
                return;
            }
            v
        }
        None => {
            print!("\nEmail for SEC filings tools (optional, press Enter to skip): ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
                return;
            }
            match setup_sec_contact_value(&line) {
                Ok(v) => v,
                Err(_) => {
                    println!(
                        "That doesn't look like an email, so it was skipped. Set it any time:"
                    );
                    println!("  market-search config --set sec_user_agent --value \"Your Name you@example.com\"");
                    return;
                }
            }
        }
    };
    match setup_store_sec_contact(&value) {
        Ok(()) => println!("Saved. The SEC filings and insider tools are on."),
        Err(e) => println!("Couldn't save it: {e:#}"),
    }
}

// ── `market-search setup` ───────────────────────────────────────────────────

async fn cmd_setup(args: SetupArgs) -> Result<()> {
    if args.gui {
        return cmd_setup_gui(args).await;
    }
    if args.json {
        // stdout carries exactly one JSON object, errors included.
        return match setup_run_json(&args) {
            Ok((out, failed)) => {
                println!("{out}");
                if failed > 0 {
                    anyhow::bail!("{failed} app(s) could not be updated");
                }
                Ok(())
            }
            Err(e) => {
                let err = json!({ "error": format!("{e:#}") });
                println!("{}", serde_json::to_string_pretty(&err).unwrap_or_default());
                Err(e)
            }
        };
    }
    setup_run_human(&args)
}

fn setup_run_human(args: &SetupArgs) -> Result<()> {
    let env = SetupEnv::from_process()?;
    let only = setup_parse_clients(&args.client)?;
    let sec_contact = args
        .sec_contact
        .as_deref()
        .map(setup_sec_contact_value)
        .transpose()?;
    let clients = setup_select_clients(&env, only.as_deref());
    let op = if args.remove {
        SetupOp::Remove
    } else {
        SetupOp::Connect
    };
    let exe = std::env::current_exe().context("locate the running market-search binary")?;
    let binary = setup_resolve_binary(&env, &exe, args.binary.as_deref())?;
    let entry = SetupEntry::for_binary(&binary.target);

    if args.list {
        setup_print_list(&env, &clients, &binary, &entry);
        return Ok(());
    }

    let plan: Vec<SetupRow> = clients
        .iter()
        .map(|c| setup_client_run(&env, c, op, &entry, false))
        .collect();
    let copy_binary = setup_needs_copy(&binary, op, &plan);
    if args.dry_run {
        setup_print_dry_run(&env, &plan, &binary, copy_binary, op);
        return Ok(());
    }

    // `curl ... | sh` and agents' shells have no terminal to answer a prompt.
    let interactive = !args.yes && std::io::stdin().is_terminal();
    if let Some(value) = &sec_contact {
        setup_store_sec_contact(value)?;
        println!("Saved the SEC contact: {value}");
    }
    let found: Vec<&SetupRow> = plan
        .iter()
        .filter(|r| r.status != SetupStatus::NotFound)
        .collect();
    let missing_named: Vec<&SetupRow> = if only.is_some() {
        plan.iter()
            .filter(|r| r.status == SetupStatus::NotFound)
            .collect()
    } else {
        Vec::new()
    };

    if found.is_empty() {
        for r in &missing_named {
            println!("{}", setup_not_found_line(&env, &r.client));
        }
        if op == SetupOp::Remove {
            println!("market-search isn't connected to any app here.");
        } else {
            println!("No supported AI app found on this machine.");
            println!(
                "Install Claude Desktop ({SETUP_CLAUDE_DOWNLOAD_URL}), then run `market-search setup` again."
            );
            println!(
                "\nOr add this to your app's MCP config by hand:\n\n{}",
                setup_generic_snippet(&entry)
            );
        }
        return Ok(());
    }

    let any_change = copy_binary || found.iter().any(|r| r.changed());
    let show_plan = interactive || !any_change;
    if show_plan {
        if copy_binary {
            println!(
                "This binary runs from {}, so setup copies it to {}{} and points apps there.\n",
                binary.copy_reason.unwrap_or("a temporary location"),
                env.show(&binary.target),
                setup_replacing_note(&binary)
            );
        }
        println!("Found:");
        for r in &found {
            let via_cli = r.via.is_some() && r.changed();
            println!(
                "  {:<18}{}{}",
                r.client.name,
                setup_planned(&r.status),
                if via_cli { " (via the claude CLI)" } else { "" }
            );
        }
        for r in &missing_named {
            println!("{}", setup_not_found_line(&env, &r.client));
        }
    }
    if !any_change {
        println!("\nNothing to change.");
        setup_print_snippets(&found);
        if interactive && op == SetupOp::Connect && sec_contact.is_none() {
            setup_offer_sec_contact();
        }
        return Ok(());
    }
    if interactive {
        let question = match op {
            SetupOp::Connect => "Connect market-search to these?",
            SetupOp::Remove => "Remove market-search from these?",
        };
        if !setup_confirm(question) {
            println!("Nothing changed.");
            return Ok(());
        }
    }

    if copy_binary {
        setup_install_binary(&env, &binary.current, &binary.target)?;
        println!("Copied the binary to {}", env.show(&binary.target));
    }
    let results: Vec<SetupRow> = clients
        .iter()
        .filter(|c| c.found.is_some())
        .map(|c| setup_client_run(&env, c, op, &entry, true))
        .collect();
    setup_print_results(&env, &results, op);
    if !show_plan {
        for r in &missing_named {
            println!("{}", setup_not_found_line(&env, &r.client));
        }
    }

    if interactive && op == SetupOp::Connect && sec_contact.is_none() {
        setup_offer_sec_contact();
    }
    let failed = results
        .iter()
        .filter(|r| matches!(r.status, SetupStatus::Failed(_)))
        .count();
    if failed > 0 {
        anyhow::bail!(
            "{failed} app{} could not be updated (see above)",
            if failed == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn setup_print_list(
    env: &SetupEnv,
    clients: &[SetupClient],
    binary: &SetupBinary,
    entry: &SetupEntry,
) {
    println!("Binary for configs: {}", env.show(&binary.target));
    if let Some(reason) = binary.copy_reason {
        println!(
            "  (this one, {}, runs from {reason}; setup copies it there first)",
            env.show(&binary.current)
        );
    }
    println!();
    for c in clients {
        if c.found.is_none() {
            println!("  {:<18}not found", c.name);
            continue;
        }
        let state = match setup_client_state(c) {
            Ok(Some(cmd)) if cmd == entry.command => format!("connected: {cmd}"),
            Ok(Some(cmd)) => format!("connected: {cmd} (setup would re-point it)"),
            Ok(None) => "not connected".to_string(),
            Err(why) => format!("unknown: {why}"),
        };
        println!("  {:<18}installed  {state}", c.name);
        let via = match &c.cli {
            Some(cli) => format!(" (via the claude CLI at {})", env.show(cli)),
            None => String::new(),
        };
        println!("  {:<18}{}{via}", "", env.show(&c.config));
    }
}

fn setup_print_dry_run(
    env: &SetupEnv,
    plan: &[SetupRow],
    binary: &SetupBinary,
    copy_binary: bool,
    op: SetupOp,
) {
    println!("Dry run: nothing is written.\n");
    if op == SetupOp::Connect {
        if copy_binary {
            println!(
                "Binary: would copy {} to {}{} ({} isn't a stable location)\n",
                env.show(&binary.current),
                env.show(&binary.target),
                setup_replacing_note(binary),
                binary.copy_reason.unwrap_or("its folder")
            );
        } else {
            println!("Binary: {}\n", env.show(&binary.target));
        }
    }
    let mut not_found = Vec::new();
    for r in plan {
        if r.status == SetupStatus::NotFound {
            not_found.push(r.client.name);
            continue;
        }
        println!("{}: {}", r.client.name, setup_planned(&r.status));
        if let Some(via) = &r.via {
            println!("  runs:   {}", setup_hang(via, 10));
        }
        println!("  file:   {}", env.show(&r.client.config));
        if r.changed() || r.status == SetupStatus::Current {
            let show = |t: &Option<String>| match t {
                Some(t) if r.client.format == SetupFormat::CodexToml => setup_hang(t, 10),
                Some(t) => format!("\"{SETUP_SERVER_NAME}\": {t}"),
                None => "(none)".to_string(),
            };
            println!("  before: {}", show(&r.before));
            println!("  after:  {}", show(&r.after));
        }
        if let Some(note) = &r.note {
            println!("  note:   {note}");
        }
        if let Some(snippet) = &r.snippet {
            println!("  {}", setup_hang(snippet, 2));
        }
        println!();
    }
    if !not_found.is_empty() {
        println!("Not installed: {}", not_found.join(", "));
    }
}

fn setup_print_results(env: &SetupEnv, rows: &[SetupRow], op: SetupOp) {
    println!();
    for r in rows {
        let where_ = if r.changed() {
            match (&r.via, &r.note) {
                (Some(_), None) => "  (claude mcp, user scope)".to_string(),
                _ => format!("  {}", env.show(&r.client.config)),
            }
        } else {
            String::new()
        };
        println!("  {:<18}{}{where_}", r.client.name, setup_done(&r.status));
        if let Some(note) = &r.note {
            println!("  {:<18}{note}", "");
        }
    }
    if rows.iter().any(|r| r.backup.is_some()) {
        println!("\nOriginals are backed up next to each file as *.market-search.bak.");
    }
    setup_print_snippets(&rows.iter().collect::<Vec<_>>());
    let hints = setup_restart_hints(env, rows, op);
    if !hints.is_empty() {
        println!();
        for h in hints {
            println!("{h}");
        }
    }
}

fn setup_print_snippets(rows: &[&SetupRow]) {
    for r in rows {
        if let Some(snippet) = &r.snippet {
            println!("\n{}: {snippet}", r.client.name);
        }
    }
}

/// `[Y/n]` on stdin. EOF counts as no.
fn setup_confirm(question: &str) -> bool {
    print!("\n{question} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes"),
    }
}

// ── --json: the machine interface (the Mac app drives setup through it) ────

#[derive(Serialize)]
struct SetupJsonList {
    binary: String,
    sec_user_agent_set: bool,
    clients: Vec<SetupJsonClient>,
}

#[derive(Serialize)]
struct SetupJsonClient {
    id: &'static str,
    name: &'static str,
    detected: bool,
    /// An entry named market-search exists (whatever it points at).
    connected: bool,
    config: Option<String>,
    /// The command our entry runs now.
    command: Option<String>,
    /// The app must restart (or start a new session) to load a change.
    restart_required: bool,
    /// Why the config couldn't be read (comments, invalid JSON).
    problem: Option<String>,
}

#[derive(Serialize)]
struct SetupJsonRun {
    binary: String,
    sec_user_agent_set: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    dry_run: Option<bool>,
    results: Vec<SetupJsonResult>,
}

#[derive(Serialize)]
struct SetupJsonResult {
    id: &'static str,
    name: &'static str,
    action: &'static str,
    config: Option<String>,
    message: String,
    restart_required: bool,
    /// What to paste by hand when setup left the file alone.
    snippet: Option<String>,
}

fn setup_json_client(client: &SetupClient) -> SetupJsonClient {
    let detected = client.found.is_some();
    let (command, problem) = if detected {
        match setup_client_state(client) {
            Ok(command) => (command, None),
            Err(why) => (None, Some(why)),
        }
    } else {
        (None, None)
    };
    SetupJsonClient {
        id: client.id.key(),
        name: client.name,
        detected,
        connected: command.is_some(),
        config: detected.then(|| client.config.display().to_string()),
        command,
        restart_required: client.id.restart_required(),
        problem,
    }
}

fn setup_json_action(status: &SetupStatus) -> &'static str {
    match status {
        SetupStatus::NotFound => "not_found",
        SetupStatus::Current | SetupStatus::Absent => "unchanged",
        SetupStatus::Added => "added",
        SetupStatus::Updated(_) => "updated",
        SetupStatus::Removed => "removed",
        SetupStatus::Manual(_) => "skipped",
        SetupStatus::Failed(_) => "failed",
    }
}

fn setup_json_result(env: &SetupEnv, row: &SetupRow, dry_run: bool) -> SetupJsonResult {
    SetupJsonResult {
        id: row.client.id.key(),
        name: row.client.name,
        action: setup_json_action(&row.status),
        config: row
            .client
            .found
            .is_some()
            .then(|| row.client.config.display().to_string()),
        message: setup_row_message(env, row, dry_run),
        restart_required: row.changed() && row.client.id.restart_required(),
        snippet: row.snippet.clone(),
    }
}

/// `--json`: the work as one JSON object (plus how many apps failed). Never
/// prompts; human-readable progress goes to stderr.
fn setup_run_json(args: &SetupArgs) -> Result<(String, usize)> {
    let env = SetupEnv::from_process()?;
    let only = setup_parse_clients(&args.client)?;
    let sec_contact = args
        .sec_contact
        .as_deref()
        .map(setup_sec_contact_value)
        .transpose()?;
    let clients = setup_select_clients(&env, only.as_deref());
    let exe = std::env::current_exe().context("locate the running market-search binary")?;
    let binary = setup_resolve_binary(&env, &exe, args.binary.as_deref())?;
    let entry = SetupEntry::for_binary(&binary.target);
    let binary_path = binary.target.display().to_string();

    if args.list {
        let out = SetupJsonList {
            binary: binary_path,
            sec_user_agent_set: setup_sec_contact_is_set(),
            clients: clients.iter().map(setup_json_client).collect(),
        };
        return Ok((serde_json::to_string_pretty(&out)?, 0));
    }
    if !args.yes && !args.dry_run && std::io::stdin().is_terminal() {
        anyhow::bail!(
            "--json never asks questions: add --yes to make the changes, or use --list or --dry-run to look first"
        );
    }

    let op = if args.remove {
        SetupOp::Remove
    } else {
        SetupOp::Connect
    };
    let plan: Vec<SetupRow> = clients
        .iter()
        .map(|c| setup_client_run(&env, c, op, &entry, false))
        .collect();
    let rows = if args.dry_run {
        plan
    } else {
        if let Some(value) = &sec_contact {
            setup_store_sec_contact(value)?;
            eprintln!("saved the SEC contact: {value}");
        }
        if setup_needs_copy(&binary, op, &plan) {
            setup_install_binary(&env, &binary.current, &binary.target)?;
            eprintln!(
                "copied {} to {}",
                binary.current.display(),
                binary.target.display()
            );
        }
        clients
            .iter()
            .map(|c| setup_client_run(&env, c, op, &entry, true))
            .collect()
    };
    let failed = rows
        .iter()
        .filter(|r| matches!(r.status, SetupStatus::Failed(_)))
        .count();
    let out = SetupJsonRun {
        binary: binary_path,
        sec_user_agent_set: setup_sec_contact_is_set(),
        dry_run: args.dry_run.then_some(true),
        results: rows
            .iter()
            .map(|r| setup_json_result(&env, r, args.dry_run))
            .collect(),
    };
    Ok((serde_json::to_string_pretty(&out)?, failed))
}

// ── macOS app: the same flow through dialogs ────────────────────────────────

/// True when macOS launched this binary as an app's main executable with no
/// arguments: a double-click (older macOS adds a `-psn_*` serial number).
fn setup_launched_as_app() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    if std::env::args_os()
        .skip(1)
        .any(|a| !a.to_string_lossy().starts_with("-psn_"))
    {
        return false;
    }
    std::env::current_exe()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

/// Plain-text record of a dialog run. A double-clicked app has no terminal,
/// so ~/Library/Logs/market-search-setup.log is where "what happened" lives.
struct SetupLog {
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl SetupLog {
    fn open(env: &SetupEnv) -> Self {
        let path = env
            .home
            .join("Library")
            .join("Logs")
            .join("market-search-setup.log");
        let file = path
            .parent()
            .and_then(|d| std::fs::create_dir_all(d).ok())
            .and_then(|_| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .ok()
            });
        Self { path, file }
    }

    fn line(&mut self, msg: &str) {
        if let Some(f) = self.file.as_mut() {
            let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
            for l in msg.lines() {
                let _ = writeln!(f, "{ts} {l}");
            }
        }
    }
}

async fn cmd_setup_gui(args: SetupArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!(
            "`setup --gui` uses macOS dialogs; run `market-search setup` in a terminal instead"
        );
    }
    let env = SetupEnv::from_process()?;
    let mut log = SetupLog::open(&env);
    log.line(&format!(
        "market-search {} setup (dialogs), running {}",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    ));
    let result = setup_gui_flow(&env, &args, &mut log);
    if let Err(e) = &result {
        log.line(&format!("error: {e:#}"));
        let message = format!("{e:#}\n\nDetails are in {}.", env.show(&log.path));
        let _ = setup_dialog_alert("Market Search couldn't finish setup", &message);
    }
    log.line("end");
    result
}

fn setup_gui_flow(env: &SetupEnv, args: &SetupArgs, log: &mut SetupLog) -> Result<()> {
    let only = setup_parse_clients(&args.client)?;
    let exe = std::env::current_exe().context("locate the running market-search binary")?;
    let binary = setup_resolve_binary(env, &exe, args.binary.as_deref())?;
    let entry = SetupEntry::for_binary(&binary.target);
    match binary.copy_reason {
        Some(reason) => log.line(&format!(
            "binary runs from {reason}; configs will point at {}",
            binary.target.display()
        )),
        None => log.line(&format!(
            "configs will point at {}",
            binary.target.display()
        )),
    }

    let clients: Vec<SetupClient> = setup_select_clients(env, only.as_deref())
        .into_iter()
        .filter(|c| c.found.is_some())
        .collect();
    if clients.is_empty() {
        log.line("no supported app found");
        let message = "Market Search didn't find an AI app it can connect to on this Mac.\n\nInstall Claude Desktop, then open Market Search again.";
        if setup_dialog(
            message,
            &["Close", "Get Claude Desktop"],
            "Get Claude Desktop",
        )?
        .as_deref()
            == Some("Get Claude Desktop")
        {
            log.line(&format!("opening {SETUP_CLAUDE_DOWNLOAD_URL}"));
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(SETUP_CLAUDE_DOWNLOAD_URL)
                .status();
        }
        return Ok(());
    }

    let plan: Vec<SetupRow> = clients
        .iter()
        .map(|c| setup_client_run(env, c, SetupOp::Connect, &entry, false))
        .collect();
    for r in &plan {
        log.line(&format!(
            "found {}: {}",
            r.client.name,
            setup_planned(&r.status)
        ));
        if let Some(snippet) = &r.snippet {
            log.line(&format!("  {snippet}"));
        }
    }
    let copy_binary = setup_needs_copy(&binary, SetupOp::Connect, &plan);
    let names_where = |pick: &dyn Fn(&SetupStatus) -> bool| -> Vec<&str> {
        plan.iter()
            .filter(|r| pick(&r.status))
            .map(|r| r.client.name)
            .collect()
    };
    let current = names_where(&|s| *s == SetupStatus::Current);
    // Files setup won't edit stay out of the checklist; the summary names them.
    let offer = names_where(&|s| matches!(s, SetupStatus::Added | SetupStatus::Updated(_)));
    let manual: Vec<&SetupRow> = plan
        .iter()
        .filter(|r| matches!(r.status, SetupStatus::Manual(_)))
        .collect();

    if offer.is_empty() && !copy_binary {
        let mut message = if current.is_empty() {
            "Market Search found nothing it can connect automatically.".to_string()
        } else {
            format!(
                "Market Search is already connected to {}.",
                setup_and_list(&current)
            )
        };
        for r in &manual {
            message.push_str(&setup_gui_manual_line(env, r, &log.path));
        }
        setup_dialog(&message, &["Done"], "Done")?;
        return Ok(());
    }

    let picked: Vec<String> = if offer.is_empty() {
        // Nothing new to connect, but the stable copy is out of date.
        Vec::new()
    } else {
        let mut prompt = "Market Search found these apps on your Mac. Connect it to:".to_string();
        if !current.is_empty() {
            prompt.push_str(&format!("\n\n(Already connected: {}.)", current.join(", ")));
        }
        match setup_dialog_choose(&prompt, &offer)? {
            Some(picked) => picked,
            None => {
                log.line("cancelled");
                return Ok(());
            }
        }
    };
    log.line(&format!("connecting: {}", picked.join(", ")));

    if copy_binary {
        setup_install_binary(env, &binary.current, &binary.target)?;
        log.line(&format!(
            "copied {} to {}",
            binary.current.display(),
            binary.target.display()
        ));
    }
    let rows: Vec<SetupRow> = clients
        .iter()
        .filter(|c| picked.iter().any(|p| p == c.name))
        .map(|c| setup_client_run(env, c, SetupOp::Connect, &entry, true))
        .collect();
    for r in &rows {
        log.line(&format!(
            "{}: {} ({})",
            r.client.name,
            setup_done(&r.status),
            r.client.config.display()
        ));
        for extra in [&r.note, &r.snippet].into_iter().flatten() {
            log.line(&format!("  {extra}"));
        }
    }

    let desktop = rows
        .iter()
        .find(|r| r.client.id == SetupClientId::ClaudeDesktop && r.changed())
        .map(|r| r.client.clone());
    let mut message = setup_gui_summary(&rows, &current);
    if copy_binary && !rows.iter().any(|r| r.changed()) {
        message = format!(
            "Updated Market Search at {}.\n\n{message}",
            env.show(&binary.target)
        );
    }
    for r in &manual {
        message.push_str(&setup_gui_manual_line(env, r, &log.path));
    }
    let buttons: &[&str] = if desktop.is_some() {
        &["Done", "Restart Claude"]
    } else {
        &["Done"]
    };
    let pressed = setup_dialog(&message, buttons, buttons[buttons.len() - 1])?;
    if let (Some("Restart Claude"), Some(client)) = (pressed.as_deref(), &desktop) {
        setup_restart_claude_desktop(env, client, &entry, log);
    }
    setup_gui_after_connect(env, &rows, log)
}

/// Hook for steps after the result dialog. A later "Set up phone access" step
/// (a claude.ai connector reaching this machine) plugs in here, with what got
/// connected and the log. Today there is nothing to do.
fn setup_gui_after_connect(_env: &SetupEnv, _rows: &[SetupRow], _log: &mut SetupLog) -> Result<()> {
    Ok(())
}

fn setup_gui_manual_line(env: &SetupEnv, row: &SetupRow, log_path: &Path) -> String {
    let why = match &row.status {
        SetupStatus::Manual(why) => why.as_str(),
        _ => "it couldn't be edited",
    };
    format!(
        "\n\n{} wasn't changed: {why}. The lines to add by hand are in {}.",
        row.client.name,
        env.show(log_path)
    )
}

fn setup_gui_summary(rows: &[SetupRow], current: &[&str]) -> String {
    let connected: Vec<&str> = rows
        .iter()
        .filter(|r| r.changed())
        .map(|r| r.client.name)
        .collect();
    let mut already: Vec<&str> = current.to_vec();
    already.extend(
        rows.iter()
            .filter(|r| r.status == SetupStatus::Current)
            .map(|r| r.client.name),
    );

    let mut parts = Vec::new();
    if !connected.is_empty() {
        parts.push(format!(
            "Connected Market Search to {}.",
            setup_and_list(&connected)
        ));
    }
    if !already.is_empty() {
        parts.push(format!("Already connected: {}.", already.join(", ")));
    }
    for r in rows {
        if let SetupStatus::Failed(e) = &r.status {
            parts.push(format!("{} failed: {e}", r.client.name));
        }
    }
    if rows
        .iter()
        .any(|r| r.client.id == SetupClientId::ClaudeDesktop && r.changed())
    {
        parts.push("Quit and reopen Claude Desktop to load it.".to_string());
    }
    if parts.is_empty() {
        parts.push("Nothing changed.".to_string());
    }
    parts.join("\n\n")
}

/// Quit Claude Desktop, wait for it to exit, check our entry survived, and
/// open it again. (Claude Desktop has been seen rewriting its config from
/// what it loaded at launch; quitting is when that would bite.)
fn setup_restart_claude_desktop(
    env: &SetupEnv,
    client: &SetupClient,
    entry: &SetupEntry,
    log: &mut SetupLog,
) {
    if setup_claude_desktop_running(env) {
        log.line("quitting Claude Desktop");
        let _ = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", "quit app \"Claude\""])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(20);
        while setup_claude_desktop_running(env) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
        }
        if setup_claude_desktop_running(env) {
            log.line("Claude Desktop is still running; asked the user to restart it");
            let _ = setup_dialog(
                "Claude is still open. Quit it (Cmd+Q) and open it again to load Market Search.",
                &["OK"],
                "OK",
            );
            return;
        }
    }
    let row = setup_client_run(env, client, SetupOp::Connect, entry, true);
    if row.status != SetupStatus::Current {
        log.line(&format!(
            "Claude Desktop config after quit: {}",
            setup_done(&row.status)
        ));
    }
    log.line("opening Claude Desktop");
    let _ = std::process::Command::new("/usr/bin/open")
        .args(["-a", "Claude"])
        .status();
}

/// Run AppleScript through osascript (Standard Additions only, no System
/// Events). Text reaches the script as `argv`, never spliced into its source,
/// so quotes and backslashes in messages can't break it. Ok(None) = the user
/// cancelled.
fn setup_osascript(body: &[&str], argv: &[&str]) -> Result<Option<String>> {
    let mut cmd = std::process::Command::new("/usr/bin/osascript");
    // The app has no Dock icon (LSUIElement), so bring the dialog forward.
    for line in ["on run argv", "tell me to activate"]
        .iter()
        .chain(body)
        .chain(["end run"].iter())
    {
        cmd.arg("-e").arg(line);
    }
    cmd.args(argv);
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .context("run /usr/bin/osascript")?;
    if out.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        ));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("-128") {
        return Ok(None);
    }
    anyhow::bail!("osascript: {}", stderr.trim())
}

/// A message with buttons; the button pressed (None = cancelled).
fn setup_dialog(message: &str, buttons: &[&str], default: &str) -> Result<Option<String>> {
    let list = buttons.iter().map(|b| format!("\"{b}\"")).join(", ");
    let show = format!(
        "set r to display dialog (item 1 of argv) with title \"{SETUP_DIALOG_TITLE}\" buttons {{{list}}} default button \"{default}\" with icon note"
    );
    setup_osascript(&[&show, "return button returned of r"], &[message])
}

/// A checklist with every item preselected; the items kept (None = cancelled).
fn setup_dialog_choose(prompt: &str, items: &[&str]) -> Result<Option<Vec<String>>> {
    let choose = format!(
        "set picked to choose from list appNames with title \"{SETUP_DIALOG_TITLE}\" with prompt (item 1 of argv) default items appNames OK button name \"Connect\" cancel button name \"Cancel\" with multiple selections allowed"
    );
    let body = [
        "set appNames to items 2 thru -1 of argv",
        choose.as_str(),
        "if picked is false then return \"\"",
        "set AppleScript's text item delimiters to linefeed",
        "return picked as text",
    ];
    let mut argv = vec![prompt];
    argv.extend_from_slice(items);
    Ok(match setup_osascript(&body, &argv)? {
        Some(out) if !out.trim().is_empty() => Some(
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    })
}

fn setup_dialog_alert(title: &str, message: &str) -> Result<()> {
    setup_osascript(
        &["display alert (item 1 of argv) message (item 2 of argv) as critical buttons {\"OK\"} default button \"OK\""],
        &[title, message],
    )?;
    Ok(())
}

#[cfg(test)]
mod setup_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A temp dir removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "market-search-setup-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_env(root: &Path, os: SetupOs) -> SetupEnv {
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        SetupEnv {
            os,
            appdata: root.join("appdata"),
            local_appdata: root.join("localappdata"),
            xdg_config: home.join(".config"),
            app_dirs: vec![root.join("Applications")],
            path_dirs: vec![root.join("bin")],
            claude_config_dir: None,
            codex_home: None,
            gemini_cli_home: None,
            temp_dirs: vec![root.join("system-tmp")],
            home,
        }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    fn entry(path: &str) -> SetupEntry {
        SetupEntry::for_binary(Path::new(path))
    }

    fn installed(env: &SetupEnv, id: SetupClientId) -> SetupClient {
        let probe = setup_detect_client(env, id).probes[0].clone();
        std::fs::create_dir_all(&probe).unwrap();
        let client = setup_detect_client(env, id);
        assert!(client.found.is_some(), "{id:?} should be detected");
        client
    }

    const JSON_CLIENTS: [SetupClientId; 8] = [
        SetupClientId::ClaudeDesktop,
        SetupClientId::Cursor,
        SetupClientId::VsCode,
        SetupClientId::VsCodeInsiders,
        SetupClientId::Windsurf,
        SetupClientId::Gemini,
        SetupClientId::LmStudio,
        SetupClientId::Zed,
    ];

    #[test]
    fn json_clients_add_reuse_repoint_remove() {
        for id in JSON_CLIENTS {
            let dir = TestDir::new("json");
            let env = test_env(&dir.0, SetupOs::Mac);
            let client = installed(&env, id);
            let key = setup_json_key(client.format);
            // Keys deliberately out of alphabetical order, plus another server.
            let original = format!(
                "{{\n  \"zeta\": 1,\n  \"{key}\": {{\n    \"other\": {{\"command\": \"other-bin\", \"args\": []}}\n  }},\n  \"alpha\": {{\"nested\": [1, 2.5, null, true, \"é\"]}}\n}}\n"
            );
            std::fs::write(&client.config, &original).unwrap();

            let a = entry("/opt/a/market-search");
            let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
            assert_eq!(row.status, SetupStatus::Added, "{id:?}");
            let text = read(&client.config);
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            let ours = &v[key][SETUP_SERVER_NAME];
            assert_eq!(ours["command"], "/opt/a/market-search", "{id:?}");
            assert_eq!(ours["args"], json!(["mcp"]), "{id:?}");
            match id {
                SetupClientId::Cursor | SetupClientId::VsCode | SetupClientId::VsCodeInsiders => {
                    assert_eq!(ours["type"], "stdio", "{id:?}")
                }
                _ => assert!(ours.get("type").is_none(), "{id:?}"),
            }
            assert!(
                ours.get("source").is_none() && ours.get("env").is_none(),
                "{id:?}"
            );
            // Everything else survives, in its original order.
            assert_eq!(v[key]["other"]["command"], "other-bin");
            assert_eq!(v["zeta"], 1);
            assert_eq!(v["alpha"]["nested"], json!([1, 2.5, null, true, "é"]));
            let at = |s: &str| text.find(s).unwrap();
            assert!(
                at("\"zeta\"") < at(&format!("\"{key}\""))
                    && at(&format!("\"{key}\"")) < at("\"alpha\"")
            );
            assert!(at("\"other\"") < at(&format!("\"{SETUP_SERVER_NAME}\"")));
            assert!(text.ends_with("}\n") && text.contains("\n  \"zeta\": 1,\n"));
            let backup = setup_backup_path(&client.config);
            assert_eq!(read(&backup), original, "{id:?} backup");
            assert_eq!(row.backup.as_deref(), Some(backup.as_path()));

            // Same binary again: nothing to do, not a byte changes.
            let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
            assert_eq!(row.status, SetupStatus::Current, "{id:?}");
            assert_eq!(read(&client.config), text);

            // Moved binary: re-pointed; the backup stays the pre-setup original.
            let b = entry("/opt/b/market-search");
            let row = setup_client_run(&env, &client, SetupOp::Connect, &b, true);
            assert_eq!(
                row.status,
                SetupStatus::Updated("/opt/a/market-search".into()),
                "{id:?}"
            );
            assert!(row.backup.is_none());
            let v: serde_json::Value = serde_json::from_str(&read(&client.config)).unwrap();
            assert_eq!(v[key][SETUP_SERVER_NAME]["command"], "/opt/b/market-search");
            assert_eq!(read(&backup), original);

            // Remove takes only our entry.
            let row = setup_client_run(&env, &client, SetupOp::Remove, &b, true);
            assert_eq!(row.status, SetupStatus::Removed, "{id:?}");
            let after = read(&client.config);
            let v: serde_json::Value = serde_json::from_str(&after).unwrap();
            assert!(v[key].get(SETUP_SERVER_NAME).is_none());
            assert_eq!(v[key]["other"]["command"], "other-bin");
            assert_eq!(v["zeta"], 1);
            let row = setup_client_run(&env, &client, SetupOp::Remove, &b, true);
            assert_eq!(row.status, SetupStatus::Absent);
            assert_eq!(read(&client.config), after);
        }
    }

    #[test]
    fn json_clients_never_overwrite_unparseable_files() {
        let bad = [
            "{\n  // a comment\n  \"mcpServers\": {}\n}\n",
            "{\"mcpServers\": {},}\n",
            "[1, 2]\n",
            "{\"mcpServers\": [], \"servers\": [], \"context_servers\": []}\n",
        ];
        for id in JSON_CLIENTS {
            for original in bad {
                let dir = TestDir::new("badjson");
                let env = test_env(&dir.0, SetupOs::Mac);
                let client = installed(&env, id);
                std::fs::write(&client.config, original).unwrap();
                for op in [SetupOp::Connect, SetupOp::Remove] {
                    let row =
                        setup_client_run(&env, &client, op, &entry("/opt/a/market-search"), true);
                    assert!(
                        matches!(row.status, SetupStatus::Manual(_)),
                        "{id:?} {original:?}: {:?}",
                        row.status
                    );
                    assert!(row.snippet.is_some());
                }
                assert_eq!(read(&client.config), original);
                assert!(!setup_backup_path(&client.config).exists());
            }
        }
    }

    #[test]
    fn zed_snippet_and_retired_source_key() {
        let dir = TestDir::new("zed");
        let env = test_env(&dir.0, SetupOs::Mac);
        let client = installed(&env, SetupClientId::Zed);
        let a = entry("/opt/a/market-search");

        // Zed's default settings file has comments: left alone, with a snippet.
        std::fs::write(
            &client.config,
            "// Zed settings\n{\n  \"theme\": \"One Dark\",\n}\n",
        )
        .unwrap();
        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, false);
        let snippet = row.snippet.unwrap();
        assert!(snippet.contains("\"context_servers\": {"), "{snippet}");
        assert!(
            snippet.contains("\"command\": \"/opt/a/market-search\"")
                && !snippet.contains("source")
        );

        // An entry from before Zed retired "source" still counts as current.
        std::fs::write(
            &client.config,
            r#"{"context_servers": {"market-search": {"source": "custom", "command": "/opt/a/market-search", "args": ["mcp"]}}}"#,
        )
        .unwrap();
        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Current);
    }

    #[test]
    fn our_own_formatting_round_trips() {
        let dir = TestDir::new("roundtrip");
        let env = test_env(&dir.0, SetupOs::Mac);
        let client = installed(&env, SetupClientId::ClaudeDesktop);
        let original = "{\n  \"globalShortcut\": \"Ctrl+Space\",\n  \"mcpServers\": {\n    \"fs\": {\n      \"command\": \"npx\",\n      \"args\": [\n        \"-y\",\n        \"@modelcontextprotocol/server-filesystem\"\n      ],\n      \"env\": {}\n    }\n  },\n  \"big\": 12345678901234,\n  \"s\": \"tab\\tquote\\\" \\u001b é\"\n}\n";
        std::fs::write(&client.config, original).unwrap();
        let a = entry("/opt/a/market-search");
        setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        setup_client_run(&env, &client, SetupOp::Remove, &a, true);
        // Adding and removing our entry gives back the same bytes.
        assert_eq!(read(&client.config), original);
    }

    #[test]
    fn crlf_and_missing_final_newline_are_kept() {
        let dir = TestDir::new("crlf");
        let env = test_env(&dir.0, SetupOs::Mac);
        let client = installed(&env, SetupClientId::Cursor);
        std::fs::write(&client.config, "{\r\n  \"mcpServers\": {}\r\n}").unwrap();
        setup_client_run(
            &env,
            &client,
            SetupOp::Connect,
            &entry("/opt/a/market-search"),
            true,
        );
        let text = read(&client.config);
        assert!(text.contains("\r\n") && !text.replace("\r\n", "").contains('\n'));
        assert!(text.ends_with('}'));
    }

    #[test]
    fn detected_client_without_a_config_file_gets_one() {
        let dir = TestDir::new("newfile");
        let env = test_env(&dir.0, SetupOs::Mac);
        // Claude Desktop installed but never opened: no config dir yet.
        std::fs::create_dir_all(dir.0.join("Applications").join("Claude.app")).unwrap();
        let client = setup_detect_client(&env, SetupClientId::ClaudeDesktop);
        assert!(client.found.is_some() && !client.config.exists());
        let row = setup_client_run(
            &env,
            &client,
            SetupOp::Connect,
            &entry("/opt/a/market-search"),
            true,
        );
        assert_eq!(row.status, SetupStatus::Added);
        assert!(row.backup.is_none());
        assert_eq!(
            read(&client.config),
            "{\n  \"mcpServers\": {\n    \"market-search\": {\n      \"command\": \"/opt/a/market-search\",\n      \"args\": [\n        \"mcp\"\n      ]\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn undetected_clients_get_no_files() {
        let dir = TestDir::new("none");
        let env = test_env(&dir.0, SetupOs::Mac);
        for client in setup_detect_clients(&env) {
            assert!(client.found.is_none(), "{:?}", client.id);
            for op in [SetupOp::Connect, SetupOp::Remove] {
                let row = setup_client_run(&env, &client, op, &entry("/opt/a/market-search"), true);
                assert_eq!(row.status, SetupStatus::NotFound);
            }
        }
        assert_eq!(
            std::fs::read_dir(&env.home).unwrap().count(),
            0,
            "home is still empty"
        );
    }

    #[test]
    fn codex_toml_add_reuse_repoint_remove() {
        let dir = TestDir::new("codex");
        let env = test_env(&dir.0, SetupOs::Mac);
        let client = installed(&env, SetupClientId::Codex);
        let original = "# my codex config\nmodel = \"gpt-5\"  # inline comment\n\n[mcp_servers.other]\ncommand = \"other\"\nargs = [\"x\"]\n\n[profiles.fast]\nmodel = \"o4-mini\"\n";
        std::fs::write(&client.config, original).unwrap();

        let a = entry("/opt/a/market-search");
        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Added);
        let text = read(&client.config);
        // Our table lands next to its sibling server; every other byte stays.
        assert_eq!(
            text,
            original.replace(
                "args = [\"x\"]\n\n",
                "args = [\"x\"]\n\n[mcp_servers.market-search]\ncommand = \"/opt/a/market-search\"\nargs = [\"mcp\"]\n\n"
            )
        );
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["market-search"]["args"][0].as_str(),
            Some("mcp")
        );
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("other")
        );
        assert_eq!(read(&setup_backup_path(&client.config)), original);

        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Current);
        assert_eq!(read(&client.config), text);

        // A key the user added to our table survives a re-point.
        std::fs::write(
            &client.config,
            text.replace(
                "args = [\"mcp\"]\n",
                "args = [\"mcp\"]\nenv = { ELI_SEC_USER_AGENT = \"Jane j@x.com\" }\n",
            ),
        )
        .unwrap();
        let b = entry("/opt/b/market-search");
        let row = setup_client_run(&env, &client, SetupOp::Connect, &b, true);
        assert_eq!(
            row.status,
            SetupStatus::Updated("/opt/a/market-search".into())
        );
        let parsed: toml::Value = toml::from_str(&read(&client.config)).unwrap();
        let ours = &parsed["mcp_servers"]["market-search"];
        assert_eq!(ours["command"].as_str(), Some("/opt/b/market-search"));
        assert_eq!(
            ours["env"]["ELI_SEC_USER_AGENT"].as_str(),
            Some("Jane j@x.com")
        );

        let row = setup_client_run(&env, &client, SetupOp::Remove, &b, true);
        assert_eq!(row.status, SetupStatus::Removed);
        let text = read(&client.config);
        assert!(!text.contains("market-search"), "{text}");
        assert_eq!(text.trim_end(), original.trim_end());
        let row = setup_client_run(&env, &client, SetupOp::Remove, &b, true);
        assert_eq!(row.status, SetupStatus::Absent);
    }

    #[test]
    fn codex_toml_new_file_inline_table_and_invalid() {
        let dir = TestDir::new("codex2");
        let env = test_env(&dir.0, SetupOs::Mac);
        let client = installed(&env, SetupClientId::Codex);
        let a = entry("/opt/a/market-search");

        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Added);
        assert_eq!(
            read(&client.config),
            "[mcp_servers.market-search]\ncommand = \"/opt/a/market-search\"\nargs = [\"mcp\"]\n"
        );

        std::fs::write(
            &client.config,
            "mcp_servers = { other = { command = \"o\" } }\n",
        )
        .unwrap();
        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Added);
        let parsed: toml::Value = toml::from_str(&read(&client.config)).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["market-search"]["command"].as_str(),
            Some("/opt/a/market-search")
        );
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("o")
        );

        // Servers defined with dotted keys at the top level.
        std::fs::write(
            &client.config,
            "model = \"gpt-5\"\nmcp_servers.other.command = \"o\"\n",
        )
        .unwrap();
        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Added);
        let text = read(&client.config);
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["market-search"]["args"][0].as_str(),
            Some("mcp"),
            "{text}"
        );
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("o")
        );
        assert!(
            text.starts_with("model = \"gpt-5\"\nmcp_servers.other.command = \"o\"\n"),
            "{text}"
        );

        for bad in ["model = \n", "mcp_servers = \"nope\"\n"] {
            std::fs::write(&client.config, bad).unwrap();
            let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
            assert!(matches!(row.status, SetupStatus::Manual(_)), "{bad:?}");
            assert!(row.snippet.unwrap().contains("[mcp_servers.market-search]"));
            assert_eq!(read(&client.config), bad);
        }
    }

    #[test]
    fn claude_code_json_fallback_without_cli() {
        let dir = TestDir::new("claudejson");
        let env = test_env(&dir.0, SetupOs::Mac);
        let path = env.home.join(".claude.json");
        let original = "{\n  \"numStartups\": 412,\n  \"projects\": {\n    \"/Users/x/repo\": {\n      \"mcpServers\": {},\n      \"allowedTools\": []\n    }\n  },\n  \"mcpServers\": {\n    \"other\": {\n      \"type\": \"stdio\",\n      \"command\": \"o\",\n      \"args\": [],\n      \"env\": {}\n    }\n  },\n  \"userID\": \"abc\"\n}";
        std::fs::write(&path, original).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let client = setup_detect_client(&env, SetupClientId::ClaudeCode);
        assert_eq!(client.found.as_deref(), Some(path.as_path()));
        assert!(client.cli.is_none());

        let a = entry("/opt/a/market-search");
        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Added);
        assert!(row.via.is_none() && row.note.is_some());
        let text = read(&path);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["mcpServers"]["market-search"],
            json!({"type": "stdio", "command": "/opt/a/market-search", "args": ["mcp"], "env": {}})
        );
        assert_eq!(v["projects"]["/Users/x/repo"]["mcpServers"], json!({}));
        assert_eq!(v["numStartups"], 412);
        assert!(
            !text.ends_with('\n'),
            "Claude Code writes no final newline; keep it that way"
        );
        let at = |s: &str| text.find(s).unwrap();
        assert!(
            at("\"numStartups\"") < at("\"projects\"") && at("\"projects\"") < at("\"userID\"")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Current);
        let b = entry("/opt/b/market-search");
        let row = setup_client_run(&env, &client, SetupOp::Connect, &b, true);
        assert_eq!(
            row.status,
            SetupStatus::Updated("/opt/a/market-search".into())
        );
        let row = setup_client_run(&env, &client, SetupOp::Remove, &b, true);
        assert_eq!(row.status, SetupStatus::Removed);
        let v: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        assert!(v["mcpServers"].get("market-search").is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "o");
    }

    #[cfg(unix)]
    fn fake_claude(dir: &Path, exit_code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let cli = bin.join("claude");
        let log = dir.join("claude-calls.log");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\necho 'boom' >&2\nexit {exit_code}\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        log
    }

    #[cfg(unix)]
    #[test]
    fn claude_code_prefers_the_cli() {
        let dir = TestDir::new("claudecli");
        let env = test_env(&dir.0, SetupOs::Mac);
        let log = fake_claude(&dir.0, 0);
        let client = setup_detect_client(&env, SetupClientId::ClaudeCode);
        assert_eq!(
            client.cli.as_deref(),
            Some(dir.0.join("bin").join("claude").as_path())
        );
        let calls = || std::fs::read_to_string(&log).unwrap_or_default();

        let a = entry("/opt/my apps/market-search");
        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, false);
        assert_eq!(row.status, SetupStatus::Added);
        assert_eq!(
            row.via.as_deref(),
            Some("claude mcp add --scope user market-search -- '/opt/my apps/market-search' mcp")
        );
        assert_eq!(calls(), "", "planning runs nothing");

        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Added);
        assert_eq!(
            calls(),
            "mcp add --scope user market-search -- /opt/my apps/market-search mcp\n"
        );
        assert!(
            !env.home.join(".claude.json").exists(),
            "the CLI owns the file"
        );

        // What the CLI would have written: re-running is a no-op.
        let path = env.home.join(".claude.json");
        std::fs::write(&path, r#"{"mcpServers": {"market-search": {"type": "stdio", "command": "/opt/my apps/market-search", "args": ["mcp"], "env": {}}}}"#).unwrap();
        let row = setup_client_run(&env, &client, SetupOp::Connect, &a, true);
        assert_eq!(row.status, SetupStatus::Current);
        assert_eq!(calls().lines().count(), 1);

        let b = entry("/opt/b/market-search");
        let row = setup_client_run(&env, &client, SetupOp::Connect, &b, true);
        assert_eq!(
            row.status,
            SetupStatus::Updated("/opt/my apps/market-search".into())
        );
        let lines: Vec<String> = calls().lines().map(str::to_string).collect();
        assert_eq!(lines[1], "mcp remove --scope user market-search");
        assert_eq!(
            lines[2],
            "mcp add --scope user market-search -- /opt/b/market-search mcp"
        );

        let row = setup_client_run(&env, &client, SetupOp::Remove, &b, true);
        assert_eq!(row.status, SetupStatus::Removed);
        assert_eq!(
            calls().lines().last(),
            Some("mcp remove --scope user market-search")
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_code_falls_back_to_the_file_when_the_cli_fails() {
        let dir = TestDir::new("claudefail");
        let env = test_env(&dir.0, SetupOs::Mac);
        fake_claude(&dir.0, 1);
        let path = env.home.join(".claude.json");
        std::fs::write(&path, "{\"userID\": \"abc\"}").unwrap();
        let client = setup_detect_client(&env, SetupClientId::ClaudeCode);
        let row = setup_client_run(
            &env,
            &client,
            SetupOp::Connect,
            &entry("/opt/a/market-search"),
            true,
        );
        assert_eq!(row.status, SetupStatus::Added);
        assert!(
            row.note.as_deref().unwrap_or("").contains("boom"),
            "{:?}",
            row.note
        );
        let v: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(
            v["mcpServers"]["market-search"]["command"],
            "/opt/a/market-search"
        );
        assert_eq!(v["userID"], "abc");

        // No usable file either: a failure with the command to run by hand.
        std::fs::write(&path, "{ not json").unwrap();
        let row = setup_client_run(
            &env,
            &client,
            SetupOp::Connect,
            &entry("/opt/b/market-search"),
            true,
        );
        assert!(
            matches!(row.status, SetupStatus::Failed(_)),
            "{:?}",
            row.status
        );
        assert!(row
            .snippet
            .unwrap()
            .contains("claude mcp add --scope user market-search"));
        assert_eq!(read(&path), "{ not json");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_is_written_through() {
        let dir = TestDir::new("symlink");
        let env = test_env(&dir.0, SetupOs::Mac);
        let client = installed(&env, SetupClientId::Cursor);
        let real = dir.0.join("dotfiles").join("mcp.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "{\"mcpServers\": {}}\n").unwrap();
        std::os::unix::fs::symlink(&real, &client.config).unwrap();
        let row = setup_client_run(
            &env,
            &client,
            SetupOp::Connect,
            &entry("/opt/a/market-search"),
            true,
        );
        assert_eq!(row.status, SetupStatus::Added);
        assert!(std::fs::symlink_metadata(&client.config)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(read(&real).contains("/opt/a/market-search"));
        assert_eq!(
            read(&setup_backup_path(&client.config)),
            "{\"mcpServers\": {}}\n"
        );
    }

    #[test]
    fn ephemeral_binary_locations() {
        let mac = SetupEnv {
            os: SetupOs::Mac,
            home: PathBuf::from("/Users/u"),
            appdata: PathBuf::new(),
            local_appdata: PathBuf::new(),
            xdg_config: PathBuf::from("/Users/u/.config"),
            app_dirs: vec![
                PathBuf::from("/Applications"),
                PathBuf::from("/Users/u/Applications"),
            ],
            path_dirs: Vec::new(),
            claude_config_dir: None,
            codex_home: None,
            gemini_cli_home: None,
            temp_dirs: [
                "/var/folders/zz/abc/T",
                "/tmp",
                "/private/tmp",
                "/var/folders",
                "/private/var/folders",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
        };
        let eph = |env: &SetupEnv, p: &str| setup_ephemeral_reason(env, Path::new(p));
        for p in [
            "/Users/u/.npm/_npx/1a2b3c/node_modules/market-search/bin/market-search",
            "/Users/u/Library/Caches/pnpm/dlx/abc/node_modules/market-search/bin/market-search",
            "/private/var/folders/zz/abc/T/bunx-501-market-search@latest/node_modules/market-search/bin/market-search",
            "/var/folders/zz/abc/T/market-search",
            "/private/tmp/market-search",
            "/Volumes/Market Search/Market Search.app/Contents/MacOS/market-search",
            "/private/var/folders/zz/abc/T/AppTranslocation/9F1E/d/Market Search.app/Contents/MacOS/market-search",
            "/Users/u/Downloads/market-search",
            "/Users/u/downloads/Market Search.app/Contents/MacOS/market-search",
            "/Users/u/Desktop/market-search",
            "/Users/u/Documents/code/market-search/target/release/market-search",
            "/Users/u/Projects/Market Search.app/Contents/MacOS/market-search",
            "/Users/u/Projects/Market Search.app/Contents/Resources/market-search",
        ] {
            assert!(eph(&mac, p).is_some(), "{p} should be ephemeral");
        }
        for p in [
            "/Applications/Market Search.app/Contents/MacOS/market-search",
            "/Applications/Market Search.app/Contents/Helpers/market-search",
            "/Users/u/Applications/Market Search.app/Contents/MacOS/market-search",
            "/opt/homebrew/bin/market-search",
            "/usr/local/bin/market-search",
            "/Users/u/.cargo/bin/market-search",
            "/Users/u/.local/bin/market-search",
            "/opt/homebrew/lib/node_modules/market-search/bin/market-search",
            "/Users/u/code/market-search/target/release/market-search",
        ] {
            assert_eq!(eph(&mac, p), None, "{p} should be stable");
        }
        assert_eq!(
            eph(
                &mac,
                "/Volumes/Market Search/Market Search.app/Contents/MacOS/market-search"
            ),
            Some("a mounted disk image")
        );

        let linux = SetupEnv {
            os: SetupOs::Linux,
            home: PathBuf::from("/home/u"),
            app_dirs: Vec::new(),
            temp_dirs: vec![PathBuf::from("/tmp")],
            ..mac.clone()
        };
        assert!(eph(&linux, "/home/u/Downloads/market-search").is_some());
        assert!(eph(
            &linux,
            "/home/u/.cache/pnpm/dlx/x/node_modules/market-search/bin/market-search"
        )
        .is_some());
        assert!(eph(&linux, "/tmp/market-search").is_some());
        assert_eq!(eph(&linux, "/home/u/Documents/market-search"), None);
        assert_eq!(eph(&linux, "/Volumes/x/market-search"), None);
        assert_eq!(
            eph(&linux, "/home/linuxbrew/.linuxbrew/bin/market-search"),
            None
        );
    }

    #[test]
    fn resolve_binary_copies_only_ephemeral_ones() {
        let dir = TestDir::new("resolve");
        let env = test_env(&dir.0, SetupOs::Mac);
        let stable = env.home.join(".local").join("bin").join("market-search");

        let downloads = env.home.join("Downloads").join("market-search");
        let b = setup_resolve_binary(&env, &downloads, None).unwrap();
        assert_eq!(b.target, stable);
        assert_eq!(b.copy_reason, Some("the Downloads folder"));

        let cargo = env.home.join(".cargo").join("bin").join("market-search");
        let b = setup_resolve_binary(&env, &cargo, None).unwrap();
        assert_eq!((b.target.as_path(), b.copy_reason), (cargo.as_path(), None));

        // --binary is taken as given, but has to exist.
        assert!(setup_resolve_binary(&env, &cargo, Some(&downloads)).is_err());
        std::fs::create_dir_all(downloads.parent().unwrap()).unwrap();
        std::fs::write(&downloads, b"bin").unwrap();
        let b = setup_resolve_binary(&env, &cargo, Some(&downloads)).unwrap();
        assert_eq!(
            (b.target.as_path(), b.copy_reason),
            (downloads.as_path(), None)
        );

        let win = test_env(&dir.0, SetupOs::Windows);
        assert_eq!(
            setup_stable_binary_path(&win),
            dir.0
                .join("localappdata")
                .join("market-search")
                .join("market-search.exe")
        );
    }

    #[test]
    fn install_binary_copies_once() {
        let dir = TestDir::new("install");
        let env = test_env(&dir.0, SetupOs::Linux);
        let from = dir.0.join("Downloads").join("market-search");
        std::fs::create_dir_all(from.parent().unwrap()).unwrap();
        std::fs::write(&from, b"#!/bin/sh\necho v1\n").unwrap();
        let to = setup_stable_binary_path(&env);
        assert!(setup_install_binary(&env, &from, &to).unwrap());
        assert_eq!(std::fs::read(&to).unwrap(), b"#!/bin/sh\necho v1\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&to).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        assert!(
            !setup_install_binary(&env, &from, &to).unwrap(),
            "identical copy is left alone"
        );
        std::fs::write(&from, b"#!/bin/sh\necho v2\n").unwrap();
        assert!(setup_install_binary(&env, &from, &to).unwrap());
        assert_eq!(std::fs::read(&to).unwrap(), b"#!/bin/sh\necho v2\n");
        let leftovers = std::fs::read_dir(to.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temp files left behind");
    }

    #[cfg(unix)]
    #[test]
    fn homebrew_cellar_path_maps_to_the_bin_symlink() {
        let dir = TestDir::new("brew");
        let real = dir
            .0
            .join("brew/Cellar/market-search/0.4.0/bin/market-search");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, b"bin").unwrap();
        let link = dir.0.join("brew/bin/market-search");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../Cellar/market-search/0.4.0/bin/market-search", &link)
            .unwrap();
        assert_eq!(setup_unversion_cellar(&real), Some(link));
        assert_eq!(setup_unversion_cellar(&dir.0.join("brew/bin/other")), None);
    }

    #[test]
    fn client_names_parse() {
        let ids = setup_parse_clients(&[
            "claude-desktop".into(),
            "VS Code".into(),
            "cursor".into(),
            "Cursor".into(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            ids,
            vec![
                SetupClientId::ClaudeDesktop,
                SetupClientId::VsCode,
                SetupClientId::Cursor
            ]
        );
        assert_eq!(setup_parse_clients(&[]).unwrap(), None);
        for id in SETUP_CLIENTS {
            assert_eq!(SetupClientId::from_key(id.key()), Some(id));
        }
        assert_eq!(
            SetupClientId::from_key("gemini"),
            Some(SetupClientId::Gemini)
        );
        assert_eq!(
            SetupClientId::from_key("lmstudio"),
            Some(SetupClientId::LmStudio)
        );
        let err = setup_parse_clients(&["notepad".into()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("notepad") && err.contains("claude-code"),
            "{err}"
        );
    }

    #[test]
    fn platform_config_paths() {
        let dir = TestDir::new("paths");
        let r = &dir.0;
        let path = |os, id| setup_detect_client(&test_env(r, os), id).config;
        let home = r.join("home");
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::ClaudeDesktop),
            home.join("Library/Application Support/Claude/claude_desktop_config.json")
        );
        assert_eq!(
            path(SetupOs::Windows, SetupClientId::ClaudeDesktop),
            r.join("appdata")
                .join("Claude")
                .join("claude_desktop_config.json")
        );
        assert_eq!(
            path(SetupOs::Linux, SetupClientId::ClaudeDesktop),
            home.join(".config/Claude/claude_desktop_config.json")
        );
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::VsCode),
            home.join("Library/Application Support/Code/User/mcp.json")
        );
        assert_eq!(
            path(SetupOs::Linux, SetupClientId::VsCodeInsiders),
            home.join(".config/Code - Insiders/User/mcp.json")
        );
        assert_eq!(
            path(SetupOs::Windows, SetupClientId::VsCode),
            r.join("appdata").join("Code").join("User").join("mcp.json")
        );
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::Zed),
            home.join(".config/zed/settings.json")
        );
        assert_eq!(
            path(SetupOs::Windows, SetupClientId::Zed),
            r.join("appdata").join("Zed").join("settings.json")
        );
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::Windsurf),
            home.join(".config/devin/mcp_config.json")
        );
        assert_eq!(
            path(SetupOs::Windows, SetupClientId::Windsurf),
            r.join("appdata").join("devin").join("mcp_config.json")
        );
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::LmStudio),
            home.join(".lmstudio/mcp.json")
        );
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::Gemini),
            home.join(".gemini/settings.json")
        );
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::Cursor),
            home.join(".cursor/mcp.json")
        );
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::Codex),
            home.join(".codex/config.toml")
        );
        assert_eq!(
            path(SetupOs::Mac, SetupClientId::ClaudeCode),
            home.join(".claude.json")
        );

        let mut env = test_env(r, SetupOs::Mac);
        env.codex_home = Some(r.join("codex-home"));
        env.claude_config_dir = Some(r.join("claude-dir"));
        env.gemini_cli_home = Some(r.join("gemini-home"));
        assert_eq!(
            setup_detect_client(&env, SetupClientId::Codex).config,
            r.join("codex-home").join("config.toml")
        );
        assert_eq!(
            setup_detect_client(&env, SetupClientId::ClaudeCode).config,
            r.join("claude-dir").join(".claude.json")
        );
        assert_eq!(
            setup_detect_client(&env, SetupClientId::Gemini).config,
            r.join("gemini-home").join(".gemini").join("settings.json")
        );
    }

    #[test]
    fn config_location_quirks() {
        let dir = TestDir::new("quirks");
        let r = &dir.0;

        // Windsurf before the Devin rename keeps its old file; Devin wins once present.
        let env = test_env(r, SetupOs::Mac);
        std::fs::create_dir_all(env.home.join(".codeium/windsurf")).unwrap();
        let c = setup_detect_client(&env, SetupClientId::Windsurf);
        assert_eq!(
            (c.name, c.config.clone()),
            (
                "Windsurf",
                env.home.join(".codeium/windsurf/mcp_config.json")
            )
        );
        std::fs::create_dir_all(r.join("Applications/Devin.app")).unwrap();
        let c = setup_detect_client(&env, SetupClientId::Windsurf);
        assert_eq!(
            (c.name, c.config.clone()),
            (
                "Devin Desktop",
                env.home.join(".config/devin/mcp_config.json")
            )
        );

        // Claude Desktop from the Microsoft Store reads its per-package copy first.
        let win = test_env(r, SetupOs::Windows);
        let pkg = r.join("localappdata/Packages/Claude_pzs8sxrjxfjjc");
        std::fs::create_dir_all(&pkg).unwrap();
        let c = setup_detect_client(&win, SetupClientId::ClaudeDesktop);
        assert_eq!(c.found.as_deref(), Some(pkg.as_path()));
        assert_eq!(
            c.config,
            r.join("appdata")
                .join("Claude")
                .join("claude_desktop_config.json")
        );
        let virtualized = pkg.join("LocalCache/Roaming/Claude/claude_desktop_config.json");
        std::fs::create_dir_all(virtualized.parent().unwrap()).unwrap();
        std::fs::write(&virtualized, "{}").unwrap();
        assert_eq!(
            setup_detect_client(&win, SetupClientId::ClaudeDesktop).config,
            virtualized
        );

        // LM Studio follows its home pointer.
        let lms = r.join("elsewhere/lm-studio");
        std::fs::create_dir_all(&lms).unwrap();
        std::fs::write(
            env.home.join(".lmstudio-home-pointer"),
            format!("{}\n", lms.display()),
        )
        .unwrap();
        assert_eq!(
            setup_detect_client(&env, SetupClientId::LmStudio).config,
            lms.join("mcp.json")
        );

        // A non-empty legacy ~/.claude/.config.json still wins in Claude Code.
        std::fs::create_dir_all(env.home.join(".claude")).unwrap();
        std::fs::write(env.home.join(".claude/.config.json"), "{}").unwrap();
        assert_eq!(
            setup_detect_client(&env, SetupClientId::ClaudeCode).config,
            env.home.join(".claude/.config.json")
        );
    }

    #[test]
    fn json_output_rows() {
        let dir = TestDir::new("jsonout");
        let env = test_env(&dir.0, SetupOs::Mac);
        let desktop = installed(&env, SetupClientId::ClaudeDesktop);
        let zed = installed(&env, SetupClientId::Zed);
        std::fs::write(&zed.config, "// comment\n{}\n").unwrap();
        let a = entry("/opt/a/market-search");

        let c = setup_json_client(&desktop);
        assert_eq!(
            (c.id, c.detected, c.connected, c.restart_required),
            ("claude-desktop", true, false, true)
        );
        assert_eq!(c.config.as_deref(), Some(desktop.config.to_str().unwrap()));
        let row = setup_client_run(&env, &desktop, SetupOp::Connect, &a, true);
        let res = setup_json_result(&env, &row, false);
        assert_eq!((res.action, res.restart_required), ("added", true));
        assert_eq!(
            res.message,
            "Connected; quit Claude Desktop fully (Cmd+Q) and reopen it to load market-search."
        );
        let c = setup_json_client(&desktop);
        assert_eq!(
            (c.connected, c.command.as_deref()),
            (true, Some("/opt/a/market-search"))
        );
        let row = setup_client_run(&env, &desktop, SetupOp::Connect, &a, true);
        assert_eq!(setup_json_result(&env, &row, false).action, "unchanged");

        let c = setup_json_client(&zed);
        assert!(c.problem.is_some() && !c.connected && !c.restart_required);
        let row = setup_client_run(&env, &zed, SetupOp::Connect, &a, true);
        let res = setup_json_result(&env, &row, false);
        assert_eq!((res.action, res.restart_required), ("skipped", false));
        assert!(res.snippet.is_some());

        let cursor = setup_detect_client(&env, SetupClientId::Cursor);
        let c = setup_json_client(&cursor);
        assert_eq!((c.detected, c.config.as_deref()), (false, None));
        let res = setup_json_result(
            &env,
            &setup_client_run(&env, &cursor, SetupOp::Connect, &a, true),
            false,
        );
        assert_eq!(res.action, "not_found");

        let ids: Vec<&str> = SETUP_CLIENTS.iter().map(|c| c.key()).collect();
        assert_eq!(
            ids,
            [
                "claude-desktop",
                "claude-code",
                "cursor",
                "vscode",
                "vscode-insiders",
                "windsurf",
                "codex",
                "gemini-cli",
                "lm-studio",
                "zed"
            ]
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(
            setup_sec_contact_value("jane@example.com").unwrap(),
            "Market Search jane@example.com"
        );
        assert_eq!(
            setup_sec_contact_value("  Jane  Doe jane@example.com ").unwrap(),
            "Jane Doe jane@example.com"
        );
        assert!(setup_sec_contact_value("Jane Doe").is_err());
        assert!(setup_sec_contact_value("").is_err());
        assert!(setup_looks_like_email("jane@example.com"));
        assert!(!setup_looks_like_email("jane@localhost"));
        assert_eq!(setup_and_list(&["A"]), "A");
        assert_eq!(setup_and_list(&["A", "B", "C"]), "A, B and C");
        assert_eq!(setup_shell_word("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(setup_shell_word("/a b/it's"), "'/a b/it'\\''s'");
        assert_eq!(
            setup_app_bundle(Path::new(
                "/Volumes/M/Market Search.app/Contents/MacOS/market-search"
            )),
            Some(PathBuf::from("/Volumes/M/Market Search.app"))
        );
        assert_eq!(
            setup_app_bundle(Path::new("/usr/local/bin/market-search")),
            None
        );
    }
}
