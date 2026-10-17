//! Discover Finished worktrees conservatively; removal is an explicit CLI action.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::git;
use crate::github::{PrState, PullRequests};
use crate::paths;
use crate::session::{Session, SessionState};
use crate::store::Run;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishedWorktree {
    pub project: String,
    pub worktree: PathBuf,
    pub branch: String,
    pub head: String,
    pub pr: u64,
    pub pr_state: PrState,
    pub pr_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Inventory {
    pub finished_worktrees: Vec<FinishedWorktree>,
    pub cleanup_warnings: Vec<String>,
    /// `status` answered before its first scan finished, so nothing is listed yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cleanup_pending: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cleaned {
    pub removed: Vec<PathBuf>,
    pub skipped: Vec<String>,
}

pub fn busy(path: &Path, sessions: &[Session], runs: &[Run]) -> bool {
    let contains = |other: &Path| {
        other
            .canonicalize()
            .unwrap_or_else(|_| other.into())
            .starts_with(path)
    };
    sessions
        .iter()
        .any(|s| s.state != SessionState::Ended && contains(&s.worktree))
        || runs
            .iter()
            .any(|r| !r.state.is_finished() && contains(&r.worktree))
}

pub fn discover(
    config: &Config,
    sessions: &[Session],
    runs: &[Run],
    github: &dyn PullRequests,
) -> Inventory {
    let mut inventory = Inventory::default();
    for (name, project) in &config.projects {
        let scan = project
            .clone_path()
            .and_then(|clone| git::worktrees(&clone));
        let worktrees = match scan {
            Ok(worktrees) => worktrees,
            Err(err) => {
                inventory.cleanup_warnings.push(format!("{name}: {err:#}"));
                continue;
            }
        };
        for wt in worktrees {
            match finished(name, &project.repo, wt, sessions, runs, github) {
                Ok(Some(found)) => inventory.finished_worktrees.push(found),
                Ok(None) => {}
                Err(err) => inventory.cleanup_warnings.push(format!("{name} {err:#}")),
            }
        }
    }
    inventory
        .finished_worktrees
        .sort_by(|a, b| a.worktree.cmp(&b.worktree));
    inventory
        .finished_worktrees
        .dedup_by(|a, b| a.worktree == b.worktree);
    inventory
}

/// `wt` as a Finished worktree, or `None` if anything may still need it. Local
/// checks come first, so only idle, clean worktrees cost a GitHub query.
fn finished(
    project: &str,
    repo: &str,
    wt: git::Worktree,
    sessions: &[Session],
    runs: &[Run],
    github: &dyn PullRequests,
) -> Result<Option<FinishedWorktree>> {
    if wt.protected || busy(&wt.path, sessions, runs) {
        return Ok(None);
    }
    let Some(branch) = wt.branch.clone() else {
        return Ok(None);
    };
    if !git::clean(&wt).with_context(|| paths::display(&wt.path))? {
        return Ok(None);
    }
    let prs = github
        .pull_requests(repo, &branch)
        .with_context(|| branch.clone())?;
    if prs.iter().any(|pr| pr.state == PrState::Open) {
        return Ok(None);
    }
    let Some(pr) = prs
        .into_iter()
        .filter(|pr| !pr.is_cross_repository)
        .max_by_key(|pr| pr.number)
    else {
        return Ok(None);
    };
    // GitHub retains the PR head after merging/deleting its branch. Exact
    // equality proves all local commits were pushed, including with squash
    // merges, without trusting stale remote-tracking refs.
    if pr.head_ref_oid != wt.head {
        return Ok(None);
    }
    Ok(Some(FinishedWorktree {
        project: project.to_string(),
        worktree: wt.path,
        branch,
        head: wt.head,
        pr: pr.number,
        pr_state: pr.state,
        pr_url: pr.url,
    }))
}

/// Final local check with the latest Session/Run records, immediately before
/// normal Git removal. The caller has just refreshed the PR state separately.
pub fn remove(
    config: &Config,
    sessions: &[Session],
    runs: &[Run],
    approved: &FinishedWorktree,
    cwd: &Path,
) -> Result<()> {
    if cwd.canonicalize()?.starts_with(&approved.worktree) {
        bail!("the invoking directory is inside this worktree");
    }
    let clone = validate(config, sessions, runs, approved)?;
    git::remove_worktree(&clone, &approved.worktree)
}

/// Recheck local state after any potentially slow GitHub queries.
pub fn validate(
    config: &Config,
    sessions: &[Session],
    runs: &[Run],
    approved: &FinishedWorktree,
) -> Result<PathBuf> {
    if busy(&approved.worktree, sessions, runs) {
        bail!("a live Session or Run is using this worktree");
    }
    let project = config
        .projects
        .get(&approved.project)
        .ok_or_else(|| anyhow::anyhow!("Project is no longer configured"))?;
    let clone = project.clone_path()?;
    let entries = git::worktrees(&clone)?;
    let Some(wt) = entries.iter().find(|w| w.path == approved.worktree) else {
        bail!("worktree is no longer registered");
    };
    if wt.protected
        || wt.head != approved.head
        || wt.branch.as_ref() != Some(&approved.branch)
        || !git::clean(wt)?
    {
        bail!("worktree changed or is protected");
    }
    Ok(clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::PullRequest;
    use serde_json::json;
    use std::process::Command;

    struct Fixture {
        dir: tempfile::TempDir,
        clone: PathBuf,
        config: Config,
        head: String,
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let clone = dir.path().join("clone");
            std::fs::create_dir(&clone).unwrap();
            git(&clone, &["init", "-q", "-b", "main"]);
            git(&clone, &["config", "user.name", "test"]);
            git(&clone, &["config", "user.email", "test@example.test"]);
            std::fs::write(clone.join("tracked"), "original").unwrap();
            std::fs::write(clone.join(".gitignore"), "ignored\n").unwrap();
            git(&clone, &["add", "."]);
            git(&clone, &["commit", "-qm", "initial"]);
            let remote = dir.path().join("remote.git");
            git(
                dir.path(),
                &["init", "--bare", "-q", remote.to_str().unwrap()],
            );
            git(
                &clone,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            git(&clone, &["push", "-q", "origin", "main"]);
            let config = Config::parse(&format!(
                "[projects.demo]\nrepo='o/r'\npath='{}'",
                clone.display()
            ))
            .unwrap();
            let head = git(&clone, &["rev-parse", "HEAD"]);
            Self {
                dir,
                clone,
                config,
                head,
            }
        }

        fn worktree(&self, branch: &str) -> PathBuf {
            // Spaces, quotes and newlines exercise porcelain -z parsing/removal.
            let path = self.dir.path().join(format!("worktree '{branch}\n"));
            git(
                &self.clone,
                &["worktree", "add", "-qb", branch, path.to_str().unwrap()],
            );
            git(&path, &["push", "-q", "origin", branch]);
            path.canonicalize().unwrap()
        }

        fn pr(&self, state: PrState) -> Fake {
            Fake {
                prs: vec![
                    serde_json::from_value(json!({
                        "number":6,"state":state,"headRefOid":self.head,"isCrossRepository":false,
                        "url":"https://github.com/o/r/pull/6"
                    }))
                    .unwrap(),
                ],
                offline: false,
            }
        }

        fn scan(&self, github: &Fake, sessions: &[Session]) -> Inventory {
            discover(&self.config, sessions, &[], github)
        }
    }

    struct Fake {
        prs: Vec<PullRequest>,
        offline: bool,
    }
    impl PullRequests for Fake {
        fn pull_requests(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
            if self.offline {
                bail!("offline");
            }
            Ok(self.prs.clone())
        }
    }

    fn session(worktree: &Path, state: SessionState) -> Session {
        serde_json::from_value(json!({"harness":"codex","id":"s","project":"demo",
            "worktree":worktree,"branch":"done","state":state,"last_activity":0}))
        .unwrap()
    }

    #[test]
    fn merged_and_closed_worktrees_are_discovered_and_removed_after_remote_branch_deletion() {
        for state in [PrState::Merged, PrState::Closed] {
            let f = Fixture::new();
            let wt = f.worktree("done");
            git(&f.clone, &["push", "-q", "origin", "--delete", "done"]);
            let found = f.scan(&f.pr(state), &[]);
            assert!(
                found.cleanup_warnings.is_empty(),
                "{:?}",
                found.cleanup_warnings
            );
            assert_eq!(found.finished_worktrees.len(), 1);
            let approved = &found.finished_worktrees[0];
            assert_eq!(approved.worktree, wt);
            remove(&f.config, &[], &[], approved, &f.clone).unwrap();
            assert!(!wt.exists());
            assert!(f.clone.exists());
            assert_eq!(git(&f.clone, &["branch", "--list", "done"]), "done");
        }
    }

    #[test]
    fn local_changes_unpushed_commits_and_live_sessions_are_never_listed() {
        let f = Fixture::new();
        let wt = f.worktree("done");
        let gh = f.pr(PrState::Merged);
        for name in ["tracked", "untracked", "ignored"] {
            std::fs::write(wt.join(name), "local changes").unwrap();
            assert!(f.scan(&gh, &[]).finished_worktrees.is_empty(), "{name}");
            if name == "tracked" {
                git(&wt, &["add", "tracked"]);
                assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
                git(&wt, &["reset", "--hard", "-q"]);
            } else {
                std::fs::remove_file(wt.join(name)).unwrap();
            }
        }
        for state in [
            SessionState::Working,
            SessionState::Waiting,
            SessionState::Idle,
        ] {
            assert!(
                f.scan(&gh, &[session(&wt, state)])
                    .finished_worktrees
                    .is_empty()
            );
        }
        assert_eq!(
            f.scan(&gh, &[session(&wt, SessionState::Ended)])
                .finished_worktrees
                .len(),
            1
        );
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            git(&wt, &["update-index", flag, "tracked"]);
            std::fs::write(wt.join("tracked"), "hidden edits").unwrap();
            assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
            git(
                &wt,
                &[
                    "update-index",
                    "--no-assume-unchanged",
                    "--no-skip-worktree",
                    "tracked",
                ],
            );
            git(&wt, &["reset", "--hard", "-q"]);
        }
        git(&wt, &["commit", "--allow-empty", "-qm", "unpushed"]);
        assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
    }

    #[test]
    fn protects_locked_detached_open_reused_fork_and_unknown_worktrees() {
        let f = Fixture::new();
        let wt = f.worktree("done");
        let mut gh = f.pr(PrState::Merged);
        git(&f.clone, &["worktree", "lock", wt.to_str().unwrap()]);
        assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
        git(&f.clone, &["worktree", "unlock", wt.to_str().unwrap()]);
        git(&wt, &["checkout", "-q", "--detach"]);
        assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
        git(&wt, &["checkout", "-q", "done"]);
        gh.prs.push(gh.prs[0].clone());
        gh.prs[1].state = PrState::Open;
        gh.prs[1].number = 7;
        assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
        gh.prs.pop();
        gh.prs[0].is_cross_repository = true;
        assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
        gh.prs.clear();
        assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
        gh.offline = true;
        let found = f.scan(&gh, &[]);
        assert!(found.finished_worktrees.is_empty());
        assert_eq!(found.cleanup_warnings.len(), 1);
        assert!(wt.exists());
    }

    /// A repository the Session wrote inside its worktree, with a clean filter
    /// that leaves `marker` behind if git ever runs it.
    fn filtered_repo(dir: &Path, marker: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.name", "test"]);
        git(dir, &["config", "user.email", "test@example.test"]);
        let command = format!("touch '{}'; cat", marker.display());
        git(dir, &["config", "filter.pwn.clean", &command]);
        std::fs::write(dir.join(".gitattributes"), "* filter=pwn\n").unwrap();
        std::fs::write(dir.join("tracked"), "original").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", "filtered"]);
        // Setting up ran the filter; a stale stat makes `status` run it again.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.join("tracked"), "original").unwrap();
    }

    #[test]
    fn scanning_never_runs_commands_from_config_a_session_wrote() {
        let f = Fixture::new();
        let marker = f.dir.path().join("filter-ran");
        let gh = f.pr(PrState::Merged);

        // An embedded repository committed as a gitlink.
        let wt = f.worktree("done");
        filtered_repo(&wt.join("sub"), &marker);
        git(&wt, &["add", "sub"]);
        git(&wt, &["commit", "-qm", "gitlink"]);
        std::fs::write(wt.join("sub/tracked"), "original").unwrap();
        let _ = std::fs::remove_file(&marker);
        assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
        assert!(!marker.exists(), "status entered a Session's submodule");

        // The worktree's `.git` file pointed at a repository with a filter.
        let wt = f.worktree("redirected");
        let evil = f.dir.path().join("evil");
        filtered_repo(&evil, &marker);
        std::fs::write(wt.join("tracked"), "original").unwrap();
        std::fs::write(wt.join(".gitattributes"), "* filter=pwn\n").unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", evil.join(".git").display()),
        )
        .unwrap();
        let mut gh = f.pr(PrState::Merged);
        gh.prs[0].head_ref_oid = git(&evil, &["rev-parse", "HEAD"]);
        let _ = std::fs::remove_file(&marker);
        assert!(f.scan(&gh, &[]).finished_worktrees.is_empty());
        assert!(!marker.exists(), "git followed a Session's `.git` file");
    }

    #[test]
    fn final_check_keeps_worktrees_that_change_after_confirmation() {
        let f = Fixture::new();
        let wt = f.worktree("done");
        let approved = f
            .scan(&f.pr(PrState::Merged), &[])
            .finished_worktrees
            .remove(0);
        assert!(remove(&f.config, &[], &[], &approved, &wt).is_err());
        assert!(
            remove(
                &f.config,
                &[session(&wt, SessionState::Idle)],
                &[],
                &approved,
                &f.clone
            )
            .is_err()
        );
        std::fs::write(wt.join("tracked"), "new work").unwrap();
        assert!(remove(&f.config, &[], &[], &approved, &f.clone).is_err());
        git(&wt, &["reset", "--hard", "-q"]);
        git(&wt, &["commit", "--allow-empty", "-qm", "new work"]);
        assert!(remove(&f.config, &[], &[], &approved, &f.clone).is_err());
        assert!(wt.exists());
    }
}
