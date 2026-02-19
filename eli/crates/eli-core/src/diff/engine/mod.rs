use crate::contract::{DiffOp, FileDiff};
use crate::diff::safe_join;
use crate::{Error, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffResult {
    pub path: String,
    pub op: String,
    pub success: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub backup_path: Option<String>,
    #[serde(default)]
    pub preview: bool,
    #[serde(default)]
    pub diff: Option<String>,
}

pub struct DiffEngine {
    project_root: PathBuf,
}

include!("engine_impl.rs");
include!("helpers.rs");
include!("undo.rs");
