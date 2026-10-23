//! Persistence for the portal restore token.
//!
//! On GNOME 46+ the portal returns a restore token that lets a later session be
//! re-created without a consent dialog. We persist it under
//! `$XDG_STATE_HOME/gnome-portal-bridge/` (falling back to
//! `~/.local/state/gnome-portal-bridge/`). GNOME 43-45 does not return a token,
//! so this file simply never gets written and a dialog appears each session.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    token: String,
}

pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new() -> Result<Self> {
        let dir = state_dir()?;
        fs::create_dir_all(&dir).context("failed to create token store directory")?;
        Ok(Self {
            path: dir.join("restore-token.json"),
        })
    }

    /// The directory the restore token lives in; also reported by `doctor`.
    pub fn dir() -> Result<PathBuf> {
        state_dir()
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn load(&self) -> Result<Option<String>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let contents =
            fs::read_to_string(&self.path).context("failed to read restore token file")?;
        let stored: StoredToken =
            serde_json::from_str(&contents).context("failed to parse restore token file")?;
        Ok(Some(stored.token))
    }

    /// Delete the stored token. Returns whether there was one.
    pub fn remove(&self) -> Result<bool> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| {
                format!("failed to delete restore token `{}`", self.path.display())
            }),
        }
    }

    /// Where the token is stored.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn save(&self, token: &str) -> Result<()> {
        let payload = serde_json::to_string_pretty(&StoredToken {
            token: token.to_owned(),
        })?;
        fs::write(&self.path, payload).context("failed to write restore token file")
    }
}

fn state_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(home).join(".local/state"));
    Ok(base.join("gnome-portal-bridge"))
}
