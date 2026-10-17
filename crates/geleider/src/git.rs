//! Working out which Project and worktree a caller's directory belongs to.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
            // A `.git` file can point any directory at the clone's repository, so
            // the shared repository alone does not make it the Project's. Only the
            // clone and the worktrees it registered are.
            if !worktrees(&clone)?.iter().any(|w| w.path == worktree) {
                bail!(
                    "{} is not the clone or a worktree of project `{name}` \
                     (see `git -C {} worktree list`)",
                    worktree.display(),
                    clone.display()
                );
            }
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

/// Runs git in `dir`. The background process runs outside any sandbox, while
/// `dir` comes from the caller and may hold a repository an agent wrote, so git's
/// settings that run commands are switched off and it never prompts.
fn git(dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("running git")
}

/// git's output, or its error message if it failed.
fn git_stdout(dir: &Path, args: &[&str]) -> Result<String> {
    let output = git(dir, args)?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// The worktree root and the shared `.git` directory of the repo at `dir`.
fn repo_dirs(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let text = git_stdout(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
    )?;
    let mut lines = text.lines();
    let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
        bail!("unexpected output from git rev-parse");
    };
    Ok((canonical(top), canonical(common)))
}

/// The repository root, including for a Project not declared in config.
pub fn worktree(cwd: &Path) -> Option<PathBuf> {
    repo_dirs(cwd).ok().map(|(root, _)| root)
}

#[derive(Debug, Default)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub protected: bool,
}

/// The clone itself and every registered worktree, including paths with spaces,
/// quotes or newlines. The primary, locked, bare and prunable entries are protected.
pub fn worktrees(clone: &Path) -> Result<Vec<Worktree>> {
    let text = git_stdout(clone, &["worktree", "list", "--porcelain", "-z"])?;
    let mut result = Vec::new();
    for record in text.split("\0\0").filter(|r| !r.is_empty()) {
        let mut wt = Worktree::default();
        for field in record.split('\0') {
            if let Some(path) = field.strip_prefix("worktree ") {
                wt.path = canonical(path);
            } else if let Some(head) = field.strip_prefix("HEAD ") {
                wt.head = head.to_string();
            } else if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                wt.branch = Some(branch.to_string());
            } else if field == "bare"
                || field.starts_with("locked")
                || field.starts_with("prunable")
            {
                wt.protected = true;
            }
        }
        if wt.path.as_os_str().is_empty() {
            bail!("unexpected output from git worktree list");
        }
        wt.protected |= result.is_empty() || wt.path == clone.canonicalize()?;
        result.push(wt);
    }
    Ok(result)
}

/// No staged, unstaged, untracked or ignored files to lose. Explicit options
/// override repository settings that might otherwise hide untracked files.
pub fn clean(worktree: &Path) -> Result<bool> {
    // These index flags let `status` hide local edits. Sparse worktrees are also
    // kept: conservatively refuse rather than treating hidden files as clean.
    let files = git_stdout(worktree, &["ls-files", "-v", "-z"])?;
    if files.split('\0').any(|entry| {
        entry
            .as_bytes()
            .first()
            .is_some_and(|flag| flag.is_ascii_lowercase() || *flag == b'S')
    }) {
        return Ok(false);
    }
    Ok(git_stdout(
        worktree,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )?
    .is_empty())
}

/// Never force removal: Git itself checks for changes, locks and submodules too.
pub fn remove_worktree(clone: &Path, worktree: &Path) -> Result<()> {
    git_stdout(
        clone,
        &[
            "worktree",
            "remove",
            "--",
            worktree.to_str().context("worktree path is not UTF-8")?,
        ],
    )?;
    Ok(())
}

/// The branch checked out in `worktree`, or `None` on a detached HEAD.
pub fn branch(worktree: &Path) -> Option<String> {
    let name = git_stdout(worktree, &["symbolic-ref", "--short", "-q", "HEAD"]).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
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
        run_git(&clone, &["init", "-q", "-b", "main"]);
        run_git(
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
        run_git(
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
        let found = locate(&config, &clone).unwrap();
        assert_eq!(found.branch.as_deref(), Some("main"));
    }

    #[test]
    fn a_directory_pointed_at_the_clone_is_not_one_of_its_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("queendom");
        std::fs::create_dir(&clone).unwrap();
        run_git(&clone, &["init", "-q", "-b", "main"]);

        // What an agent could write anywhere it may write: a worktree-style git
        // dir whose `commondir` names the clone's repository.
        let fake_gitdir = dir.path().join("fake-gitdir");
        std::fs::create_dir(&fake_gitdir).unwrap();
        std::fs::write(fake_gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            fake_gitdir.join("commondir"),
            format!("{}\n", clone.join(".git").display()),
        )
        .unwrap();
        let crafted = dir.path().join("crafted");
        std::fs::create_dir(&crafted).unwrap();
        std::fs::write(
            crafted.join(".git"),
            format!("gitdir: {}\n", fake_gitdir.display()),
        )
        .unwrap();
        std::fs::write(
            fake_gitdir.join("gitdir"),
            crafted.join(".git").display().to_string(),
        )
        .unwrap();
        // git itself takes it as sharing the clone's repository.
        let (_, common) = repo_dirs(&crafted).unwrap();
        assert_eq!(common, std::fs::canonicalize(clone.join(".git")).unwrap());

        let config = Config::parse(&format!(
            "[projects.queendom]\nrepo = \"Eloyye/queendom\"\npath = \"{}\"\n",
            clone.display()
        ))
        .unwrap();
        let err = locate(&config, &crafted).unwrap_err();
        assert!(
            format!("{err:#}").contains("not the clone or a worktree"),
            "{err:#}"
        );
    }
}
