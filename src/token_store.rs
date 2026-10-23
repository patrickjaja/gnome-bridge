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
    /// The store, creating its directory (for saving a new token).
    pub fn new() -> Result<Self> {
        let store = Self::locate()?;
        if let Some(dir) = store.path.parent() {
            fs::create_dir_all(dir).context("failed to create token store directory")?;
        }
        Ok(store)
    }

    /// The store at its contract path
    /// (`${XDG_STATE_HOME:-$HOME/.local/state}/gnome-portal-bridge/restore-token.json`),
    /// without touching the filesystem: `doctor` and `forget-session` must
    /// not create state directories.
    pub fn locate() -> Result<Self> {
        Ok(Self {
            path: state_dir()?.join("restore-token.json"),
        })
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

/// `${XDG_STATE_HOME:-$HOME/.local/state}/gnome-portal-bridge`: an empty
/// `XDG_STATE_HOME` counts as unset, as with the shell's `:-`.
fn state_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(base) => PathBuf::from(base),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .context("HOME is not set")?;
            PathBuf::from(home).join(".local/state")
        }
    };
    Ok(base.join("gnome-portal-bridge"))
}
