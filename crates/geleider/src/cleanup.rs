//! Discover Finished worktrees conservatively; removal is an explicit CLI action.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::git;
use crate::github::{PrState, PullRequests};
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

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Inventory {
    pub finished_worktrees: Vec<FinishedWorktree>,
    pub cleanup_warnings: Vec<String>,
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
        let mut scan = || -> Result<Vec<FinishedWorktree>> {
            let clone = project.clone_path()?;
            let mut finished = Vec::new();
            for wt in git::worktrees(&clone)? {
                if wt.protected || busy(&wt.path, sessions, runs) || !git::clean(&wt.path)? {
                    continue;
                }
                let Some(branch) = wt.branch else { continue };
                let prs = match github.pull_requests(&project.repo, &branch) {
                    Ok(prs) => prs,
                    Err(err) => {
                        inventory
                            .cleanup_warnings
                            .push(format!("{name} {branch}: {err:#}"));
                        continue;
                    }
                };
                if prs.iter().any(|pr| pr.state == PrState::Open) {
                    continue;
                }
                let Some(pr) = prs
                    .iter()
                    .filter(|pr| !pr.is_cross_repository)
                    .max_by_key(|pr| pr.number)
                else {
                    continue;
                };
                // GitHub retains the PR head after merging/deleting its branch.
                // Exact equality proves all local commits were pushed, including
                // with squash merges, without trusting stale remote-tracking refs.
                if pr.head_ref_oid != wt.head {
                    continue;
                }
                finished.push(FinishedWorktree {
                    project: name.clone(),
                    worktree: wt.path,
                    branch,
                    head: wt.head,
                    pr: pr.number,
                    pr_state: pr.state,
                    pr_url: pr.url.clone(),
                });
            }
            Ok(finished)
        };
        match scan() {
            Ok(finished) => inventory.finished_worktrees.extend(finished),
            Err(err) => inventory.cleanup_warnings.push(format!("{name}: {err:#}")),
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
        || !git::clean(&wt.path)?
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
