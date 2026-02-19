impl Paths {
    pub fn discover() -> Result<Self> {
        if let Ok(home) = std::env::var("ELI_HOME") {
            let base = PathBuf::from(home);
            return Ok(Self {
                config_dir: base.join("config"),
                data_dir: base.join("data"),
                cache_dir: base.join("cache"),
            });
        }

        if let Some(dirs) = ProjectDirs::from("dev", "eli", "eli") {
            return Ok(Self {
                config_dir: dirs.config_dir().to_path_buf(),
                data_dir: dirs.data_dir().to_path_buf(),
                cache_dir: dirs.cache_dir().to_path_buf(),
            });
        }

        let home = std::env::var("HOME").map_err(|_| {
            Error::InvalidConfig("could not determine HOME for config paths".to_string())
        })?;
        let base = PathBuf::from(home).join(".eli");
        Ok(Self {
            config_dir: base.join("config"),
            data_dir: base.join("data"),
            cache_dir: base.join("cache"),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        if let Ok(p) = std::env::var("ELI_CONFIG") {
            return PathBuf::from(p);
        }
        self.config_dir.join("config.toml")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.cache_dir)?;
        Ok(())
    }
}

pub fn load_or_default(paths: &Paths) -> Result<ConfigFile> {
    let path = paths.config_file();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ConfigFile::default()),
        Err(e) => return Err(Error::Io(e)),
    };
    Ok(toml::from_str(&raw)?)
}

pub fn load_or_create(paths: &Paths) -> Result<ConfigFile> {
    paths.ensure_dirs()?;
    let cfg = load_or_default(paths)?;
    if !paths.config_file().exists() {
        save(paths, &cfg)?;
    }
    Ok(cfg)
}

pub fn save(paths: &Paths, cfg: &ConfigFile) -> Result<()> {
    paths.ensure_dirs()?;
    let contents = toml::to_string_pretty(cfg)?;
    let path = paths.config_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

