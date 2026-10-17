//! Where geleider keeps its config and state on disk.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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

    /// Creates the state directory readable only by the user, and tightens one an
    /// earlier version made. Run logs can hold secrets a Job printed, and the socket
    /// accepts Job submissions, so no other account may reach either.
    pub fn ensure_state_dir(&self) -> Result<()> {
        let dir = &self.state_dir;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_is_private_even_if_it_existed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            state_dir: dir.path().join("state"),
        };
        std::fs::create_dir(&paths.state_dir).unwrap();
        std::fs::set_permissions(&paths.state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        paths.ensure_state_dir().unwrap();
        let mode = std::fs::metadata(&paths.state_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
