// `market-search mcp service` — keep a claude.ai connector URL alive in the
// background. claude.ai (web and phone) only reaches an MCP server through a
// public HTTPS URL, so the person's own always-on Mac or Linux box runs the HTTP
// server plus a tunnel under launchd (macOS) or systemd --user (Linux), which
// survive reboots, crashes and closed terminals.
//
//   install    stable binary → token → tunnel agent ready → service file → load → URL
//   uninstall  unload and remove the service file (the token and binary stay)
//   status     service, local health, tunnel (via ngrok's local API), URL
//   url        https://<domain>/mcp/<token>
//
// The service process runs `market-search mcp share --service` (run_share_service):
// the HTTP server on 127.0.0.1 behind the token, a supervised tunnel child, a
// caffeinate assertion on macOS, and a small JSON state file holding the public
// URL. Providers live behind TunnelKind; Tailscale Funnel would be one more variant.

mod mcp_service {
    use super::*;
    use std::fmt::Write as _;
    use std::io::Read as _;

    const SERVICE_LABEL: &str = "com.eliterminal.market-search";
    const SYSTEMD_UNIT: &str = "market-search.service";
    const TOKEN_FILE_NAME: &str = "mcp-token";
    const STATE_FILE_NAME: &str = "mcp-service.json";
    const DEFAULT_SERVICE_PORT: u16 = 8485;
    const CLAUDE_CONNECTORS_URL: &str = "https://claude.ai/settings/connectors";
    const NGROK_AUTHTOKEN_URL: &str = "https://dashboard.ngrok.com/get-started/your-authtoken";
    const NGROK_DOWNLOAD_BASE: &str = "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable";
    /// How often the supervisor asks the tunnel agent whether the tunnel still exists.
    const TUNNEL_CHECK_EVERY: Duration = Duration::from_secs(60);

    /// TUNNEL_CHECK_EVERY, unless MARKET_SEARCH_TUNNEL_CHECK_SECS overrides it (for
    /// exercising the restart path in tests without waiting minutes).
    fn tunnel_check_every() -> Duration {
        std::env::var("MARKET_SEARCH_TUNNEL_CHECK_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|s| *s >= 1)
            .map(Duration::from_secs)
            .unwrap_or(TUNNEL_CHECK_EVERY)
    }
    /// How long `install` waits for the service to report a public URL.
    const URL_WAIT: Duration = Duration::from_secs(90);
    /// Exit code when install stops because a person has to supply something.
    const EXIT_NEEDS_INPUT: i32 = 3;

    /// Optional settings copied from the installing shell into the service, because
    /// launchd and systemd start it with an empty environment. MARKET_SEARCH_MCP_TOKEN
    /// is deliberately absent: the service reads the token file.
    const SERVICE_ENV_PASSTHROUGH: &[&str] = &[
        "ELI_HOME",
        "ELI_CONFIG",
        "ELI_INV_PATH",
        "ELI_SEC_USER_AGENT",
        "EIA_API_KEY",
        "FRED_API_KEY",
        "ELI_AUDIT_LOG",
        "ELI_AUDIT_ARCHIVE",
        "IBKR_HOST",
        "IBKR_PORT",
        "IBKR_CLIENT_ID",
        "IBKR_ACCOUNT",
        "IBKR_MARKET_DATA_TYPE",
        "IBKR_TIMEOUT_SECS",
    ];
    /// Passthrough values that are secrets: their presence makes the service file 0600.
    const SERVICE_ENV_SECRETS: &[&str] = &["EIA_API_KEY", "FRED_API_KEY"];

    // ── platform + paths ────────────────────────────────────────────────────────

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum ServiceOs {
        Macos,
        Linux,
    }

    impl ServiceOs {
        fn current() -> Option<Self> {
            if cfg!(target_os = "macos") {
                Some(Self::Macos)
            } else if cfg!(target_os = "linux") {
                Some(Self::Linux)
            } else {
                None
            }
        }

        /// The platform to act on: this machine's, or a `--platform` preview (dry run only).
        fn resolve(flag: Option<&str>, dry_run: bool) -> Result<Self> {
            let Some(flag) = flag else {
                return Self::current().ok_or_else(|| {
                    anyhow::anyhow!(
                        "`mcp service` supports macOS (launchd) and Linux (systemd --user). On other systems, \
                         run `market-search mcp --http` under your own service manager; see SELFHOST.md."
                    )
                });
            };
            let os = match flag.trim().to_ascii_lowercase().as_str() {
                "macos" | "mac" | "darwin" => Self::Macos,
                "linux" => Self::Linux,
                other => anyhow::bail!("unknown --platform '{other}': use macos or linux"),
            };
            if Some(os) != Self::current() && !dry_run {
                anyhow::bail!("--platform previews another platform's service file and only works with --dry-run");
            }
            Ok(os)
        }

        fn manager(self) -> &'static str {
            match self {
                Self::Macos => "launchd",
                Self::Linux => "systemd --user",
            }
        }
    }

    /// Every path the service touches. Built from an explicit home so tests can
    /// point it at a scratch directory.
    #[derive(Clone, Debug)]
    pub(super) struct ServiceLayout {
        os: ServiceOs,
        home: PathBuf,
        token_file: PathBuf,
        state_file: PathBuf,
        work_dir: PathBuf,
        stable_bin: PathBuf,
        service_file: PathBuf,
        log_file: PathBuf,
    }

    impl ServiceLayout {
        fn discover(os: ServiceOs) -> Result<Self> {
            let home = home_dir()?;
            let (config_dir, data_dir) = if Some(os) == ServiceOs::current() {
                let paths = Paths::discover().context("find the config directory")?;
                (paths.config_dir, paths.data_dir)
            } else {
                conventional_dirs(os, &home)
            };
            let env_dir = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
            Ok(Self::build(
                os,
                &home,
                &config_dir,
                &data_dir,
                env_dir("XDG_CONFIG_HOME"),
                env_dir("XDG_STATE_HOME"),
            ))
        }

        fn build(
            os: ServiceOs,
            home: &Path,
            config_dir: &Path,
            data_dir: &Path,
            xdg_config: Option<PathBuf>,
            xdg_state: Option<PathBuf>,
        ) -> Self {
            let (service_file, log_file) = match os {
                ServiceOs::Macos => (
                    home.join("Library/LaunchAgents").join(format!("{SERVICE_LABEL}.plist")),
                    home.join("Library/Logs/market-search.log"),
                ),
                ServiceOs::Linux => (
                    xdg_config
                        .unwrap_or_else(|| home.join(".config"))
                        .join("systemd/user")
                        .join(SYSTEMD_UNIT),
                    xdg_state
                        .unwrap_or_else(|| home.join(".local/state"))
                        .join("market-search/service.log"),
                ),
            };
            Self {
                os,
                home: home.to_path_buf(),
                token_file: config_dir.join(TOKEN_FILE_NAME),
                state_file: data_dir.join(STATE_FILE_NAME),
                work_dir: data_dir.to_path_buf(),
                stable_bin: home.join(".local/bin/market-search"),
                service_file,
                log_file,
            }
        }
    }

    /// Where `directories` puts eli's config and data on `os`, for previews of a
    /// platform other than this one. Mirrors Paths::discover, ELI_HOME included.
    fn conventional_dirs(os: ServiceOs, home: &Path) -> (PathBuf, PathBuf) {
        if let Some(base) = std::env::var_os("ELI_HOME").filter(|v| !v.is_empty()) {
            let base = PathBuf::from(base);
            return (base.join("config"), base.join("data"));
        }
        match os {
            ServiceOs::Macos => {
                let d = home.join("Library/Application Support/dev.eli.eli");
                (d.clone(), d)
            }
            ServiceOs::Linux => (home.join(".config/eli"), home.join(".local/share/eli")),
        }
    }

    fn home_dir() -> Result<PathBuf> {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")
    }

    fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
        match path.strip_prefix("~") {
            Ok(rest) => home.join(rest),
            Err(_) => path.to_path_buf(),
        }
    }

    // ── token ───────────────────────────────────────────────────────────────────

    /// 32 random bytes as base64url: 43 characters, safe in a URL path as is.
    fn generate_token() -> String {
        use base64::Engine as _;
        let mut bytes = [0u8; 32];
        let from_os = std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .is_ok();
        if !from_os {
            bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn read_token_file(path: &Path) -> Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(s.trim().to_string()).filter(|t| !t.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Write `contents` readable by the owner only (0600), replacing any old file atomically.
    fn write_private_file(path: &Path, contents: &str) -> Result<()> {
        write_file_atomic(path, contents, 0o600)
    }

    fn write_file_atomic(path: &Path, contents: &str, mode: u32) -> Result<()> {
        let dir = path.parent().context("path has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                opts.mode(mode);
            }
            let mut f = opts.open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(contents.as_bytes())
                .with_context(|| format!("write {}", tmp.display()))?;
            let _ = f.sync_all();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("chmod {}", tmp.display()))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// The token the service enforces: the token file, else MARKET_SEARCH_MCP_TOKEN.
    /// File first, so `mcp service url` (which reads the file) always matches what
    /// the running service accepts.
    fn resolve_service_token(layout: &ServiceLayout) -> Result<(String, String)> {
        if let Some(token) = read_token_file(&layout.token_file)? {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if let Ok(meta) = std::fs::metadata(&layout.token_file) {
                    if meta.permissions().mode() & 0o077 != 0 {
                        svc_log(format!(
                            "warning: {} is readable by other users; run chmod 600 on it",
                            layout.token_file.display()
                        ));
                    }
                }
            }
            return Ok((token, layout.token_file.display().to_string()));
        }
        if let Some(token) = mcp_http_token() {
            return Ok((token.to_string(), "MARKET_SEARCH_MCP_TOKEN".to_string()));
        }
        anyhow::bail!(
            "no token: {} does not exist. Run `market-search mcp service install` (it creates one), \
             or set MARKET_SEARCH_MCP_TOKEN.",
            layout.token_file.display()
        )
    }

    // ── binary location ─────────────────────────────────────────────────────────

    /// Which binary the service runs.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum BinaryPlan {
        /// Run it where it is.
        Keep(PathBuf),
        /// Copy the running binary to `to`, then run that copy.
        Copy { from: PathBuf, to: PathBuf, reason: String },
    }

    impl BinaryPlan {
        fn target(&self) -> &Path {
            match self {
                Self::Keep(p) => p,
                Self::Copy { to, .. } => to,
            }
        }
    }

    fn default_temp_dirs() -> Vec<PathBuf> {
        let mut dirs = vec![std::env::temp_dir()];
        for d in [
            "/tmp",
            "/private/tmp",
            "/var/tmp",
            "/private/var/tmp",
            "/var/folders",
            "/private/var/folders",
        ] {
            dirs.push(PathBuf::from(d));
        }
        dirs
    }

    /// macOS privacy (TCC) folders: a launchd job cannot exec a binary inside them;
    /// it hangs before main. Removable and network volumes are protected the same way.
    fn protected_reason(path: &Path, home: &Path) -> Option<String> {
        const PROTECTED: &[&str] = &[
            "Downloads",
            "Desktop",
            "Documents",
            "Library/Mobile Documents",
            "Library/CloudStorage",
        ];
        for p in PROTECTED {
            if path.starts_with(home.join(p)) {
                return Some(format!(
                    "it is under ~/{p}, a protected folder that background services can't run from"
                ));
            }
        }
        if path.starts_with("/Volumes") {
            return Some("it is on an external or network volume".to_string());
        }
        None
    }

    /// Why `path` is a poor home for a long-lived service binary, if it is one.
    fn unstable_reason(path: &Path, home: &Path, temp_dirs: &[PathBuf]) -> Option<String> {
        if let Some(reason) = protected_reason(path, home) {
            return Some(reason);
        }
        if temp_dirs
            .iter()
            .any(|t| !t.as_os_str().is_empty() && path.starts_with(t))
        {
            return Some("it is in a temporary directory".to_string());
        }
        let parts: Vec<String> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let is_profile = |s: &str| s == "debug" || s == "release";
        let in_cargo_target = parts.windows(2).any(|w| w[0] == "target" && is_profile(&w[1]))
            || parts.windows(3).any(|w| w[0] == "target" && is_profile(&w[2]));
        if in_cargo_target {
            return Some("it is a cargo build output, which a rebuild or `cargo clean` replaces".to_string());
        }
        if parts.iter().any(|p| p == "node_modules" || p == "_npx") {
            return Some("it is inside an npm/npx package cache".to_string());
        }
        if parts.iter().any(|p| p.ends_with(".app")) {
            return Some("it is inside an app bundle, which app updates replace".to_string());
        }
        None
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => a == b,
        }
    }

    /// Decide which binary the service runs. With `--binary`, that path (the running
    /// binary is copied there unless it already is that file). Otherwise the running
    /// binary stays put, unless it lives somewhere ephemeral or TCC-protected, in
    /// which case it is copied to ~/.local/bin/market-search.
    fn plan_binary(
        current: &Path,
        requested: Option<&Path>,
        layout: &ServiceLayout,
        temp_dirs: &[PathBuf],
    ) -> Result<BinaryPlan> {
        let current = std::fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf());
        // The binary path is canonical, so compare against the canonical home too
        // (a home under a symlinked directory would otherwise never match).
        let mut homes = vec![layout.home.clone()];
        if let Ok(canonical) = std::fs::canonicalize(&layout.home) {
            if canonical != layout.home {
                homes.push(canonical);
            }
        }
        if let Some(target) = requested {
            let target = expand_tilde(target, &layout.home);
            if !target.is_absolute() {
                anyhow::bail!("--binary needs an absolute path, got {}", target.display());
            }
            if layout.os == ServiceOs::Macos {
                if let Some(reason) = homes.iter().find_map(|h| protected_reason(&target, h)) {
                    anyhow::bail!(
                        "--binary {}: {reason}. Use a path such as {}",
                        target.display(),
                        layout.stable_bin.display()
                    );
                }
            }
            if same_file(&current, &target) {
                return Ok(BinaryPlan::Keep(target));
            }
            return Ok(BinaryPlan::Copy {
                from: current,
                to: target,
                reason: "--binary names this path".to_string(),
            });
        }
        if same_file(&current, &layout.stable_bin) {
            return Ok(BinaryPlan::Keep(layout.stable_bin.clone()));
        }
        match homes.iter().find_map(|h| unstable_reason(&current, h, temp_dirs)) {
            Some(reason) => Ok(BinaryPlan::Copy {
                from: current,
                to: layout.stable_bin.clone(),
                reason,
            }),
            None => Ok(BinaryPlan::Keep(current)),
        }
    }

    /// Carry out a Copy plan. The copy lands under a temp name and is renamed over
    /// the old one, so a running service keeps its old file and nothing is truncated
    /// under it.
    fn install_binary(plan: &BinaryPlan) -> Result<()> {
        let BinaryPlan::Copy { from, to, .. } = plan else {
            return Ok(());
        };
        let dir = to.parent().context("binary path has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = dir.join(format!(".market-search.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::fs::copy(from, &tmp)
            .with_context(|| format!("copy {} to {}", from.display(), tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("chmod {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, to).with_context(|| format!("move the copy to {}", to.display()))?;
        if cfg!(target_os = "macos") {
            // A quarantine flag copied along with the file can make Gatekeeper stall
            // the launchd exec. Absent on most installs; ignore the result.
            let _ = std::process::Command::new("xattr")
                .args(["-d", "com.apple.quarantine"])
                .arg(to)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        Ok(())
    }

    // ── service definition ──────────────────────────────────────────────────────

    #[derive(Clone, Debug)]
    pub(super) struct ServiceSpec {
        program: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
        work_dir: PathBuf,
        log_file: PathBuf,
        /// Holds a secret passed through from the environment: write the file 0600.
        private: bool,
    }

    impl ServiceSpec {
        /// The same spec with copied environment values masked, for printing.
        fn masked(&self) -> Self {
            let mut spec = self.clone();
            for (k, v) in spec.env.iter_mut() {
                if k != "HOME" && k != "PATH" {
                    *v = "(copied from this shell)".to_string();
                }
            }
            spec
        }
    }

    fn service_args(kind: TunnelKind, port: u16, domain: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = ["mcp", "share", "--service", "--provider", kind.name(), "--port"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        args.push(port.to_string());
        if let Some(d) = domain {
            args.push("--domain".to_string());
            args.push(d.to_string());
        }
        args
    }

    fn build_service_spec(
        layout: &ServiceLayout,
        program: &Path,
        kind: TunnelKind,
        port: u16,
        domain: Option<&str>,
        tool_dirs: &[PathBuf],
        env_lookup: &dyn Fn(&str) -> Option<String>,
    ) -> ServiceSpec {
        let mut path_dirs: Vec<PathBuf> = tool_dirs.to_vec();
        path_dirs.push(layout.home.join(".local/bin"));
        if layout.os == ServiceOs::Macos {
            path_dirs.push(PathBuf::from("/opt/homebrew/bin"));
        }
        for d in ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
            path_dirs.push(PathBuf::from(d));
        }
        let mut seen = std::collections::HashSet::new();
        path_dirs.retain(|d| seen.insert(d.clone()));
        let path_value = path_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(":");

        let mut env = vec![
            ("HOME".to_string(), layout.home.display().to_string()),
            ("PATH".to_string(), path_value),
        ];
        let mut private = false;
        for name in SERVICE_ENV_PASSTHROUGH {
            if let Some(value) = env_lookup(name).filter(|v| !v.trim().is_empty()) {
                private |= SERVICE_ENV_SECRETS.contains(name);
                env.push((name.to_string(), value));
            }
        }
        ServiceSpec {
            program: program.to_path_buf(),
            args: service_args(kind, port, domain),
            env,
            work_dir: layout.work_dir.clone(),
            log_file: layout.log_file.clone(),
            private,
        }
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    fn xml_unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    /// launchd job: start at login (RunAtLoad), restart whenever it exits (KeepAlive).
    fn render_launchd_plist(spec: &ServiceSpec) -> String {
        let mut s = String::new();
        s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        s.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
        s.push_str("<plist version=\"1.0\">\n<dict>\n");
        let _ = writeln!(s, "\t<key>Label</key>\n\t<string>{SERVICE_LABEL}</string>");
        s.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
        let _ = writeln!(s, "\t\t<string>{}</string>", xml_escape(&spec.program.display().to_string()));
        for a in &spec.args {
            let _ = writeln!(s, "\t\t<string>{}</string>", xml_escape(a));
        }
        s.push_str("\t</array>\n");
        s.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
        s.push_str("\t<key>KeepAlive</key>\n\t<true/>\n");
        s.push_str("\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n");
        let _ = writeln!(
            s,
            "\t<key>WorkingDirectory</key>\n\t<string>{}</string>",
            xml_escape(&spec.work_dir.display().to_string())
        );
        s.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (k, v) in &spec.env {
            let _ = writeln!(
                s,
                "\t\t<key>{}</key>\n\t\t<string>{}</string>",
                xml_escape(k),
                xml_escape(v)
            );
        }
        s.push_str("\t</dict>\n");
        let log = xml_escape(&spec.log_file.display().to_string());
        let _ = writeln!(s, "\t<key>StandardOutPath</key>\n\t<string>{log}</string>");
        let _ = writeln!(s, "\t<key>StandardErrorPath</key>\n\t<string>{log}</string>");
        s.push_str("</dict>\n</plist>\n");
        s
    }

    /// One ExecStart= word. `%` (specifiers) and `$` (variable expansion) are always
    /// escaped; words with spaces, quotes or backslashes are double-quoted.
    fn systemd_word(arg: &str) -> String {
        let needs_quotes = arg.is_empty()
            || arg
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';'));
        let escaped = arg
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$");
        if needs_quotes {
            format!("\"{escaped}\"")
        } else {
            escaped
        }
    }

    /// systemd --user service: restart forever, log to a file next to the other state.
    fn render_systemd_unit(spec: &ServiceSpec) -> String {
        let exec = std::iter::once(spec.program.display().to_string())
            .chain(spec.args.iter().cloned())
            .map(|a| systemd_word(&a))
            .collect::<Vec<_>>()
            .join(" ");
        let pct = |p: &Path| p.display().to_string().replace('%', "%%");
        let mut s = String::new();
        s.push_str("[Unit]\n");
        s.push_str("Description=market-search MCP server and public tunnel (claude.ai connector)\n");
        s.push_str("Documentation=https://github.com/efoltyn/market-search\n");
        s.push_str("StartLimitIntervalSec=0\n\n");
        s.push_str("[Service]\n");
        s.push_str("Type=simple\n");
        let _ = writeln!(s, "ExecStart={exec}");
        let _ = writeln!(s, "WorkingDirectory={}", pct(&spec.work_dir));
        for (k, v) in &spec.env {
            let value = format!("{k}={v}")
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%");
            let _ = writeln!(s, "Environment=\"{value}\"");
        }
        s.push_str("Restart=always\n");
        s.push_str("RestartSec=10\n");
        let _ = writeln!(s, "StandardOutput=append:{}", pct(&spec.log_file));
        let _ = writeln!(s, "StandardError=append:{}", pct(&spec.log_file));
        s.push_str("\n[Install]\nWantedBy=default.target\n");
        s
    }

    fn render_service_file(os: ServiceOs, spec: &ServiceSpec) -> String {
        match os {
            ServiceOs::Macos => render_launchd_plist(spec),
            ServiceOs::Linux => render_systemd_unit(spec),
        }
    }

    /// The command line (program first) recorded in an installed service file.
    fn service_file_command(os: ServiceOs, text: &str) -> Vec<String> {
        match os {
            ServiceOs::Macos => {
                let Some(start) = text.find("<key>ProgramArguments</key>") else {
                    return Vec::new();
                };
                let rest = &text[start..];
                let end = rest.find("</array>").unwrap_or(rest.len());
                static STRING_RE: Lazy<Regex> =
                    Lazy::new(|| Regex::new(r"<string>(.*?)</string>").expect("valid regex"));
                STRING_RE
                    .captures_iter(&rest[..end])
                    .map(|c| xml_unescape(&c[1]))
                    .collect()
            }
            ServiceOs::Linux => {
                let Some(line) = text.lines().find_map(|l| l.strip_prefix("ExecStart=")) else {
                    return Vec::new();
                };
                split_systemd_words(line)
            }
        }
    }

    fn split_systemd_words(line: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut has_word = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if in_quotes => {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                '"' => {
                    in_quotes = !in_quotes;
                    has_word = true;
                }
                c if c.is_whitespace() && !in_quotes => {
                    if has_word {
                        words.push(std::mem::take(&mut cur));
                        has_word = false;
                    }
                }
                c => {
                    cur.push(c);
                    has_word = true;
                }
            }
        }
        if has_word {
            words.push(cur);
        }
        words
            .into_iter()
            .map(|w| w.replace("%%", "%").replace("$$", "$"))
            .collect()
    }

    fn arg_value(command: &[String], flag: &str) -> Option<String> {
        command
            .iter()
            .position(|a| a == flag)
            .and_then(|i| command.get(i + 1))
            .cloned()
    }

    // ── tunnel providers ────────────────────────────────────────────────────────

    /// Tunnel providers the service can supervise. Tailscale Funnel would be one more
    /// variant: a spawn command (or none: `tailscale funnel --bg`), a URL source
    /// (`tailscale status --json`), and a liveness probe.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum TunnelKind {
        /// Stable URL: a free account's dev domain, the same across restarts.
        Ngrok,
        /// Cloudflare quick tunnel: a new random URL on every start.
        Cloudflare,
    }

    impl TunnelKind {
        fn parse(s: &str) -> Result<Self> {
            match s.trim().to_ascii_lowercase().as_str() {
                "ngrok" => Ok(Self::Ngrok),
                "cloudflare" | "cloudflared" | "cf" => Ok(Self::Cloudflare),
                other => anyhow::bail!(
                    "the service supports --provider ngrok (same URL every time) or cloudflare \
                     (new URL on every restart), not '{other}'"
                ),
            }
        }

        fn name(self) -> &'static str {
            match self {
                Self::Ngrok => "ngrok",
                Self::Cloudflare => "cloudflare",
            }
        }

        fn binary(self) -> &'static str {
            match self {
                Self::Ngrok => "ngrok",
                Self::Cloudflare => "cloudflared",
            }
        }

        fn stable_url(self) -> bool {
            matches!(self, Self::Ngrok)
        }

        /// Child command line. The upstream is IPv4 on purpose: `localhost` can
        /// resolve to ::1 first while the server listens on 127.0.0.1 only.
        fn command_args(self, port: u16, domain: Option<&str>) -> Vec<String> {
            match self {
                Self::Ngrok => {
                    let mut args = vec!["http".to_string(), format!("127.0.0.1:{port}")];
                    if let Some(d) = domain {
                        args.push(format!("--url={d}"));
                    }
                    // No request capture in ngrok's local inspector: it would record
                    // every /mcp/<token> path for any local user to read. The agent
                    // API (/api/tunnels) that the supervisor polls stays on.
                    args.push("--inspect=false".to_string());
                    args.push("--log=stdout".to_string());
                    args.push("--log-format=logfmt".to_string());
                    args
                }
                Self::Cloudflare => vec![
                    "tunnel".to_string(),
                    "--url".to_string(),
                    format!("http://127.0.0.1:{port}"),
                    "--no-autoupdate".to_string(),
                ],
            }
        }
    }

    /// Accept "mysub", "mysub.ngrok-free.dev" or "https://mysub.ngrok-free.dev/".
    fn normalize_ngrok_domain(d: &str) -> String {
        let d = d
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        if d.contains('.') {
            d.to_string()
        } else {
            format!("{d}.ngrok-free.dev")
        }
    }

    /// What the tunnel child has told us so far, from its log lines.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub(super) struct TunnelObservation {
        public_url: Option<String>,
        /// ngrok's local agent API ("127.0.0.1:4040"; 4041+ when another agent holds 4040).
        api_addr: Option<String>,
        last_error: Option<String>,
    }

    fn truncate_chars(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            s.to_string()
        } else {
            let mut out: String = s.chars().take(max).collect();
            out.push('…');
            out
        }
    }

    fn observe_tunnel_line(kind: TunnelKind, line: &str, obs: &mut TunnelObservation) {
        static NGROK_URL: Lazy<Regex> =
            Lazy::new(|| Regex::new(r#"\burl=(https://[^\s"]+)"#).expect("valid regex"));
        static NGROK_WEB: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r#"msg="starting web service".*?\baddr=([^\s"]+)"#).expect("valid regex")
        });
        static NGROK_ERR: Lazy<Regex> =
            Lazy::new(|| Regex::new(r#"\berr="((?:[^"\\]|\\.)*)""#).expect("valid regex"));
        static CF_URL: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"https://[a-z0-9-]+\.trycloudflare\.com").expect("valid regex")
        });
        match kind {
            TunnelKind::Ngrok => {
                if line.contains("msg=\"started tunnel\"") {
                    if let Some(c) = NGROK_URL.captures(line) {
                        obs.public_url = Some(c[1].trim_end_matches('/').to_string());
                    }
                }
                if let Some(c) = NGROK_WEB.captures(line) {
                    obs.api_addr = Some(c[1].replace("localhost", "127.0.0.1"));
                }
                let is_error = line.contains("lvl=eror")
                    || line.contains("lvl=crit")
                    || line.contains("ERR_NGROK_")
                    || line.trim_start().starts_with("ERROR:");
                if is_error {
                    let raw = NGROK_ERR
                        .captures(line)
                        .map(|c| c[1].replace("\\\"", "\"").replace("\\n", " "))
                        .unwrap_or_else(|| line.trim().trim_start_matches("ERROR:").to_string());
                    let msg = raw.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !msg.is_empty() {
                        obs.last_error = Some(truncate_chars(&msg, 300));
                    }
                }
            }
            TunnelKind::Cloudflare => {
                if let Some(m) = CF_URL.find(line) {
                    obs.public_url = Some(m.as_str().to_string());
                }
                if line.contains(" ERR ") {
                    obs.last_error = Some(truncate_chars(line.trim(), 300));
                }
            }
        }
    }

    /// ngrok errors that mean the saved authtoken is missing or bad.
    fn ngrok_auth_error(err: &str) -> bool {
        ["ERR_NGROK_105", "ERR_NGROK_107", "ERR_NGROK_4018", "authentication failed"]
            .iter()
            .any(|code| err.contains(code))
            && !err.contains("ERR_NGROK_108")
    }

    fn addr_matches_port(addr: &str, port: u16) -> bool {
        let a = addr.trim().trim_end_matches('/');
        a == port.to_string() || a.ends_with(&format!(":{port}"))
    }

    /// The public URL of the tunnel that forwards to our port, from ngrok's
    /// `GET /api/tunnels`. Another agent's tunnels (other ports) don't count.
    fn ngrok_tunnel_for_port(body: &serde_json::Value, port: u16) -> Option<String> {
        body.get("tunnels")?.as_array()?.iter().find_map(|t| {
            let addr = t.pointer("/config/addr")?.as_str()?;
            let url = t.get("public_url")?.as_str()?;
            (url.starts_with("https://") && addr_matches_port(addr, port))
                .then(|| url.trim_end_matches('/').to_string())
        })
    }

    fn local_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    }

    async fn ngrok_api_tunnels(client: &reqwest::Client, api_addr: &str) -> Result<serde_json::Value> {
        let url = format!("http://{api_addr}/api/tunnels");
        let resp = client.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        let resp = resp.error_for_status().with_context(|| format!("GET {url}"))?;
        resp.json().await.with_context(|| format!("parse {url}"))
    }

    async fn local_health(client: &reqwest::Client, port: u16) -> bool {
        match client.get(format!("http://127.0.0.1:{port}/")).send().await {
            Ok(r) if r.status().is_success() => r.text().await.map(|b| b.contains("eli-mcp")).unwrap_or(false),
            _ => false,
        }
    }

    enum TunnelProbe {
        Up(Option<String>),
        Missing(String),
        Unknown,
    }

    async fn probe_tunnel(
        kind: TunnelKind,
        obs: &TunnelObservation,
        port: u16,
        client: &reqwest::Client,
    ) -> TunnelProbe {
        match kind {
            TunnelKind::Ngrok => {
                let Some(api) = obs.api_addr.as_deref() else {
                    return TunnelProbe::Unknown;
                };
                match ngrok_api_tunnels(client, api).await {
                    Ok(body) => match ngrok_tunnel_for_port(&body, port) {
                        Some(url) => TunnelProbe::Up(Some(url)),
                        None => TunnelProbe::Missing(format!(
                            "ngrok's local API ({api}) lists no tunnel to 127.0.0.1:{port}"
                        )),
                    },
                    Err(e) => TunnelProbe::Missing(format!("ngrok's local API is not answering: {e:#}")),
                }
            }
            // cloudflared has no free local API worth polling; the process exit is the signal.
            TunnelKind::Cloudflare => TunnelProbe::Unknown,
        }
    }

    // ── state file (written by the service, read by install/status/url) ────────

    #[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
    pub(super) struct ServiceState {
        provider: String,
        /// starting | up | reconnecting | stopped
        status: String,
        public_url: Option<String>,
        local_port: u16,
        pid: u32,
        api_addr: Option<String>,
        last_error: Option<String>,
        started_at: String,
        updated_at: String,
    }

    fn now_utc() -> String {
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    fn read_state(path: &Path) -> Option<ServiceState> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write_state(path: &Path, state: &mut ServiceState) {
        state.updated_at = now_utc();
        let Ok(text) = serde_json::to_string_pretty(state) else {
            return;
        };
        if let Err(e) = write_file_atomic(path, &text, 0o644) {
            svc_log(format!("could not write {}: {e:#}", path.display()));
        }
    }

    // ── service manager (launchctl / systemctl) ─────────────────────────────────

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub(super) struct ServiceRuntime {
        loaded: bool,
        running: bool,
        pid: Option<u32>,
    }

    fn current_uid() -> Result<u32> {
        let out = std::process::Command::new("id")
            .arg("-u")
            .output()
            .context("run `id -u`")?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .context("read the output of `id -u`")
    }

    fn launchd_domain(native: bool) -> String {
        if native {
            if let Ok(uid) = current_uid() {
                return format!("gui/{uid}");
            }
        }
        "gui/$(id -u)".to_string()
    }

    fn parse_launchctl_print(text: &str) -> ServiceRuntime {
        static STATE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*state = (\S+)").expect("valid regex"));
        static PID: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*pid = (\d+)").expect("valid regex"));
        let state = STATE.captures(text).map(|c| c[1].to_string());
        let pid = PID.captures(text).and_then(|c| c[1].parse().ok());
        ServiceRuntime {
            loaded: true,
            running: state.as_deref() == Some("running"),
            pid,
        }
    }

    fn parse_systemctl_show(text: &str) -> ServiceRuntime {
        let get = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .map(str::trim)
                .unwrap_or("")
        };
        let pid = get("MainPID").parse::<u32>().ok().filter(|p| *p != 0);
        ServiceRuntime {
            loaded: get("LoadState") == "loaded",
            running: get("ActiveState") == "active",
            pid,
        }
    }

    async fn query_service(os: ServiceOs) -> ServiceRuntime {
        match os {
            ServiceOs::Macos => {
                let target = format!("{}/{SERVICE_LABEL}", launchd_domain(true));
                match TokioCommand::new("launchctl").args(["print", &target]).output().await {
                    Ok(o) if o.status.success() => parse_launchctl_print(&String::from_utf8_lossy(&o.stdout)),
                    _ => ServiceRuntime::default(),
                }
            }
            ServiceOs::Linux => {
                let out = TokioCommand::new("systemctl")
                    .args(["--user", "show", SYSTEMD_UNIT, "-p", "LoadState", "-p", "ActiveState", "-p", "MainPID"])
                    .output()
                    .await;
                match out {
                    Ok(o) if o.status.success() => parse_systemctl_show(&String::from_utf8_lossy(&o.stdout)),
                    _ => ServiceRuntime::default(),
                }
            }
        }
    }

    fn unload_commands(os: ServiceOs, domain: &str) -> Vec<Vec<String>> {
        let v = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match os {
            ServiceOs::Macos => vec![v(&["launchctl", "bootout", &format!("{domain}/{SERVICE_LABEL}")])],
            ServiceOs::Linux => vec![v(&["systemctl", "--user", "disable", "--now", SYSTEMD_UNIT])],
        }
    }

    fn load_commands(os: ServiceOs, domain: &str, service_file: &Path) -> Vec<Vec<String>> {
        let v = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match os {
            ServiceOs::Macos => vec![
                // A job disabled earlier (launchctl disable) refuses to bootstrap.
                v(&["launchctl", "enable", &format!("{domain}/{SERVICE_LABEL}")]),
                v(&["launchctl", "bootstrap", domain, &service_file.display().to_string()]),
            ],
            ServiceOs::Linux => vec![
                v(&["systemctl", "--user", "daemon-reload"]),
                v(&["systemctl", "--user", "enable", SYSTEMD_UNIT]),
                v(&["systemctl", "--user", "restart", SYSTEMD_UNIT]),
            ],
        }
    }

    fn shell_join(argv: &[String]) -> String {
        argv.iter()
            .map(|a| {
                if !a.is_empty()
                    && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,$()".contains(c))
                {
                    a.clone()
                } else {
                    format!("'{}'", a.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    async fn run_checked(argv: &[String]) -> Result<()> {
        let out = TokioCommand::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("run {}", argv[0]))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = if stderr.trim().is_empty() { stdout.trim().to_string() } else { stderr.trim().to_string() };
        anyhow::bail!("`{}` failed ({}): {detail}", shell_join(argv), out.status)
    }

    /// Linux: a user service stops at logout unless lingering is on.
    fn linger_note() -> Option<String> {
        let user = std::env::var("USER").ok().filter(|u| !u.is_empty())?;
        let out = std::process::Command::new("loginctl")
            .args(["show-user", &user, "--property=Linger"])
            .output()
            .ok()?;
        if String::from_utf8_lossy(&out.stdout).trim() == "Linger=yes" {
            return None;
        }
        Some(format!(
            "It runs while you are logged in. To keep it running after you log out, run once: \
             sudo loginctl enable-linger {user}"
        ))
    }

    // ── other ngrok agents (a free account allows one online at a time) ────────

    fn parse_ps_for_ngrok(text: &str, exclude_parent: Option<u32>) -> Vec<u32> {
        text.lines()
            .filter_map(|line| {
                let mut it = line.split_whitespace();
                let pid: u32 = it.next()?.parse().ok()?;
                let ppid: u32 = it.next()?.parse().ok()?;
                let comm = it.collect::<Vec<_>>().join(" ");
                let base = Path::new(&comm).file_name()?.to_string_lossy().into_owned();
                (base == "ngrok" && Some(ppid) != exclude_parent).then_some(pid)
            })
            .collect()
    }

    /// ngrok agents running on this machine, other than our own service's child.
    fn other_ngrok_agents(service_pid: Option<u32>) -> Vec<u32> {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-e", "-o", "pid=,ppid=,comm="])
            .output()
        else {
            return Vec::new();
        };
        parse_ps_for_ngrok(&String::from_utf8_lossy(&out.stdout), service_pid)
    }

    // ── tools: find, install ngrok, authtoken ───────────────────────────────────

    fn tool_search_dirs(home: &Path) -> Vec<PathBuf> {
        vec![
            home.join(".local/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/snap/bin"),
            PathBuf::from("/home/linuxbrew/.linuxbrew/bin"),
        ]
    }

    /// PATH first, then the usual install spots (launchd's PATH is minimal).
    fn find_tool(name: &str, home: &Path) -> Option<PathBuf> {
        which(name).or_else(|| {
            tool_search_dirs(home)
                .into_iter()
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        })
    }

    fn ngrok_download_url(os: ServiceOs, arch: &str, archive: &str) -> Result<String> {
        let os = match os {
            ServiceOs::Macos => "darwin",
            ServiceOs::Linux => "linux",
        };
        let arch = match arch {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            "arm" => "arm",
            "x86" => "386",
            other => anyhow::bail!("ngrok publishes no build for {os}/{other}"),
        };
        Ok(format!("{NGROK_DOWNLOAD_BASE}-{os}-{arch}.{archive}"))
    }

    fn ngrok_config_files(home: &Path, os: ServiceOs) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            files.push(PathBuf::from(x).join("ngrok/ngrok.yml"));
        }
        if os == ServiceOs::Macos {
            files.push(home.join("Library/Application Support/ngrok/ngrok.yml"));
        }
        files.push(home.join(".config/ngrok/ngrok.yml"));
        files.push(home.join(".ngrok2/ngrok.yml"));
        files
    }

    /// The ngrok config file that holds an authtoken, if any. Checks presence only;
    /// the value is never read out or printed.
    fn ngrok_authtoken_file(home: &Path, os: ServiceOs) -> Option<PathBuf> {
        static AUTHTOKEN: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r#"(?m)^\s*authtoken:\s*["']?[A-Za-z0-9_\-]{8,}"#).expect("valid regex")
        });
        ngrok_config_files(home, os).into_iter().find(|p| {
            std::fs::read_to_string(p)
                .map(|s| AUTHTOKEN.is_match(&s))
                .unwrap_or(false)
        })
    }

    /// Accept the bare token, or the whole `ngrok config add-authtoken <token>` line
    /// the dashboard shows.
    fn clean_ngrok_authtoken(input: &str) -> Option<String> {
        let t = input.split_whitespace().last()?.trim_matches(|c| c == '"' || c == '\'');
        let ok = (20..=200).contains(&t.len())
            && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        ok.then(|| t.to_string())
    }

    async fn install_ngrok(layout: &ServiceLayout, ui: &Ui) -> Result<PathBuf> {
        if layout.os == ServiceOs::Macos {
            if let Some(brew) = find_tool("brew", &layout.home) {
                ui.say("  ngrok    installing with Homebrew: brew install --cask ngrok");
                let status = TokioCommand::new(&brew)
                    .args(["install", "--cask", "ngrok"])
                    .stdin(Stdio::null())
                    .stdout(ui.child_stdout())
                    .stderr(ui.child_stderr())
                    .status()
                    .await;
                if matches!(status, Ok(s) if s.success()) {
                    if let Some(p) = find_tool("ngrok", &layout.home) {
                        return Ok(p);
                    }
                }
                ui.say("  ngrok    Homebrew didn't install it; downloading from ngrok.com instead");
            }
        }
        download_ngrok(layout, ui).await
    }

    /// Download the official ngrok v3 build into ~/.local/bin/ngrok.
    async fn download_ngrok(layout: &ServiceLayout, ui: &Ui) -> Result<PathBuf> {
        let bin_dir = layout.home.join(".local/bin");
        let use_zip = layout.os == ServiceOs::Macos || find_tool("unzip", &layout.home).is_some();
        let archive = if use_zip { "zip" } else { "tgz" };
        let url = ngrok_download_url(layout.os, std::env::consts::ARCH, archive)?;
        ui.say(format!("  ngrok    downloading {url}"));
        let bytes = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?
            .get(&url)
            .send()
            .await
            .with_context(|| format!("download {url}"))?
            .error_for_status()
            .with_context(|| format!("download {url}"))?
            .bytes()
            .await
            .with_context(|| format!("download {url}"))?;
        // Fresh, empty directory for the untrusted archive.
        let work = bin_dir.join(format!(".ngrok-download-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
        let result = (|| -> Result<PathBuf> {
            let archive_path = work.join(format!("ngrok.{archive}"));
            std::fs::write(&archive_path, &bytes)?;
            let status = if use_zip {
                std::process::Command::new("unzip")
                    .args(["-o", "-q"])
                    .arg(&archive_path)
                    .arg("-d")
                    .arg(&work)
                    .status()
            } else {
                std::process::Command::new("tar")
                    .arg("-xzf")
                    .arg(&archive_path)
                    .arg("-C")
                    .arg(&work)
                    .status()
            }
            .context("extract the ngrok archive")?;
            anyhow::ensure!(status.success(), "extracting {} failed", archive_path.display());
            let extracted = work.join("ngrok");
            anyhow::ensure!(extracted.is_file(), "the archive had no ngrok binary");
            let dest = bin_dir.join("ngrok");
            std::fs::rename(&extracted, &dest)
                .with_context(|| format!("move ngrok to {}", dest.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
            }
            Ok(dest)
        })();
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    async fn save_ngrok_authtoken(ngrok: &Path, token: &str, ui: &Ui) -> Result<()> {
        let out = TokioCommand::new(ngrok)
            .args(["config", "add-authtoken", token])
            .stdin(Stdio::null())
            .output()
            .await
            .context("run ngrok config add-authtoken")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).replace(token, "<authtoken>");
            anyhow::bail!("`ngrok config add-authtoken` failed: {}", err.trim());
        }
        ui.say("  ngrok    saved your authtoken (ngrok config add-authtoken)");
        Ok(())
    }

    async fn ensure_ngrok_authtoken(
        ngrok: &Path,
        layout: &ServiceLayout,
        passed: Option<&str>,
        ui: &Ui,
    ) -> Result<()> {
        if let Some(raw) = passed {
            let Some(token) = clean_ngrok_authtoken(raw) else {
                return Err(NeedsInput::authtoken(format!(
                    "That doesn't look like an ngrok authtoken. Copy it again from {NGROK_AUTHTOKEN_URL}."
                ))
                .into());
            };
            return save_ngrok_authtoken(ngrok, &token, ui).await;
        }
        if let Some(file) = ngrok_authtoken_file(&layout.home, layout.os) {
            ui.say(format!("  ngrok    authtoken found in {}", file.display()));
            return Ok(());
        }
        if let Some(token) = std::env::var("NGROK_AUTHTOKEN")
            .ok()
            .and_then(|t| clean_ngrok_authtoken(&t))
        {
            ui.say("  ngrok    saving NGROK_AUTHTOKEN into ngrok's config so the service can use it");
            return save_ngrok_authtoken(ngrok, &token, ui).await;
        }
        if ui.interactive {
            ui.say("");
            ui.say("ngrok needs a free account so your URL stays the same.");
            ui.say(format!("  1. Sign in (Google, GitHub, or email) at {NGROK_AUTHTOKEN_URL}"));
            ui.say("  2. Copy \"Your Authtoken\" and paste it here.");
            if open_in_browser(NGROK_AUTHTOKEN_URL) {
                ui.say("(Opened that page in your browser.)");
            }
            for _ in 0..3 {
                eprint!("ngrok authtoken: ");
                let line = console::Term::stderr()
                    .read_secure_line()
                    .context("read the authtoken from the terminal")?;
                match clean_ngrok_authtoken(&line) {
                    Some(token) => return save_ngrok_authtoken(ngrok, &token, ui).await,
                    None => ui.say("That doesn't look like an authtoken (letters, digits, - and _). Try again."),
                }
            }
            return Err(NeedsInput::authtoken(format!(
                "No ngrok authtoken. Copy it from {NGROK_AUTHTOKEN_URL} and run again with --authtoken <token>."
            ))
            .into());
        }
        if ui.may_open_browser {
            // Not a terminal, but a person may be at the screen (an agent ran us).
            open_in_browser(NGROK_AUTHTOKEN_URL);
        }
        Err(NeedsInput::authtoken(format!(
            "ngrok needs your authtoken (free account). Copy it from {NGROK_AUTHTOKEN_URL} \
             and run again with --authtoken <token>."
        ))
        .into())
    }

    // ── small UI helpers ────────────────────────────────────────────────────────

    /// Install stops here because a person has to supply something.
    #[derive(Debug)]
    struct NeedsInput {
        needs: &'static str,
        message: String,
    }

    impl NeedsInput {
        fn authtoken(message: String) -> Self {
            Self { needs: "ngrok_authtoken", message }
        }
    }

    impl std::fmt::Display for NeedsInput {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl std::error::Error for NeedsInput {}

    struct Ui {
        json: bool,
        /// May prompt and read the terminal.
        interactive: bool,
        /// May open a browser.
        may_open_browser: bool,
    }

    impl Ui {
        fn new(json: bool, yes: bool) -> Self {
            let hands_off = json || yes;
            Self {
                json,
                interactive: !hands_off
                    && std::io::stdin().is_terminal()
                    && std::io::stderr().is_terminal(),
                may_open_browser: !hands_off,
            }
        }

        /// People-facing output: stdout, or stderr under --json (stdout then carries
        /// only the JSON object).
        fn say(&self, msg: impl std::fmt::Display) {
            if self.json {
                eprintln!("{msg}");
            } else {
                println!("{msg}");
            }
        }

        fn child_stdout(&self) -> Stdio {
            if self.json {
                Stdio::from(std::io::stderr())
            } else {
                Stdio::inherit()
            }
        }

        fn child_stderr(&self) -> Stdio {
            Stdio::inherit()
        }

        fn confirm(&self, question: &str) -> bool {
            if !self.interactive {
                return false;
            }
            eprint!("{question} [Y/n] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            if std::io::stdin().read_line(&mut answer).is_err() {
                return false;
            }
            matches!(answer.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
        }
    }

    fn open_in_browser(url: &str) -> bool {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(opener)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Copy to the clipboard with whatever this desktop has; returns the tool used.
    fn copy_to_clipboard(text: &str) -> Option<&'static str> {
        let candidates: Vec<(&'static str, Vec<&'static str>)> = if cfg!(target_os = "macos") {
            vec![("pbcopy", vec![])]
        } else {
            let mut c = Vec::new();
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                c.push(("wl-copy", vec![]));
            }
            if std::env::var_os("DISPLAY").is_some() {
                c.push(("xclip", vec!["-selection", "clipboard"]));
                c.push(("xsel", vec!["--clipboard", "--input"]));
            }
            c
        };
        for (tool, args) in candidates {
            let Ok(mut child) = std::process::Command::new(tool)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return Some(tool);
            }
        }
        None
    }

    fn tail_lines(path: &Path, n: usize) -> String {
        let Ok(text) = std::fs::read_to_string(path) else {
            return String::new();
        };
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }

    /// "a", "a and b", "a, b, and c".
    fn join_clauses(items: &[String]) -> String {
        match items {
            [] => String::new(),
            [one] => one.clone(),
            [a, b] => format!("{a} and {b}"),
            [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
        }
    }

    fn redact_url_token(url: &str) -> String {
        match url.rsplit_once("/mcp/") {
            Some((base, token)) if token.len() > 6 => format!("{base}/mcp/{}…", &token[..4]),
            _ => url.to_string(),
        }
    }

    // ── JSON report (the --json contract) ───────────────────────────────────────

    /// The one object `install|uninstall|status|url --json` print on stdout.
    #[derive(Clone, Debug, Serialize, PartialEq)]
    pub(super) struct ServiceReport {
        installed: bool,
        running: bool,
        /// "up" | "down" | "unknown"
        tunnel: &'static str,
        provider: String,
        /// https://<domain>/mcp/<token>, or null
        url: Option<String>,
        service_file: String,
        logs: String,
        /// One plain sentence for people.
        message: String,
        /// null | "ngrok_authtoken" | "ngrok_install"
        needs: Option<&'static str>,
    }

    impl ServiceReport {
        fn blank(provider: &str) -> Self {
            Self {
                installed: false,
                running: false,
                tunnel: "unknown",
                provider: provider.to_string(),
                url: None,
                service_file: String::new(),
                logs: String::new(),
                message: String::new(),
                needs: None,
            }
        }
    }

    fn print_json(report: &ServiceReport) {
        match serde_json::to_string(report) {
            Ok(s) => println!("{s}"),
            Err(_) => println!("{{}}"),
        }
    }

    /// Snapshot for status/url (and for failure reports).
    struct StatusView {
        report: ServiceReport,
        runtime: ServiceRuntime,
        state: Option<ServiceState>,
        local_ok: Option<bool>,
        port: u16,
    }

    async fn gather_status(layout: &ServiceLayout) -> StatusView {
        let runtime = query_service(layout.os).await;
        let file_text = std::fs::read_to_string(&layout.service_file).ok();
        let installed = file_text.is_some() || runtime.loaded;
        let command = file_text
            .as_deref()
            .map(|t| service_file_command(layout.os, t))
            .unwrap_or_default();
        let state = read_state(&layout.state_file);
        let provider = arg_value(&command, "--provider")
            .or_else(|| state.as_ref().map(|s| s.provider.clone()))
            .unwrap_or_else(|| "ngrok".to_string());
        let port = arg_value(&command, "--port")
            .and_then(|p| p.parse().ok())
            .or_else(|| state.as_ref().map(|s| s.local_port))
            .unwrap_or(DEFAULT_SERVICE_PORT);
        let token = read_token_file(&layout.token_file).ok().flatten();
        let client = local_client();

        let local_ok = if runtime.running { Some(local_health(&client, port).await) } else { None };
        let mut live_url = None;
        let tunnel: &'static str = if !runtime.running {
            "down"
        } else {
            let api = state.as_ref().and_then(|s| s.api_addr.clone());
            match (TunnelKind::parse(&provider).ok(), api) {
                (Some(TunnelKind::Ngrok), Some(api)) => match ngrok_api_tunnels(&client, &api).await {
                    Ok(body) => match ngrok_tunnel_for_port(&body, port) {
                        Some(u) => {
                            live_url = Some(u);
                            "up"
                        }
                        None => "down",
                    },
                    Err(_) => "down",
                },
                _ => match state.as_ref().map(|s| s.status.as_str()) {
                    Some("up") => "up",
                    Some("reconnecting") => "down",
                    _ => "unknown",
                },
            }
        };
        let public = live_url.or_else(|| state.as_ref().and_then(|s| s.public_url.clone()));
        let url = match (&public, &token) {
            (Some(p), Some(t)) => Some(mcp_endpoint_url(p, Some(t))),
            _ => None,
        };
        let last_error = state.as_ref().and_then(|s| s.last_error.clone());
        let needs = last_error
            .as_deref()
            .filter(|e| tunnel != "up" && ngrok_auth_error(e))
            .map(|_| "ngrok_authtoken");

        let message = if !installed {
            "The background service is not installed.".to_string()
        } else if !runtime.running {
            format!("The service is installed but not running; see {}.", layout.log_file.display())
        } else if needs.is_some() {
            "The service is running, but ngrok rejected its authtoken.".to_string()
        } else {
            match tunnel {
                "up" => "The service is running and claude.ai can reach it at the connector URL.".to_string(),
                "down" => match &last_error {
                    Some(e) => format!("The service is running but the tunnel is down; it keeps retrying. Last error: {e}"),
                    None => "The service is running but the tunnel is down; it keeps retrying.".to_string(),
                },
                _ => "The service is running; the tunnel state can't be checked from here.".to_string(),
            }
        };

        StatusView {
            report: ServiceReport {
                installed,
                running: runtime.running,
                tunnel,
                provider,
                url,
                service_file: layout.service_file.display().to_string(),
                logs: layout.log_file.display().to_string(),
                message,
                needs,
            },
            runtime,
            state,
            local_ok,
            port,
        }
    }

    // ── commands ────────────────────────────────────────────────────────────────

    pub(crate) async fn cmd_mcp_service(args: McpServiceArgs) -> Result<()> {
        match args.cmd {
            McpServiceCommand::Install(a) => service_install_cmd(a).await,
            McpServiceCommand::Uninstall(a) => service_uninstall_cmd(a).await,
            McpServiceCommand::Status(a) => service_status_cmd(a).await,
            McpServiceCommand::Url(a) => service_url_cmd(a).await,
        }
    }

    /// Shared tail for install/uninstall: JSON on stdout, the right exit code.
    async fn finish(json: bool, provider: &str, platform: Option<&str>, result: Result<ServiceReport>) -> Result<()> {
        match result {
            Ok(report) => {
                if json {
                    print_json(&report);
                }
                Ok(())
            }
            Err(err) => {
                let needs = err.downcast_ref::<NeedsInput>().map(|n| n.needs);
                let code = if needs.is_some() { EXIT_NEEDS_INPUT } else { 1 };
                if json {
                    let mut report = match ServiceOs::resolve(platform, true)
                        .ok()
                        .and_then(|os| ServiceLayout::discover(os).ok())
                    {
                        Some(layout) if Some(layout.os) == ServiceOs::current() => gather_status(&layout).await.report,
                        Some(layout) => ServiceReport {
                            service_file: layout.service_file.display().to_string(),
                            logs: layout.log_file.display().to_string(),
                            ..ServiceReport::blank(provider)
                        },
                        None => ServiceReport::blank(provider),
                    };
                    report.message = format!("{err:#}");
                    report.needs = needs;
                    if needs.is_some() {
                        report.url = None;
                    }
                    print_json(&report);
                    std::process::exit(code);
                }
                if needs.is_some() {
                    eprintln!("{err}");
                    std::process::exit(code);
                }
                Err(err)
            }
        }
    }

    async fn service_install_cmd(args: McpServiceInstallArgs) -> Result<()> {
        let ui = Ui::new(args.json, args.yes);
        let result = service_install(&args, &ui).await;
        finish(args.json, &args.provider, args.platform.as_deref(), result).await
    }

    async fn service_install(args: &McpServiceInstallArgs, ui: &Ui) -> Result<ServiceReport> {
        let os = ServiceOs::resolve(args.platform.as_deref(), args.dry_run)?;
        let kind = TunnelKind::parse(&args.provider)?;
        let layout = ServiceLayout::discover(os)?;
        let native = Some(os) == ServiceOs::current();
        let dry = args.dry_run;
        let domain = args.domain.as_deref().map(normalize_ngrok_domain);
        if domain.is_some() && kind != TunnelKind::Ngrok {
            anyhow::bail!("--domain only applies to --provider ngrok");
        }
        if args.port == 0 {
            anyhow::bail!("--port must be a fixed port, not 0");
        }

        if dry {
            ui.say(format!("Dry run of `mcp service install` ({}). Nothing is changed.\n", os.manager()));
        } else {
            ui.say(format!("Installing the market-search background service ({}).\n", os.manager()));
        }

        // 1. Which binary the service runs (decided now so a bad --binary fails
        // before anything else; copied in step 4).
        let current = std::env::current_exe().context("find the running binary")?;
        let plan = plan_binary(&current, args.binary.as_deref(), &layout, &default_temp_dirs())?;

        // 2. What is there now (its pid tells our own ngrok child apart from others).
        let runtime = if native { query_service(os).await } else { ServiceRuntime::default() };
        let was_installed = layout.service_file.exists() || runtime.loaded;
        let mut needs: Option<&'static str> = None;
        let mut notes: Vec<String> = Vec::new();

        // The port must be free, unless our own running service holds it (a
        // reinstall stops that one first). Checked before anything is stopped.
        if native {
            let old_command = std::fs::read_to_string(&layout.service_file)
                .ok()
                .map(|t| service_file_command(os, &t))
                .unwrap_or_default();
            let old_port = arg_value(&old_command, "--port").and_then(|p| p.parse::<u16>().ok());
            let ours = runtime.running && old_port == Some(args.port);
            if !ours && std::net::TcpListener::bind(("127.0.0.1", args.port)).is_err() {
                let msg = format!(
                    "Port {} on 127.0.0.1 is already in use by another program. Pick another with --port.",
                    args.port
                );
                if !dry {
                    anyhow::bail!(msg);
                }
                ui.say(format!("  check    STOP: {msg}"));
                notes.push(format!("port {} is in use (pass --port)", args.port));
            }
        }

        // 3. The tunnel agent.
        let tunnel_bin: PathBuf = match kind {
            TunnelKind::Ngrok => {
                if native {
                    let others = other_ngrok_agents(runtime.pid);
                    if !others.is_empty() && !args.force {
                        let pids = others.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
                        let msg = format!(
                            "Another ngrok agent is already running on this machine (pid {pids}). A free ngrok \
                             account allows one agent at a time, so the service could fail or knock that one \
                             offline. Stop it first, or run again with --force."
                        );
                        if !dry {
                            anyhow::bail!(msg);
                        }
                        ui.say(format!("  check    STOP: {msg}"));
                        notes.push("another ngrok agent is running here (stop it or pass --force)".to_string());
                    }
                }
                let bin = match find_tool("ngrok", &layout.home) {
                    Some(p) => {
                        ui.say(format!("  ngrok    {}", p.display()));
                        p
                    }
                    None if dry => {
                        let how = if os == ServiceOs::Macos && find_tool("brew", &layout.home).is_some() {
                            "brew install --cask ngrok".to_string()
                        } else {
                            let archive = if os == ServiceOs::Macos || find_tool("unzip", &layout.home).is_some() { "zip" } else { "tgz" };
                            format!(
                                "download {} and unpack it to {}",
                                ngrok_download_url(os, std::env::consts::ARCH, archive)?,
                                layout.home.join(".local/bin/ngrok").display()
                            )
                        };
                        ui.say(format!("  ngrok    not found; would install it: {how}"));
                        layout.home.join(".local/bin/ngrok")
                    }
                    None => install_ngrok(&layout, ui).await.map_err(|e| NeedsInput {
                        needs: "ngrok_install",
                        message: format!(
                            "Couldn't install ngrok automatically ({e:#}). Install it from \
                             https://ngrok.com/download, then run this again."
                        ),
                    })?,
                };
                if dry {
                    if args.authtoken.is_some() {
                        ui.say("  ngrok    would save the --authtoken you passed: ngrok config add-authtoken <authtoken>");
                    } else if let Some(file) = ngrok_authtoken_file(&layout.home, os) {
                        ui.say(format!("  ngrok    authtoken found in {}", file.display()));
                    } else if std::env::var("NGROK_AUTHTOKEN").ok().and_then(|t| clean_ngrok_authtoken(&t)).is_some() {
                        ui.say("  ngrok    would save NGROK_AUTHTOKEN: ngrok config add-authtoken <authtoken>");
                    } else if ui.interactive {
                        ui.say(format!("  ngrok    no authtoken yet; would open {NGROK_AUTHTOKEN_URL} and ask you to paste it"));
                    } else {
                        ui.say("  ngrok    no authtoken yet; a real install stops and asks for --authtoken <token>");
                        needs = Some("ngrok_authtoken");
                    }
                } else {
                    ensure_ngrok_authtoken(&bin, &layout, args.authtoken.as_deref(), ui).await?;
                }
                bin
            }
            TunnelKind::Cloudflare => match find_tool("cloudflared", &layout.home) {
                Some(p) => {
                    ui.say(format!("  tunnel   {} (the URL changes on every restart)", p.display()));
                    p
                }
                None => anyhow::bail!(
                    "cloudflared not found. Install it (macOS: brew install cloudflared; Linux: \
                     https://github.com/cloudflare/cloudflared/releases/latest), or use the default --provider ngrok."
                ),
            },
        };

        // 4. Put the binary in place.
        if !dry {
            install_binary(&plan)?;
        }
        match &plan {
            BinaryPlan::Keep(p) => ui.say(format!("  binary   {} (runs from where it is)", p.display())),
            BinaryPlan::Copy { from, to, reason } => ui.say(format!(
                "  binary   {} {}\n           to {}\n           because {reason}",
                if dry { "would copy" } else { "copied" },
                from.display(),
                to.display()
            )),
        }

        // 5. Token.
        let existing = read_token_file(&layout.token_file)?;
        let token: Option<String> = match existing {
            Some(t) if !args.new_token => {
                ui.say(format!("  token    keeping {} (the URL stays the same)", layout.token_file.display()));
                Some(t)
            }
            _ if dry => {
                let verb = if args.new_token { "would replace" } else { "would create" };
                ui.say(format!("  token    {verb} {} (only you can read it)", layout.token_file.display()));
                None
            }
            _ => {
                let t = generate_token();
                write_private_file(&layout.token_file, &t)?;
                let verb = if args.new_token { "replaced" } else { "created" };
                ui.say(format!(
                    "  token    {verb} {} (only you can read it){}",
                    layout.token_file.display(),
                    if args.new_token { "; the old URL no longer works" } else { "" }
                ));
                Some(t)
            }
        };

        // 6. Service file.
        let tool_dirs: Vec<PathBuf> = tunnel_bin.parent().map(|d| vec![d.to_path_buf()]).unwrap_or_default();
        let spec = build_service_spec(
            &layout,
            plan.target(),
            kind,
            args.port,
            domain.as_deref(),
            &tool_dirs,
            &|k| std::env::var(k).ok(),
        );
        let rendered = render_service_file(os, &spec);
        let mode = if spec.private { 0o600 } else { 0o644 };
        let passed: Vec<&str> = spec
            .env
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| *k != "HOME" && *k != "PATH")
            .collect();
        if !passed.is_empty() {
            ui.say(format!("  env      copying {} from this shell into the service", passed.join(", ")));
        }
        let domain_arg = launchd_domain(native);
        let unload = unload_commands(os, &domain_arg);
        let load = load_commands(os, &domain_arg, &layout.service_file);

        if dry {
            ui.say(format!(
                "  service  would write {} (mode {:o}):\n",
                layout.service_file.display(),
                mode
            ));
            ui.say(render_service_file(os, &spec.masked()).trim_end());
            ui.say("\n  commands it would run:");
            if was_installed {
                for c in &unload {
                    ui.say(format!("    {}    (stop the old one first)", shell_join(c)));
                }
            }
            let dirs = [
                layout.service_file.parent(),
                layout.log_file.parent(),
                Some(layout.work_dir.as_path()),
            ];
            let mkdir: Vec<String> = std::iter::once("mkdir".to_string())
                .chain(std::iter::once("-p".to_string()))
                .chain(dirs.iter().flatten().map(|d| d.display().to_string()))
                .collect();
            ui.say(format!("    {}", shell_join(&mkdir)));
            for c in &load {
                ui.say(format!("    {}", shell_join(c)));
            }
            if os == ServiceOs::Linux {
                ui.say("    loginctl show-user $USER --property=Linger    (tells you how to keep it running after logout)");
            }
            ui.say("\n  the service then runs:");
            ui.say(format!("    {}", shell_join(&std::iter::once(spec.program.display().to_string()).chain(spec.args.iter().cloned()).collect::<Vec<_>>())));
            ui.say(format!("      HTTP MCP on 127.0.0.1:{} (token required: /mcp/<token>)", args.port));
            ui.say(format!(
                "      {} {}    (restarted when it exits{})",
                kind.binary(),
                shell_join(&kind.command_args(args.port, domain.as_deref())),
                if kind == TunnelKind::Ngrok {
                    ", and when ngrok's local API stops listing the tunnel; checked every 60s"
                } else {
                    ""
                }
            ));
            if os == ServiceOs::Macos {
                ui.say("      caffeinate -s -w <service pid>    (no system sleep while on power)");
            }
            ui.say(format!("      writes the public URL to {}", layout.state_file.display()));
            ui.say(format!(
                "  then: wait up to {}s for the URL, print https://<domain>/mcp/<token>, copy it, and offer to open {CLAUDE_CONNECTORS_URL}",
                URL_WAIT.as_secs()
            ));
            if needs.is_some() {
                notes.push("ngrok needs your authtoken (pass --authtoken)".to_string());
            }
            let message = if notes.is_empty() {
                format!(
                    "Dry run: would {}install the {} service running {} with {}, and print the connector URL. Nothing was changed.",
                    if was_installed { "re" } else { "" },
                    os.manager(),
                    plan.target().display(),
                    kind.name()
                )
            } else {
                format!("Dry run: a real install would stop because {}. Nothing was changed.", join_clauses(&notes))
            };
            return Ok(ServiceReport {
                installed: was_installed,
                running: runtime.running,
                tunnel: if runtime.running { "unknown" } else { "down" },
                provider: kind.name().to_string(),
                url: None,
                service_file: layout.service_file.display().to_string(),
                logs: layout.log_file.display().to_string(),
                message,
                needs,
            });
        }

        // 7. Write the service file and (re)load it.
        for dir in [layout.service_file.parent(), layout.log_file.parent(), Some(layout.work_dir.as_path())]
            .into_iter()
            .flatten()
        {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        if was_installed {
            for c in &unload {
                let _ = run_checked(c).await; // fine if it wasn't loaded
            }
            // Give the old process a moment to let go of its port and tunnel.
            for _ in 0..20 {
                if !query_service(os).await.running {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        write_file_atomic(&layout.service_file, &rendered, mode)?;
        ui.say(format!("  service  {}", layout.service_file.display()));
        let _ = std::fs::remove_file(&layout.state_file);
        for c in &load {
            let mut attempt = 0;
            loop {
                attempt += 1;
                match run_checked(c).await {
                    Ok(()) => break,
                    // launchd can still be tearing the old job down: retry bootstrap.
                    Err(_) if attempt < 5 && os == ServiceOs::Macos && c[1] == "bootstrap" => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) if os == ServiceOs::Linux => {
                        return Err(e.context(
                            "systemd --user is not reachable. Log in to a full session (or run \
                             `sudo loginctl enable-linger $USER` and log in again), then retry",
                        ));
                    }
                    Err(e) => {
                        return Err(e.context(
                            "launchd couldn't load the service. If you're connected over SSH, log in \
                             at the Mac itself once, then retry",
                        ))
                    }
                }
            }
            ui.say(format!("  load     {}", shell_join(c)));
        }

        // 8. Wait for the tunnel to report its public URL.
        ui.say("  waiting for the tunnel...");
        let state = wait_for_tunnel(&layout, kind, runtime.pid, ui).await?;
        let token = token.context("token missing after install")?;
        let public = state.public_url.clone().context("no public URL")?;
        let url = mcp_endpoint_url(&public, Some(&token));
        let copied = if ui.json { None } else { copy_to_clipboard(&url) };

        // 9. Tell the person what to do with it.
        ui.say("");
        ui.say(format!(
            "market-search is running in the background. {}",
            match os {
                ServiceOs::Macos => "It starts when you log in and restarts itself if it stops.",
                ServiceOs::Linux => "It starts with your user session and restarts itself if it stops.",
            }
        ));
        ui.say("");
        ui.say(if copied.is_some() { "Your connector URL (copied to the clipboard):" } else { "Your connector URL:" });
        ui.say("");
        ui.say(format!("    {url}"));
        ui.say("");
        ui.say("Keep it private. Anyone with this URL can use your market-search tools.");
        if !kind.stable_url() {
            ui.say("cloudflare gives a new URL each time the service restarts; `market-search mcp service url` prints the current one.");
        }
        ui.say("");
        ui.say("Add it to claude.ai once. It then works in the Claude app on your phone too.");
        ui.say(format!("  1. Open {CLAUDE_CONNECTORS_URL} (Settings, then Connectors)."));
        ui.say("  2. Click \"Add custom connector\".");
        ui.say("  3. Name it Market Search, paste the URL, and click \"Add\".");
        ui.say("");
        ui.say("  market-search mcp service status      check on it");
        ui.say("  market-search mcp service url         print the URL again");
        ui.say("  market-search mcp service uninstall   stop it");
        if os == ServiceOs::Linux {
            if let Some(note) = linger_note() {
                ui.say("");
                ui.say(note);
            }
        }
        if ui.confirm("\nOpen claude.ai connector settings now?") {
            open_in_browser(CLAUDE_CONNECTORS_URL);
        }

        Ok(ServiceReport {
            installed: true,
            running: true,
            tunnel: "up",
            provider: kind.name().to_string(),
            url: Some(url),
            service_file: layout.service_file.display().to_string(),
            logs: layout.log_file.display().to_string(),
            message: "market-search is running in the background; add the URL in claude.ai under Settings, Connectors, Add custom connector.".to_string(),
            needs: None,
        })
    }

    /// Wait for the new service process to report its public URL. A state file
    /// still written by the old process (`old_pid`) doesn't count: after
    /// --new-token that process would reject the new URL.
    async fn wait_for_tunnel(
        layout: &ServiceLayout,
        kind: TunnelKind,
        old_pid: Option<u32>,
        ui: &Ui,
    ) -> Result<ServiceState> {
        let deadline = Instant::now() + URL_WAIT;
        let mut last_note = Instant::now();
        loop {
            if let Some(state) = read_state(&layout.state_file).filter(|s| Some(s.pid) != old_pid) {
                if state.status == "up" && state.public_url.is_some() {
                    return Ok(state);
                }
                if let Some(err) = state.last_error.as_deref() {
                    if kind == TunnelKind::Ngrok && ngrok_auth_error(err) {
                        return Err(NeedsInput::authtoken(format!(
                            "ngrok rejected the saved authtoken ({err}). Copy a fresh one from \
                             {NGROK_AUTHTOKEN_URL} and run again with --authtoken <token>."
                        ))
                        .into());
                    }
                    if err.contains("ERR_NGROK_108") {
                        anyhow::bail!(
                            "Your ngrok account already has an agent online somewhere else, and a free account \
                             allows one. Stop that one; the service keeps retrying and comes up by itself. \
                             Check with `market-search mcp service status`."
                        );
                    }
                }
            }
            if Instant::now() >= deadline {
                let tail = tail_lines(&layout.log_file, 15);
                anyhow::bail!(
                    "The service started, but no public URL appeared within {}s. It keeps retrying; \
                     check `market-search mcp service status` or the log at {}{}",
                    URL_WAIT.as_secs(),
                    layout.log_file.display(),
                    if tail.is_empty() { String::new() } else { format!("\n--- last lines of the log ---\n{tail}") }
                );
            }
            if last_note.elapsed() >= Duration::from_secs(15) {
                ui.say("  still waiting for the tunnel...");
                last_note = Instant::now();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn service_uninstall_cmd(args: McpServiceUninstallArgs) -> Result<()> {
        let ui = Ui::new(args.json, true);
        let result = service_uninstall(&args, &ui).await;
        finish(args.json, "ngrok", args.platform.as_deref(), result).await
    }

    async fn service_uninstall(args: &McpServiceUninstallArgs, ui: &Ui) -> Result<ServiceReport> {
        let os = ServiceOs::resolve(args.platform.as_deref(), args.dry_run)?;
        let layout = ServiceLayout::discover(os)?;
        let native = Some(os) == ServiceOs::current();
        let runtime = if native { query_service(os).await } else { ServiceRuntime::default() };
        let file_text = std::fs::read_to_string(&layout.service_file).ok();
        let installed = file_text.is_some() || runtime.loaded;
        let command = file_text.as_deref().map(|t| service_file_command(os, t)).unwrap_or_default();
        let provider = arg_value(&command, "--provider").unwrap_or_else(|| "ngrok".to_string());
        let binary = command.first().cloned();
        let unload = unload_commands(os, &launchd_domain(native));
        let report = |installed: bool, running: bool, message: String| ServiceReport {
            installed,
            running,
            tunnel: if running { "unknown" } else { "down" },
            provider: provider.clone(),
            url: None,
            service_file: layout.service_file.display().to_string(),
            logs: layout.log_file.display().to_string(),
            message,
            needs: None,
        };

        if args.dry_run {
            ui.say(format!("Dry run of `mcp service uninstall` ({}). Nothing is changed.\n", os.manager()));
            for c in &unload {
                ui.say(format!("    {}", shell_join(c)));
            }
            ui.say(format!("    rm {}", shell_join(&[layout.service_file.display().to_string()])));
            if os == ServiceOs::Linux {
                ui.say("    systemctl --user daemon-reload");
            }
            ui.say(format!("    rm {}", shell_join(&[layout.state_file.display().to_string()])));
            ui.say(format!("  keeps {} and the binary", layout.token_file.display()));
            return Ok(report(
                installed,
                runtime.running,
                if installed {
                    "Dry run: would stop and remove the background service, keeping the token and binary.".to_string()
                } else {
                    "Dry run: the background service is not installed, so there is nothing to remove.".to_string()
                },
            ));
        }

        if !installed {
            ui.say("The background service is not installed; nothing to remove.");
            return Ok(report(false, false, "The background service is not installed; nothing to remove.".to_string()));
        }
        for c in &unload {
            if let Err(e) = run_checked(c).await {
                if runtime.loaded {
                    ui.say(format!("  note     {e:#}"));
                }
            } else {
                ui.say(format!("  stop     {}", shell_join(c)));
            }
        }
        if layout.service_file.exists() {
            std::fs::remove_file(&layout.service_file)
                .with_context(|| format!("remove {}", layout.service_file.display()))?;
            ui.say(format!("  removed  {}", layout.service_file.display()));
        }
        if os == ServiceOs::Linux {
            let _ = run_checked(&["systemctl".to_string(), "--user".to_string(), "daemon-reload".to_string()]).await;
        }
        let _ = std::fs::remove_file(&layout.state_file);
        ui.say("");
        ui.say(format!(
            "Stopped and removed the background service. Kept the token ({}){}.",
            layout.token_file.display(),
            binary.map(|b| format!(" and the binary ({b})")).unwrap_or_default()
        ));
        ui.say("Reinstalling brings the same URL back. To retire the URL, remove the connector in claude.ai, or reinstall with --new-token.");
        let running = query_service(os).await.running;
        Ok(report(
            false,
            running,
            "Stopped and removed the background service; the token and binary are kept.".to_string(),
        ))
    }

    async fn service_status_cmd(args: McpServiceQueryArgs) -> Result<()> {
        let os = ServiceOs::resolve(None, false)?;
        let layout = ServiceLayout::discover(os)?;
        let view = gather_status(&layout).await;
        let healthy = view.report.running && view.report.tunnel == "up";
        if args.json {
            print_json(&view.report);
            if !healthy {
                std::process::exit(1);
            }
            return Ok(());
        }

        let r = &view.report;
        println!("market-search background service ({})", os.manager());
        if !r.installed {
            println!("  service  not installed");
            println!();
            println!("Install it with: market-search mcp service install");
            std::process::exit(1);
        }
        let service_line = match (view.runtime.running, view.runtime.pid) {
            (true, Some(pid)) => format!("running (pid {pid})"),
            (true, None) => "running".to_string(),
            (false, _) if view.runtime.loaded => "loaded but not running".to_string(),
            (false, _) => "installed but not loaded".to_string(),
        };
        println!("  service  {service_line}");
        println!("  file     {}", r.service_file);
        match view.local_ok {
            Some(true) => println!("  local    http://127.0.0.1:{}/ answers", view.port),
            Some(false) => println!("  local    http://127.0.0.1:{}/ does not answer", view.port),
            None => {}
        }
        let api = view.state.as_ref().and_then(|s| s.api_addr.clone());
        let public = view.state.as_ref().and_then(|s| s.public_url.clone());
        let via = match (r.provider.as_str(), &api) {
            ("ngrok", Some(a)) => format!(" (checked with ngrok's local API at {a})"),
            _ => String::new(),
        };
        println!(
            "  tunnel   {} {}{}{}",
            r.provider,
            r.tunnel,
            public.map(|u| format!(": {u}")).unwrap_or_default(),
            via
        );
        if let Some(url) = &r.url {
            println!("  url      {}  (full URL: market-search mcp service url)", redact_url_token(url));
        }
        if let Some(err) = view.state.as_ref().and_then(|s| s.last_error.clone()) {
            if r.tunnel != "up" {
                println!("  error    {err}");
            }
        }
        println!("  logs     {}", r.logs);
        if layout.os == ServiceOs::Linux {
            if let Some(note) = linger_note() {
                println!("  note     {note}");
            }
        }
        println!();
        println!("{}", r.message);
        if !healthy {
            std::process::exit(1);
        }
        Ok(())
    }

    async fn service_url_cmd(args: McpServiceQueryArgs) -> Result<()> {
        let os = ServiceOs::resolve(None, false)?;
        let layout = ServiceLayout::discover(os)?;
        let view = gather_status(&layout).await;
        if args.json {
            let mut report = view.report;
            if report.url.is_none() && report.installed {
                report.message = "No connector URL yet: the service hasn't reported a public URL.".to_string();
            }
            let has_url = report.url.is_some();
            print_json(&report);
            if !has_url {
                std::process::exit(1);
            }
            return Ok(());
        }
        match &view.report.url {
            Some(url) => {
                println!("{url}");
                if !view.report.running || view.report.tunnel == "down" {
                    eprintln!("note: {}", view.report.message);
                }
                Ok(())
            }
            None if !view.report.installed => {
                anyhow::bail!("the background service is not installed. Run: market-search mcp service install")
            }
            None => anyhow::bail!(
                "no connector URL yet: the service hasn't reported a public URL. Check `market-search mcp service status`."
            ),
        }
    }

    // ── the service process: `mcp share --service` ──────────────────────────────

    fn svc_log(msg: impl std::fmt::Display) {
        eprintln!("[{}] {msg}", now_utc());
    }

    async fn service_shutdown_signal() {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut term) => {
                    tokio::select! {
                        _ = term.recv() => {}
                        r = tokio::signal::ctrl_c() => {
                            if r.is_err() {
                                term.recv().await;
                            }
                        }
                    }
                }
                Err(_) => {
                    if tokio::signal::ctrl_c().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// macOS: hold a "no system sleep on AC power" assertion for as long as this
    /// process lives (`-w` releases it when we exit).
    fn keep_awake() -> Option<tokio::process::Child> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        match TokioCommand::new("/usr/bin/caffeinate")
            .args(["-s", "-w", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => {
                svc_log("caffeinate -s: this Mac won't sleep while it is on power");
                Some(child)
            }
            Err(e) => {
                svc_log(format!("caffeinate unavailable ({e}); if the Mac sleeps, the tunnel drops"));
                None
            }
        }
    }

    fn forward_lines<R>(reader: Option<R>, tx: tokio::sync::mpsc::UnboundedSender<String>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let Some(reader) = reader else { return };
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt as _;
            let mut lines = tokio::io::BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }

    enum TunnelExit {
        Stop,
        ServerDied,
        Restart(String),
    }

    /// The long-lived service mode. Runs until SIGTERM/SIGINT; the tunnel child is
    /// restarted (with backoff) whenever it exits or its tunnel disappears.
    pub(crate) async fn run_share_service(args: ShareArgs) -> Result<()> {
        let kind = TunnelKind::parse(args.provider.as_deref().unwrap_or("ngrok"))?;
        let os = ServiceOs::resolve(None, false)?;
        let layout = ServiceLayout::discover(os)?;
        let port = args.port;
        let domain = args.domain.as_deref().map(normalize_ngrok_domain);
        let (token, source) = resolve_service_token(&layout)?;
        svc_log(format!(
            "market-search {} service starting: provider={} port={port} token from {source}",
            env!("CARGO_PKG_VERSION"),
            kind.name()
        ));

        // 1. The HTTP server: loopback only, token required.
        let auth = McpHttpAuth::new(Some(token));
        let server = tokio::spawn(async move { serve_mcp_http("127.0.0.1", port, auth, &source).await });
        let client = local_client();
        let mut healthy = false;
        for _ in 0..50 {
            if server.is_finished() {
                break;
            }
            if local_health(&client, port).await {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if !healthy {
            if server.is_finished() {
                return match server.await {
                    Ok(Err(e)) => Err(e.context(format!("the local HTTP server could not start on 127.0.0.1:{port}"))),
                    _ => Err(anyhow::anyhow!("the local HTTP server stopped during startup")),
                };
            }
            anyhow::bail!("the local HTTP server did not answer on 127.0.0.1:{port} within 10 seconds");
        }

        // 2. Stay awake on power (macOS).
        let _caffeinate = keep_awake();

        // 3. Stop on SIGTERM (launchd bootout, systemctl stop) or Ctrl-C.
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            service_shutdown_signal().await;
            let _ = stop_tx.send(true);
        });

        // 4. Supervise the tunnel.
        let mut state = ServiceState {
            provider: kind.name().to_string(),
            status: "starting".to_string(),
            local_port: port,
            pid: std::process::id(),
            started_at: now_utc(),
            ..ServiceState::default()
        };
        write_state(&layout.state_file, &mut state);
        let mut backoff = Duration::from_secs(5);
        loop {
            if *stop_rx.borrow() {
                break;
            }
            let started = Instant::now();
            match run_tunnel_once(kind, port, domain.as_deref(), &layout, &mut state, &client, &mut stop_rx, &server).await {
                TunnelExit::Stop => break,
                TunnelExit::ServerDied => {
                    state.status = "stopped".to_string();
                    state.last_error = Some("the local HTTP server stopped".to_string());
                    write_state(&layout.state_file, &mut state);
                    anyhow::bail!("the local HTTP server stopped; exiting so the service manager restarts us");
                }
                TunnelExit::Restart(reason) => {
                    svc_log(format!("tunnel down: {reason}"));
                    if started.elapsed() > Duration::from_secs(600) {
                        backoff = Duration::from_secs(5);
                    }
                    state.status = "reconnecting".to_string();
                    if state.last_error.is_none() {
                        state.last_error = Some(truncate_chars(&reason, 300));
                    }
                    write_state(&layout.state_file, &mut state);
                    svc_log(format!("restarting the tunnel in {}s", backoff.as_secs()));
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = stop_rx.changed() => break,
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(300));
                }
            }
        }
        state.status = "stopped".to_string();
        write_state(&layout.state_file, &mut state);
        svc_log("stopped");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_tunnel_once(
        kind: TunnelKind,
        port: u16,
        domain: Option<&str>,
        layout: &ServiceLayout,
        state: &mut ServiceState,
        client: &reqwest::Client,
        stop_rx: &mut tokio::sync::watch::Receiver<bool>,
        server: &tokio::task::JoinHandle<Result<()>>,
    ) -> TunnelExit {
        let Some(bin) = find_tool(kind.binary(), &layout.home) else {
            return TunnelExit::Restart(format!(
                "{} not found on PATH or in ~/.local/bin, /opt/homebrew/bin, /usr/local/bin",
                kind.binary()
            ));
        };
        let args = kind.command_args(port, domain);
        svc_log(format!("starting tunnel: {} {}", bin.display(), args.join(" ")));
        let mut child = match TokioCommand::new(&bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return TunnelExit::Restart(format!("could not start {}: {e}", bin.display())),
        };
        let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
        forward_lines(child.stdout.take(), tx.clone());
        forward_lines(child.stderr.take(), tx);

        let mut obs = TunnelObservation::default();
        state.status = "starting".to_string();
        state.api_addr = None;
        let every = tunnel_check_every();
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let url_deadline = Instant::now() + Duration::from_secs(60);
        let mut misses = 0u32;

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    let _ = child.kill().await;
                    return TunnelExit::Stop;
                }
                status = child.wait() => {
                    // Let the readers flush the last lines, which usually hold the reason.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    while let Ok(line) = lines.try_recv() {
                        observe_tunnel_line(kind, &line, &mut obs);
                    }
                    if obs.last_error.is_some() {
                        state.last_error = obs.last_error.clone();
                    }
                    let status = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                    let why = obs.last_error.as_deref().map(|e| format!(": {e}")).unwrap_or_default();
                    return TunnelExit::Restart(format!("{} exited ({status}){why}", kind.binary()));
                }
                Some(line) = lines.recv() => {
                    let before = obs.clone();
                    observe_tunnel_line(kind, &line, &mut obs);
                    if obs == before {
                        continue;
                    }
                    if obs.last_error != before.last_error {
                        if let Some(e) = &obs.last_error {
                            svc_log(format!("{}: {e}", kind.binary()));
                            state.last_error = Some(e.clone());
                        }
                    }
                    if obs.public_url != before.public_url {
                        if let Some(url) = &obs.public_url {
                            svc_log(format!("tunnel up: {url}"));
                            state.status = "up".to_string();
                            state.public_url = Some(url.clone());
                            state.last_error = None;
                        }
                    }
                    state.api_addr = obs.api_addr.clone();
                    write_state(&layout.state_file, state);
                }
                _ = ticker.tick() => {
                    if server.is_finished() {
                        let _ = child.kill().await;
                        return TunnelExit::ServerDied;
                    }
                    if kind == TunnelKind::Ngrok && obs.public_url.is_some() && obs.api_addr.is_none() {
                        // No "starting web service" line seen: try ngrok's default API
                        // port, trusting it only if it lists a tunnel to our port.
                        if let Ok(body) = ngrok_api_tunnels(client, "127.0.0.1:4040").await {
                            if ngrok_tunnel_for_port(&body, port).is_some() {
                                obs.api_addr = Some("127.0.0.1:4040".to_string());
                                state.api_addr = obs.api_addr.clone();
                                write_state(&layout.state_file, state);
                            }
                        }
                    }
                    if obs.public_url.is_none() {
                        if Instant::now() >= url_deadline {
                            let _ = child.kill().await;
                            let why = obs.last_error.as_deref().map(|e| format!(": {e}")).unwrap_or_default();
                            return TunnelExit::Restart(format!("{} printed no public URL within 60s{why}", kind.binary()));
                        }
                        continue;
                    }
                    match probe_tunnel(kind, &obs, port, client).await {
                        TunnelProbe::Up(url) => {
                            misses = 0;
                            if let Some(u) = url {
                                if state.public_url.as_deref() != Some(u.as_str()) || state.status != "up" {
                                    svc_log(format!("tunnel up: {u}"));
                                    state.public_url = Some(u);
                                    state.status = "up".to_string();
                                    write_state(&layout.state_file, state);
                                }
                            }
                        }
                        TunnelProbe::Missing(why) => {
                            misses += 1;
                            svc_log(format!("tunnel check {misses}/2 failed: {why}"));
                            if misses >= 2 {
                                let _ = child.kill().await;
                                return TunnelExit::Restart(why);
                            }
                        }
                        TunnelProbe::Unknown => {}
                    }
                }
            }
        }
    }

    // ── tests ───────────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("ms-service-test-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn layout_for(os: ServiceOs, home: &Path) -> ServiceLayout {
            let (config, data) = match os {
                ServiceOs::Macos => {
                    let d = home.join("Library/Application Support/dev.eli.eli");
                    (d.clone(), d)
                }
                ServiceOs::Linux => (home.join(".config/eli"), home.join(".local/share/eli")),
            };
            ServiceLayout::build(os, home, &config, &data, None, None)
        }

        fn fake_spec(os: ServiceOs, home: &Path) -> ServiceSpec {
            let layout = layout_for(os, home);
            let env = |k: &str| match k {
                "ELI_SEC_USER_AGENT" => Some("Jane Doe jane@example.com".to_string()),
                "EIA_API_KEY" => Some("abc&123%".to_string()),
                _ => None,
            };
            build_service_spec(
                &layout,
                &layout.stable_bin,
                TunnelKind::Ngrok,
                8485,
                None,
                &[PathBuf::from("/opt/homebrew/bin")],
                &env,
            )
        }

        #[test]
        fn token_is_43_url_safe_chars_and_random() {
            let a = generate_token();
            let b = generate_token();
            assert_eq!(a.len(), 43);
            assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
            assert_ne!(a, b);
            assert_eq!(mcp_path_encode(&a), a, "generated tokens need no URL encoding");
        }

        #[cfg(unix)]
        #[test]
        fn token_file_is_owner_only_and_round_trips() {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = scratch("token");
            let path = dir.join("nested/mcp-token");
            assert_eq!(read_token_file(&path).unwrap(), None);
            write_private_file(&path, "first").unwrap();
            write_private_file(&path, "second\n").unwrap();
            assert_eq!(read_token_file(&path).unwrap().as_deref(), Some("second"));
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn binary_plan_copies_from_ephemeral_or_protected_places() {
            let home = PathBuf::from("/Users/jane");
            let layout = layout_for(ServiceOs::Macos, &home);
            let temps = vec![PathBuf::from("/private/var/folders"), PathBuf::from("/tmp")];
            let copy_reason = |current: &str| match plan_binary(Path::new(current), None, &layout, &temps).unwrap() {
                BinaryPlan::Copy { to, reason, .. } => {
                    assert_eq!(to, home.join(".local/bin/market-search"));
                    Some(reason)
                }
                BinaryPlan::Keep(_) => None,
            };
            assert!(copy_reason("/Users/jane/Downloads/market-search").unwrap().contains("Downloads"));
            assert!(copy_reason("/Users/jane/Desktop/x/market-search").unwrap().contains("Desktop"));
            assert!(copy_reason("/Users/jane/Documents/code/market-search").is_some());
            assert!(copy_reason("/Users/jane/src/market-search/target/debug/market-search").unwrap().contains("cargo"));
            assert!(copy_reason("/Users/jane/src/ms/target/aarch64-apple-darwin/release/market-search").is_some());
            assert!(copy_reason("/Users/jane/.npm/_npx/0a1b/node_modules/market-search/bin/market-search").unwrap().contains("npm"));
            assert!(copy_reason("/tmp/build/market-search").unwrap().contains("temporary"));
            assert!(copy_reason("/private/var/folders/xy/T/market-search").is_some());
            assert!(copy_reason("/Applications/Market Search.app/Contents/MacOS/market-search").unwrap().contains("app bundle"));
            assert!(copy_reason("/Volumes/USB/market-search").is_some());
            // Stable places stay put.
            assert_eq!(copy_reason("/Users/jane/.cargo/bin/market-search"), None);
            assert_eq!(copy_reason("/usr/local/bin/market-search"), None);
            assert_eq!(copy_reason("/opt/homebrew/bin/market-search"), None);
            assert_eq!(copy_reason("/Users/jane/.local/bin/market-search"), None);
        }

        #[test]
        fn binary_plan_honours_explicit_binary() {
            let home = PathBuf::from("/Users/jane");
            let layout = layout_for(ServiceOs::Macos, &home);
            let current = Path::new("/Applications/Market Search.app/Contents/MacOS/market-search");
            let plan = plan_binary(current, Some(Path::new("~/.local/bin/market-search")), &layout, &[]).unwrap();
            assert_eq!(
                plan,
                BinaryPlan::Copy {
                    from: current.to_path_buf(),
                    to: home.join(".local/bin/market-search"),
                    reason: "--binary names this path".to_string()
                }
            );
            // Same file: nothing to copy.
            let plan = plan_binary(Path::new("/usr/local/bin/market-search"), Some(Path::new("/usr/local/bin/market-search")), &layout, &[]).unwrap();
            assert_eq!(plan, BinaryPlan::Keep(PathBuf::from("/usr/local/bin/market-search")));
            // A protected destination is refused on macOS.
            assert!(plan_binary(current, Some(Path::new("/Users/jane/Downloads/ms")), &layout, &[]).is_err());
            assert!(plan_binary(current, Some(Path::new("relative/ms")), &layout, &[]).is_err());
        }

        #[cfg(unix)]
        #[test]
        fn install_binary_copies_into_an_injected_home() {
            use std::os::unix::fs::PermissionsExt as _;
            let home = scratch("home");
            let downloads = home.join("Downloads");
            std::fs::create_dir_all(&downloads).unwrap();
            let current = downloads.join("market-search");
            std::fs::write(&current, b"#!/bin/sh\necho fake\n").unwrap();
            let layout = layout_for(ServiceOs::Macos, &home);
            // The scratch home itself lives in the OS temp dir, so pass no temp dirs:
            // the Downloads rule alone must trigger the copy.
            let plan = plan_binary(&current, None, &layout, &[]).unwrap();
            assert!(matches!(&plan, BinaryPlan::Copy { to, .. } if to == &home.join(".local/bin/market-search")));
            install_binary(&plan).unwrap();
            let dest = home.join(".local/bin/market-search");
            assert_eq!(std::fs::read(&dest).unwrap(), b"#!/bin/sh\necho fake\n");
            assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
            // Re-planning from the copy keeps it.
            assert_eq!(plan_binary(&dest, None, &layout, &[]).unwrap(), BinaryPlan::Keep(layout.stable_bin.clone()));
            // A second install replaces the file in place of the old one.
            install_binary(&plan).unwrap();
            assert!(dest.is_file());
            std::fs::remove_dir_all(home).unwrap();
        }

        #[test]
        fn layout_paths_per_platform() {
            let mac = layout_for(ServiceOs::Macos, Path::new("/Users/jane"));
            assert_eq!(mac.service_file, PathBuf::from("/Users/jane/Library/LaunchAgents/com.eliterminal.market-search.plist"));
            assert_eq!(mac.log_file, PathBuf::from("/Users/jane/Library/Logs/market-search.log"));
            assert_eq!(mac.token_file, PathBuf::from("/Users/jane/Library/Application Support/dev.eli.eli/mcp-token"));
            let linux = layout_for(ServiceOs::Linux, Path::new("/home/jane"));
            assert_eq!(linux.service_file, PathBuf::from("/home/jane/.config/systemd/user/market-search.service"));
            assert_eq!(linux.log_file, PathBuf::from("/home/jane/.local/state/market-search/service.log"));
            assert_eq!(linux.token_file, PathBuf::from("/home/jane/.config/eli/mcp-token"));
            assert_eq!(linux.stable_bin, PathBuf::from("/home/jane/.local/bin/market-search"));
        }

        #[test]
        fn launchd_plist_has_the_required_keys() {
            let spec = fake_spec(ServiceOs::Macos, Path::new("/Users/jane"));
            assert!(spec.private, "EIA_API_KEY is a secret, so the file must be 0600");
            let plist = render_launchd_plist(&spec);
            assert!(plist.contains("<key>Label</key>\n\t<string>com.eliterminal.market-search</string>"));
            assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
            assert!(plist.contains("<key>KeepAlive</key>\n\t<true/>"));
            assert!(plist.contains("<string>/Users/jane/.local/bin/market-search</string>"));
            assert!(plist.contains("<string>/Users/jane/Library/Logs/market-search.log</string>"));
            assert!(plist.contains("<string>abc&amp;123%</string>"), "values are XML-escaped");
            assert!(!plist.contains("MARKET_SEARCH_MCP_TOKEN"));
            // What --dry-run prints masks the copied values.
            let shown = render_launchd_plist(&spec.masked());
            assert!(!shown.contains("abc&amp;123%") && !shown.contains("jane@example.com"));
            assert!(shown.contains("<key>EIA_API_KEY</key>\n\t\t<string>(copied from this shell)</string>"));
            assert!(shown.contains("<string>/Users/jane</string>"), "HOME stays visible");
            assert_eq!(
                service_file_command(ServiceOs::Macos, &plist),
                vec!["/Users/jane/.local/bin/market-search", "mcp", "share", "--service", "--provider", "ngrok", "--port", "8485"]
            );
            // The PATH puts the tunnel tool's directory first.
            assert!(plist.contains("<string>/opt/homebrew/bin:/Users/jane/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>"));
            if cfg!(target_os = "macos") && Path::new("/usr/bin/plutil").exists() {
                let dir = scratch("plist");
                let file = dir.join("x.plist");
                std::fs::write(&file, &plist).unwrap();
                let out = std::process::Command::new("/usr/bin/plutil").arg("-lint").arg(&file).output().unwrap();
                assert!(out.status.success(), "plutil -lint: {}", String::from_utf8_lossy(&out.stdout));
                std::fs::remove_dir_all(dir).unwrap();
            }
        }

        #[test]
        fn systemd_unit_quotes_and_escapes() {
            let home = Path::new("/home/jane doe");
            let layout = layout_for(ServiceOs::Linux, home);
            let spec = build_service_spec(&layout, &layout.stable_bin, TunnelKind::Ngrok, 8485, Some("my.example.dev"), &[], &|_| None);
            assert!(!spec.private);
            let unit = render_systemd_unit(&spec);
            assert!(unit.contains("ExecStart=\"/home/jane doe/.local/bin/market-search\" mcp share --service --provider ngrok --port 8485 --domain my.example.dev\n"));
            assert!(unit.contains("Restart=always\n"));
            assert!(unit.contains("WantedBy=default.target\n"));
            assert!(unit.contains("Environment=\"HOME=/home/jane doe\"\n"));
            assert!(unit.contains("StandardOutput=append:/home/jane doe/.local/state/market-search/service.log\n"));
            assert_eq!(
                service_file_command(ServiceOs::Linux, &unit),
                vec!["/home/jane doe/.local/bin/market-search", "mcp", "share", "--service", "--provider", "ngrok", "--port", "8485", "--domain", "my.example.dev"]
            );
            assert_eq!(systemd_word("100%"), "100%%");
            assert_eq!(systemd_word("$HOME"), "$$HOME");
            assert_eq!(systemd_word("a \"b\""), "\"a \\\"b\\\"\"");
        }

        #[test]
        fn ngrok_log_lines_yield_url_api_and_errors() {
            let mut obs = TunnelObservation::default();
            observe_tunnel_line(TunnelKind::Ngrok, r#"t=2026-11-02T22:00:00+0000 lvl=info msg="starting web service" obj=web addr=127.0.0.1:4041 allow_hosts=[]"#, &mut obs);
            observe_tunnel_line(TunnelKind::Ngrok, r#"t=2026-11-02T22:00:01+0000 lvl=info msg="started tunnel" obj=tunnels name=command_line addr=http://127.0.0.1:8485 url=https://quiet-otter.ngrok-free.dev"#, &mut obs);
            assert_eq!(obs.api_addr.as_deref(), Some("127.0.0.1:4041"));
            assert_eq!(obs.public_url.as_deref(), Some("https://quiet-otter.ngrok-free.dev"));
            assert_eq!(obs.last_error, None);
            observe_tunnel_line(TunnelKind::Ngrok, r#"t=2026-11-02T22:05:00+0000 lvl=eror msg="session closing" obj=tunnels.session err="authentication failed: Your account is limited to 1 simultaneous ngrok agent sessions.\n\nERR_NGROK_108\n""#, &mut obs);
            let err = obs.last_error.clone().unwrap();
            assert_eq!(
                err,
                "authentication failed: Your account is limited to 1 simultaneous ngrok agent sessions. ERR_NGROK_108"
            );
            assert!(!ngrok_auth_error(&err), "108 is a session limit, not a bad token");
            assert!(ngrok_auth_error("ERR_NGROK_4018 authentication failed: usage requires a verified account"));
            let mut cf = TunnelObservation::default();
            observe_tunnel_line(TunnelKind::Cloudflare, "2026-11-02T22:00:00Z INF |  https://brave-lion-cat.trycloudflare.com  |", &mut cf);
            assert_eq!(cf.public_url.as_deref(), Some("https://brave-lion-cat.trycloudflare.com"));
        }

        #[test]
        fn ngrok_api_matches_only_our_port() {
            let body = json!({
                "tunnels": [
                    { "name": "other", "public_url": "https://someone-else.ngrok-free.dev", "proto": "https", "config": { "addr": "http://localhost:8484" } },
                    { "name": "command_line", "public_url": "https://quiet-otter.ngrok-free.dev", "proto": "https", "config": { "addr": "http://127.0.0.1:8485" } }
                ],
                "uri": "/api/tunnels"
            });
            assert_eq!(ngrok_tunnel_for_port(&body, 8485).as_deref(), Some("https://quiet-otter.ngrok-free.dev"));
            assert_eq!(ngrok_tunnel_for_port(&body, 9999), None);
            assert_eq!(ngrok_tunnel_for_port(&json!({"tunnels": []}), 8485), None);
        }

        #[test]
        fn service_manager_output_parsing() {
            let print = "gui/501/com.eliterminal.market-search = {\n\tactive count = 1\n\tpath = /Users/jane/Library/LaunchAgents/com.eliterminal.market-search.plist\n\ttype = LaunchAgent\n\tstate = running\n\n\tprogram = /Users/jane/.local/bin/market-search\n\tpid = 4242\n\tlast exit code = (never exited)\n}\n";
            assert_eq!(parse_launchctl_print(print), ServiceRuntime { loaded: true, running: true, pid: Some(4242) });
            let waiting = "gui/501/x = {\n\tstate = not running\n}\n";
            assert!(!parse_launchctl_print(waiting).running);
            let show = "LoadState=loaded\nActiveState=active\nMainPID=777\n";
            assert_eq!(parse_systemctl_show(show), ServiceRuntime { loaded: true, running: true, pid: Some(777) });
            let gone = "LoadState=not-found\nActiveState=inactive\nMainPID=0\n";
            assert_eq!(parse_systemctl_show(gone), ServiceRuntime::default());
        }

        #[test]
        fn ps_scan_skips_our_own_ngrok_child() {
            let ps = "  101     1 /opt/homebrew/bin/ngrok\n  202   500 /Users/jane/.local/bin/ngrok\n  303     1 /usr/sbin/sshd\n  404   600 ngrok\n";
            assert_eq!(parse_ps_for_ngrok(ps, Some(500)), vec![101, 404]);
            assert_eq!(parse_ps_for_ngrok(ps, None), vec![101, 202, 404]);
        }

        #[test]
        fn ngrok_downloads_and_authtokens() {
            assert_eq!(
                ngrok_download_url(ServiceOs::Macos, "aarch64", "zip").unwrap(),
                "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-darwin-arm64.zip"
            );
            assert_eq!(
                ngrok_download_url(ServiceOs::Linux, "x86_64", "tgz").unwrap(),
                "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-linux-amd64.tgz"
            );
            assert!(ngrok_download_url(ServiceOs::Linux, "riscv64", "zip").is_err());
            let t = "2abcDEFghiJKLmnoPQRstu_VWXyz0123456789abcdEFGH";
            assert_eq!(clean_ngrok_authtoken(t).as_deref(), Some(t));
            assert_eq!(clean_ngrok_authtoken(&format!("ngrok config add-authtoken {t}\n")).as_deref(), Some(t));
            assert_eq!(clean_ngrok_authtoken("short"), None);
            assert_eq!(clean_ngrok_authtoken("has spaces? no!"), None);
            assert_eq!(
                TunnelKind::Ngrok.command_args(8485, None),
                vec!["http", "127.0.0.1:8485", "--inspect=false", "--log=stdout", "--log-format=logfmt"]
            );
            assert_eq!(
                TunnelKind::Cloudflare.command_args(8485, None),
                vec!["tunnel", "--url", "http://127.0.0.1:8485", "--no-autoupdate"]
            );
            assert_eq!(normalize_ngrok_domain("quiet-otter"), "quiet-otter.ngrok-free.dev");
            assert_eq!(normalize_ngrok_domain("https://quiet-otter.ngrok-free.dev/"), "quiet-otter.ngrok-free.dev");
        }

        #[test]
        fn ngrok_authtoken_detection_reads_presence_only() {
            let home = scratch("ngrokcfg");
            assert_eq!(ngrok_authtoken_file(&home, ServiceOs::Linux), None);
            let cfg = home.join(".config/ngrok/ngrok.yml");
            std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
            std::fs::write(&cfg, "version: \"3\"\nagent:\n    authtoken: 2abcDEFghiJKLmnoPQRstu_VWXyz\n").unwrap();
            assert_eq!(ngrok_authtoken_file(&home, ServiceOs::Linux), Some(cfg.clone()));
            std::fs::write(&cfg, "version: \"2\"\n").unwrap();
            assert_eq!(ngrok_authtoken_file(&home, ServiceOs::Linux), None);
            std::fs::remove_dir_all(home).unwrap();
        }

        #[tokio::test]
        async fn wait_for_tunnel_reads_the_state_file() {
            let home = scratch("wait");
            let layout = layout_for(ServiceOs::Macos, &home);
            let ui = Ui { json: true, interactive: false, may_open_browser: false };
            let mut state = ServiceState {
                provider: "ngrok".to_string(),
                status: "reconnecting".to_string(),
                last_error: Some("authentication failed: ERR_NGROK_4018".to_string()),
                ..ServiceState::default()
            };
            write_state(&layout.state_file, &mut state);
            let err = wait_for_tunnel(&layout, TunnelKind::Ngrok, None, &ui).await.unwrap_err();
            assert_eq!(err.downcast_ref::<NeedsInput>().map(|n| n.needs), Some("ngrok_authtoken"));
            state.last_error = Some("ERR_NGROK_108".to_string());
            write_state(&layout.state_file, &mut state);
            let err = wait_for_tunnel(&layout, TunnelKind::Ngrok, None, &ui).await.unwrap_err();
            assert!(err.downcast_ref::<NeedsInput>().is_none());
            assert!(format!("{err}").contains("one"), "{err}");
            state.status = "up".to_string();
            state.public_url = Some("https://quiet-otter.ngrok-free.dev".to_string());
            state.last_error = None;
            write_state(&layout.state_file, &mut state);
            let up = wait_for_tunnel(&layout, TunnelKind::Ngrok, None, &ui).await.unwrap();
            assert_eq!(up.public_url.as_deref(), Some("https://quiet-otter.ngrok-free.dev"));
            // The old process's state (same pid) is ignored; give the wait a short deadline.
            let old = tokio::time::timeout(
                Duration::from_millis(1500),
                wait_for_tunnel(&layout, TunnelKind::Ngrok, Some(state.pid), &ui),
            )
            .await;
            assert!(old.is_err(), "a state file from the old pid must not count");
            assert_eq!(join_clauses(&["a".into(), "b".into(), "c".into()]), "a, b, and c");
            std::fs::remove_dir_all(home).unwrap();
        }

        #[test]
        fn report_json_has_exactly_the_contract_keys() {
            let report = ServiceReport {
                installed: true,
                running: true,
                tunnel: "up",
                provider: "ngrok".to_string(),
                url: Some("https://quiet-otter.ngrok-free.dev/mcp/abc".to_string()),
                service_file: "/x.plist".to_string(),
                logs: "/x.log".to_string(),
                message: "ok".to_string(),
                needs: None,
            };
            let v = serde_json::to_value(&report).unwrap();
            let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
            keys.sort();
            assert_eq!(keys, vec!["installed", "logs", "message", "needs", "provider", "running", "service_file", "tunnel", "url"]);
            assert!(v["needs"].is_null());
            let blank = serde_json::to_value(ServiceReport::blank("ngrok")).unwrap();
            assert!(blank["url"].is_null());
            assert_eq!(redact_url_token("https://h.dev/mcp/abcdefghij"), "https://h.dev/mcp/abcd…");
        }
    }
}

use mcp_service::{cmd_mcp_service, run_share_service};
