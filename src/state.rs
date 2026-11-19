use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::wallet::WalletState;

pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_or_default(&self) -> Result<WalletState> {
        if !self.path.exists() {
            return Ok(WalletState::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read state file {}", self.path.display()))?;
        if raw.trim().is_empty() {
            return Ok(WalletState::new());
        }
        let state: WalletState = serde_json::from_str(&raw)
            .with_context(|| format!("parse state file {}", self.path.display()))?;
        Ok(state)
    }

    pub fn save(&self, state: &WalletState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let raw = serde_json::to_string_pretty(state)?;
        std::fs::write(&self.path, raw)
            .with_context(|| format!("write state file {}", self.path.display()))?;
        Ok(())
    }
}
