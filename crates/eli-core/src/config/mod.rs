use crate::{types::ProviderKind, Error, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub chat: ChatConfig,
    #[serde(default)]
    pub finance: FinanceConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct FinanceConfig {
    #[serde(default)]
    pub fred_api_key: Option<String>,
}

include!("model.rs");
include!("io.rs");
include!("defaults.rs");
