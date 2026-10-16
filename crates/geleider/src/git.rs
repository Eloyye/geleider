//! Working out which Project and worktree a caller's directory belongs to.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::{Config, Project};

#[derive(Debug, Clone)]
pub struct Location<'a> {
    pub project_name: &'a str,
    pub project: &'a Project,
    pub worktree: PathBuf,
    pub branch: Option<String>,
}

/// Finds the Project whose clone `cwd` is in, directly or through one of the
/// clone's worktrees.
pub fn locate<'a>(config: &'a Config, cwd: &Path) -> Result<Location<'a>> {
    let (worktree, common_dir) = repo_dirs(cwd)
        .with_context(|| format!("{} is not inside a git repository", cwd.display()))?;
    for (name, project) in &config.projects {
        let Ok(clone) = project.clone_path() else {
            continue;
        };
        let Ok((_, project_common)) = repo_dirs(&clone) else {
            continue;
        };
        if project_common == common_dir {
            return Ok(Location {
                project_name: name,
                project,
                branch: branch(&worktree),
                worktree,
            });
        }
    }
    bail!(
        "{} is not in any project in geleider's config (projects: {})",
        worktree.display(),
        config
            .projects
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The worktree root and the shared `.git` directory of the repo at `dir`.
fn repo_dirs(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
        bail!("unexpected output from git rev-parse");
    };
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
    Ok((canonical(top), canonical(common)))
}

/// The branch checked out in `worktree`, or `None` on a detached HEAD.
pub fn branch(worktree: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    #[test]
    fn finds_project_from_a_worktree_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("queendom");
        std::fs::create_dir(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(
            &clone,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let wt = dir.path().join("wt-feat");
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat/x",
                wt.to_str().unwrap(),
            ],
        );
        std::fs::create_dir(wt.join("sub")).unwrap();

        let config = Config::parse(&format!(
            "[projects.other]\nrepo = \"a/b\"\npath = \"{}\"\n\
             [projects.queendom]\nrepo = \"Eloyye/queendom\"\npath = \"{}\"\n",
            dir.path().display(),
            clone.display()
        ))
        .unwrap();
        let found = locate(&config, &wt.join("sub")).unwrap();
        assert_eq!(found.project_name, "queendom");
        assert_eq!(found.worktree, std::fs::canonicalize(&wt).unwrap());
        assert_eq!(found.branch.as_deref(), Some("feat/x"));

        assert!(locate(&config, dir.path()).is_err());
    }
}
