//! Where geleider keeps its config and state on disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Locations of geleider's own files. `GELEIDER_CONFIG` and `GELEIDER_STATE_DIR`
/// override the defaults, which tests use to run an isolated daemon.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = home_dir()?;
        let config = std::env::var_os("GELEIDER_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/geleider/config.toml"));
        let state_dir = std::env::var_os("GELEIDER_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Library/Application Support/geleider"));
        Ok(Self { config, state_dir })
    }

    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("geleider.sock")
    }

    pub fn database(&self) -> PathBuf {
        self.state_dir.join("state.sqlite")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("daemon.lock")
    }

    pub fn run_logs(&self) -> PathBuf {
        self.state_dir.join("runs")
    }
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// Expands a leading `~/` to the home directory.
pub fn expand_tilde(path: &str) -> Result<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => Ok(home_dir()?.join(rest)),
        None if path == "~" => home_dir(),
        None => Ok(PathBuf::from(path)),
    }
}

/// Shortens a path under the home directory to `~/…` for display.
pub fn display(path: &Path) -> String {
    if let Ok(home) = home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}
