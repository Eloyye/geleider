//! `geleider update`: build the latest `main` from GitHub with `cargo install`, then
//! run the new binary's `install` so the background process, MCP entries and hooks
//! move to it. A Run running across that restart keeps running but ends as `lost`,
//! so update refuses while one is, unless forced.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::one_line;
use crate::paths::Paths;
use crate::protocol::{self, Request, RunInfo};
use crate::store::RunState;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const BRANCH: &str = "main";

pub fn run(paths: &Paths, force: bool) -> Result<i32> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let root = install_root(&exe)?;
    check_no_running_runs(paths, force, "Nothing was built.")?;

    println!(
        "Building geleider from {REPOSITORY} ({BRANCH}) into {}",
        root.display()
    );
    let status = Command::new("cargo")
        .args(cargo_args(&root))
        .status()
        .context("running cargo; is Rust installed?")?;
    if !status.success() {
        bail!("cargo install failed; geleider was not changed.");
    }

    // The build takes minutes; a Run may have started meanwhile.
    check_no_running_runs(
        paths,
        force,
        "The new binary is installed but not yet in use; run `geleider install` once the Run ends.",
    )?;
    let status = Command::new(root.join("bin/geleider"))
        .arg("install")
        .status()
        .context("running the new geleider's install")?;
    if !status.success() {
        bail!("the new geleider's install failed.");
    }
    println!("\nRestart running Sessions to load updated hooks and MCP tools.");
    Ok(0)
}

/// The `cargo install --root` that puts the new binary where this one is, so the
/// paths registered by `install` stay the same.
fn install_root(exe: &Path) -> Result<PathBuf> {
    match exe.parent() {
        Some(bin) if bin.file_name().is_some_and(|name| name == "bin") => Ok(bin
            .parent()
            .context("geleider has no install root")?
            .to_path_buf()),
        _ => bail!(
            "{} is not in a `bin` directory from `cargo install`; update a build directory \
             with `git pull` and `cargo build` instead.",
            exe.display()
        ),
    }
}

fn cargo_args(root: &Path) -> Vec<String> {
    [
        "install",
        "--locked",
        "--git",
        REPOSITORY,
        "--branch",
        BRANCH,
        "--root",
        &root.to_string_lossy(),
        "geleider",
    ]
    .map(String::from)
    .to_vec()
}

fn check_no_running_runs(paths: &Paths, force: bool, consequence: &str) -> Result<()> {
    // Without a background process there is nothing to restart a Run out of.
    let Ok(reply) = protocol::call(paths, &Request::ListRuns { finished: 0 }) else {
        return Ok(());
    };
    let running = running(reply.into_runs()?);
    if running.is_empty() {
        return Ok(());
    }
    if force {
        eprintln!("These Runs keep running but end as `lost`, with no exit code:\n{running}");
        return Ok(());
    }
    bail!(
        "restarting the background process would leave these Runs to end as `lost`:\n{running}{consequence} \
         Wait for them, or pass --force."
    )
}

/// One line per running Run, or empty when none is running.
fn running(runs: Vec<RunInfo>) -> String {
    runs.iter()
        .filter(|info| info.run.state == RunState::Running)
        .map(|info| format!("  {}\n", one_line(info)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_next_to_the_running_binary() {
        let root = install_root(Path::new("/Users/me/.cargo/bin/geleider")).unwrap();
        assert_eq!(root, Path::new("/Users/me/.cargo"));
        let args = cargo_args(&root);
        assert!(args.windows(2).any(|w| w == ["--root", "/Users/me/.cargo"]));
        assert!(args.windows(2).any(|w| w == ["--branch", "main"]));
        assert!(args.windows(2).any(|w| w == ["--git", REPOSITORY]));
    }

    fn info(id: i64, state: &str) -> RunInfo {
        serde_json::from_value(serde_json::json!({
            "id": id, "project": "lanternfall", "repo": "Eloyye/lanternfall", "job": "playmode-test",
            "args": [], "resource": "unity", "worktree": "/wt", "branch": "b", "argv": [],
            "submitter": "mcp", "session": null, "state": state, "submitted_at": 0,
            "started_at": 0, "ended_at": null, "pid": null, "pid_started": null,
            "exit_code": null, "error": null, "log_path": "/log", "queue_position": null,
            "log_tail": null,
        }))
        .unwrap()
    }

    #[test]
    fn only_running_runs_block_an_update() {
        assert_eq!(running(vec![info(1, "queued")]), "");
        let shown = running(vec![info(7, "running"), info(8, "queued")]);
        assert!(shown.contains("#7"));
        assert!(!shown.contains("#8"));
    }

    #[test]
    fn refuses_a_build_directory() {
        let err = install_root(Path::new("/src/geleider/target/release/geleider")).unwrap_err();
        assert!(err.to_string().contains("cargo build"));
    }
}
