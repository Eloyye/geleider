//! Claims: a Session's exclusive hold on one issue, shown on GitHub as the issue
//! assigned to the user plus a comment naming the Session. A Claim ends when the
//! issue closes or it is released, never by timing out.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::github::GitHub;
use crate::report;
use crate::session::{Session, SessionKey, SessionState};

/// How long a holder may be quiet before `geleider status` flags its Claim.
pub const QUIET_AFTER: Duration = Duration::from_secs(24 * 3600);
/// The largest Claims snapshot a start hook reads.
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// GitHub repository as `owner/name`.
    pub repo: String,
    pub issue: u64,
    pub project: String,
    /// The Session holding the Claim.
    pub session: SessionKey,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    /// The repository's shared `.git` directory, so a start hook can match its
    /// own worktree to a Claim without running git.
    pub git_dir: PathBuf,
    /// The GitHub login the issue was assigned to.
    pub assignee: String,
    /// Unix time in milliseconds.
    pub claimed_at: i64,
}

impl Claim {
    /// e.g. `Eloyye/geleider#5`.
    pub fn name(&self) -> String {
        format!("{}#{}", self.repo, self.issue)
    }

    /// e.g. `Codex session 019a… on `codex/issue-5-x``.
    pub fn holder(&self) -> String {
        format!(
            "{} session {} on {}",
            self.session.harness.name(),
            self.session.id,
            branch(self.branch.as_deref())
        )
    }
}

fn branch(branch: Option<&str>) -> String {
    branch.map_or("a detached HEAD".into(), |b| format!("`{b}`"))
}

/// A Claim as `geleider status` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimStatus {
    #[serde(flatten)]
    pub claim: Claim,
    /// The holder's state, if geleider still knows the Session.
    pub session_state: Option<SessionState>,
    /// When the holder last did anything, or when it claimed if later.
    pub last_activity: i64,
    /// The holder has been quiet for more than a day. The Claim is kept anyway.
    pub quiet: bool,
}

impl ClaimStatus {
    pub fn new(claim: Claim, holder: Option<&Session>, quiet_after: Duration, now: i64) -> Self {
        let last_activity =
            holder.map_or(claim.claimed_at, |s| s.last_activity.max(claim.claimed_at));
        Self {
            session_state: holder.map(|s| s.state),
            quiet: now - last_activity > quiet_after.as_millis() as i64,
            last_activity,
            claim,
        }
    }
}

/// An issue as `claim` and `release` name it: `5`, `#5` or `owner/name#5`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRef {
    /// Absent means the Project of the caller's worktree.
    pub repo: Option<String>,
    pub number: u64,
}

impl IssueRef {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (repo, number) = match text.rsplit_once('#') {
            Some((repo, number)) if !repo.is_empty() => (Some(repo), number),
            Some((_, number)) => (None, number),
            None => (None, text),
        };
        let number = match number.parse::<u64>() {
            Ok(n) if n > 0 => n,
            _ => bail!("`{text}` is not an issue: use a number such as 5, #5 or owner/name#5"),
        };
        if let Some(repo) = repo {
            check_repo(repo)?;
        }
        Ok(Self {
            repo: repo.map(str::to_string),
            number,
        })
    }
}

/// A repository name is put into GitHub API paths, so it must be exactly
/// `owner/name`, with nothing that could change the path.
pub fn check_repo(repo: &str) -> Result<()> {
    let part = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if part(owner) && part(name) => Ok(()),
        _ => bail!("`{repo}` is not a GitHub repository (expected owner/name)"),
    }
}

/// Why another Session cannot claim the issue `claim` holds.
pub fn held_by(claim: &Claim) -> String {
    format!(
        "{} is already claimed by {} (since {}). Only one Session works on an issue at a time; \
         ask the user before working on it.",
        claim.name(),
        claim.holder(),
        report::clock(claim.claimed_at, crate::store::now_ms())
    )
}

/// Assigns the issue to the user and comments naming the Session. Fails, naming
/// the assignees, if the issue is closed, a pull request, or assigned already.
pub fn claim_on_github(github: &dyn GitHub, mut claim: Claim) -> Result<Claim> {
    let (repo, number) = (claim.repo.as_str(), claim.issue);
    let issue = github.issue(repo, number)?;
    if issue.pull_request {
        bail!("{repo}#{number} is a pull request, not an issue");
    }
    if !issue.open {
        bail!("{repo}#{number} is closed");
    }
    if !issue.assignees.is_empty() {
        bail!(
            "{repo}#{number} is already assigned to {} on GitHub, so someone is on it. \
             Ask the user before working on it.",
            logins(&issue.assignees)
        );
    }
    let login = github.viewer()?;
    let assigned = github.set_assignee(repo, number, &login, true)?;
    if !assigned.assignees.contains(&login) {
        bail!("GitHub did not assign {repo}#{number} to @{login}; does @{login} have push access?");
    }
    let body = format!("Claimed by {}.", claim.holder());
    if let Err(err) = github.create_comment(repo, number, &body) {
        // Leave the issue as it was, so nothing shows a Claim geleider doesn't have.
        let _ = github.set_assignee(repo, number, &login, false);
        return Err(err.context(format!("commenting on {repo}#{number}")));
    }
    claim.assignee = login;
    Ok(claim)
}

/// Unassigns the user and comments that the Claim was released.
pub fn release_on_github(github: &dyn GitHub, claim: &Claim) -> Result<()> {
    github.set_assignee(&claim.repo, claim.issue, &claim.assignee, false)?;
    let body = format!("Claim released (was held by {}).", claim.holder());
    github.create_comment(&claim.repo, claim.issue, &body)
}

fn logins(logins: &[String]) -> String {
    logins
        .iter()
        .map(|l| format!("@{l}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Writes every Claim where start hooks can read it without asking the daemon.
pub fn write_snapshot(path: &Path, claims: &[Claim]) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(&serde_json::to_vec(claims)?)?;
    drop(file);
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))
}

fn read_snapshot(path: &Path) -> Option<Vec<Claim>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_SNAPSHOT_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// What a Session starting in `cwd` should hear: Claims other Sessions hold on
/// the issues its branch names, such as `codex/issue-5-x`. It only warns; it
/// never claims. Reads files only, so it is quick enough for a start hook.
pub fn start_warning(snapshot: &Path, session: &SessionKey, cwd: &Path) -> Option<String> {
    let (git_dir, branch) = repository(cwd)?;
    let issues = branch_issues(&branch);
    if issues.is_empty() {
        return None;
    }
    let held: Vec<String> = read_snapshot(snapshot)?
        .into_iter()
        .filter(|c| c.git_dir == git_dir && issues.contains(&c.issue) && c.session != *session)
        .map(|c| format!("- {} is claimed by {}.", c.name(), c.holder()))
        .collect();
    if held.is_empty() {
        return None;
    }
    Some(format!(
        "geleider: this Session's branch `{branch}` names an issue another Session holds:\n{}\n\
         Only one Session works on an issue at a time. Don't work on it here; tell the user and \
         ask what to do. (This is a warning from geleider's start hook; nothing was claimed.)",
        held.join("\n")
    ))
}

/// The issue numbers a branch names as `issue-<n>`.
pub fn branch_issues(branch: &str) -> Vec<u64> {
    let mut issues = Vec::new();
    let mut rest = branch;
    while let Some(at) = rest.find("issue-") {
        rest = &rest[at + "issue-".len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if let Ok(n) = rest[..digits].parse::<u64>()
            && n > 0
            && !issues.contains(&n)
        {
            issues.push(n);
        }
    }
    issues
}

/// The shared `.git` directory and checked-out branch of the repository at
/// `cwd`, read from the files git keeps rather than by running it.
fn repository(cwd: &Path) -> Option<(PathBuf, String)> {
    let (top, dot_git) = cwd
        .ancestors()
        .map(|dir| (dir, dir.join(".git")))
        .find(|(_, git)| git.exists())?;
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        top.join(text.trim().strip_prefix("gitdir:")?.trim())
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?.to_string();
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => git_dir.join(text.trim()),
        Err(_) => git_dir,
    };
    Some((std::fs::canonicalize(common).ok()?, branch))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::process::Command;

    use super::*;
    use crate::github::Issue;
    use crate::session::Harness;

    fn key(harness: Harness, id: &str) -> SessionKey {
        SessionKey::new(harness, id).unwrap()
    }

    fn claim(git_dir: &Path, issue: u64, holder: SessionKey) -> Claim {
        Claim {
            repo: "o/r".into(),
            issue,
            project: "demo".into(),
            session: holder,
            worktree: "/wt".into(),
            branch: Some("feat/x".into()),
            git_dir: git_dir.to_path_buf(),
            assignee: String::new(),
            claimed_at: 1_000,
        }
    }

    #[test]
    fn issues_are_numbers_with_an_optional_repository() {
        let parse = |t| IssueRef::parse(t).unwrap();
        assert_eq!(
            parse("5"),
            IssueRef {
                repo: None,
                number: 5
            }
        );
        assert_eq!(
            parse("#12"),
            IssueRef {
                repo: None,
                number: 12
            }
        );
        assert_eq!(
            parse("Eloyye/geleider#5").repo.as_deref(),
            Some("Eloyye/geleider")
        );
        for bad in [
            "", "0", "#", "five", "-1", "a/../b#1", "o/r/x#1", "../r#1", "o#1",
        ] {
            assert!(IssueRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn branches_name_issues_as_issue_dash_number() {
        assert_eq!(branch_issues("codex/issue-12-fix"), [12]);
        assert_eq!(branch_issues("claude/issue-5-d4ff9e"), [5]);
        assert_eq!(branch_issues("issue-3-and-issue-40"), [3, 40]);
        assert!(branch_issues("feat/issues-5").is_empty());
        assert!(branch_issues("issue-").is_empty());
        assert!(branch_issues("main").is_empty());
    }

    #[test]
    fn a_holder_quiet_for_a_day_is_flagged() {
        let day = QUIET_AFTER.as_millis() as i64;
        let c = claim(Path::new("/g"), 1, key(Harness::Codex, "s"));
        let session = |last_activity| Session {
            key: c.session.clone(),
            project: None,
            worktree: "/wt".into(),
            branch: None,
            state: SessionState::Idle,
            last_activity,
            pending_prompts: Vec::new(),
            owner_pid: None,
            owner_started: None,
        };
        let at = |now, s: Option<&Session>| ClaimStatus::new(c.clone(), s, QUIET_AFTER, now);
        assert!(!at(c.claimed_at + day, None).quiet);
        assert!(at(c.claimed_at + day + 1, None).quiet);
        let active = session(c.claimed_at + day);
        assert!(!at(c.claimed_at + day + 1, Some(&active)).quiet);
        assert_eq!(at(0, Some(&active)).session_state, Some(SessionState::Idle));
        // Activity from before the Claim does not make it look older.
        let earlier = session(0);
        assert_eq!(at(0, Some(&earlier)).last_activity, c.claimed_at);
    }

    /// One repo's issues, in memory.
    #[derive(Default)]
    struct FakeGitHub {
        issue: RefCell<Issue>,
        comments: RefCell<Vec<String>>,
        fail_comments: bool,
    }

    impl GitHub for FakeGitHub {
        fn open_pr(&self, _: &str, _: &str) -> Result<Option<u64>> {
            Ok(None)
        }
        fn find_comment(&self, _: &str, _: u64, _: &str) -> Result<Option<u64>> {
            Ok(None)
        }
        fn create_comment(&self, _: &str, _: u64, body: &str) -> Result<()> {
            if self.fail_comments {
                bail!("comments are down");
            }
            self.comments.borrow_mut().push(body.into());
            Ok(())
        }
        fn edit_comment(&self, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn issue(&self, _: &str, _: u64) -> Result<Issue> {
            Ok(self.issue.borrow().clone())
        }
        fn viewer(&self) -> Result<String> {
            Ok("me".into())
        }
        fn set_assignee(&self, _: &str, _: u64, login: &str, assigned: bool) -> Result<Issue> {
            let mut issue = self.issue.borrow_mut();
            issue.assignees.retain(|l| l != login);
            if assigned {
                issue.assignees.push(login.into());
            }
            Ok(issue.clone())
        }
    }

    fn open_issue() -> Issue {
        Issue {
            open: true,
            assignees: Vec::new(),
            pull_request: false,
        }
    }

    #[test]
    fn claiming_assigns_the_user_and_names_the_session_once() {
        let gh = FakeGitHub {
            issue: RefCell::new(open_issue()),
            ..Default::default()
        };
        let new = claim(Path::new("/g"), 5, key(Harness::Claude, "abc"));
        let claimed = claim_on_github(&gh, new.clone()).unwrap();
        assert_eq!(claimed.assignee, "me");
        assert_eq!(gh.issue.borrow().assignees, ["me"]);
        assert_eq!(
            *gh.comments.borrow(),
            ["Claimed by Claude Code session abc on `feat/x`."]
        );
        // Now assigned, so a second claim fails without commenting.
        let err = claim_on_github(&gh, new).unwrap_err();
        assert!(
            format!("{err:#}").contains("already assigned to @me"),
            "{err:#}"
        );
        assert_eq!(gh.comments.borrow().len(), 1);

        release_on_github(&gh, &claimed).unwrap();
        assert!(gh.issue.borrow().assignees.is_empty());
        assert_eq!(
            gh.comments.borrow()[1],
            "Claim released (was held by Claude Code session abc on `feat/x`)."
        );
    }

    #[test]
    fn closed_issues_pull_requests_and_failed_comments_are_not_claimed() {
        let new = claim(Path::new("/g"), 5, key(Harness::Codex, "abc"));
        let gh = |issue| FakeGitHub {
            issue: RefCell::new(issue),
            ..Default::default()
        };
        let closed = gh(Issue {
            open: false,
            ..open_issue()
        });
        assert!(
            format!("{:#}", claim_on_github(&closed, new.clone()).unwrap_err()).contains("closed")
        );
        let pr = gh(Issue {
            pull_request: true,
            ..open_issue()
        });
        assert!(claim_on_github(&pr, new.clone()).is_err());
        let failing = FakeGitHub {
            fail_comments: true,
            ..gh(open_issue())
        };
        assert!(claim_on_github(&failing, new).is_err());
        assert!(failing.issue.borrow().assignees.is_empty());
    }

    fn run_git(dir: &Path, args: &[&str]) {
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
    }

    #[test]
    fn a_session_starting_on_a_claimed_issues_branch_is_warned() {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("clone");
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
                "i",
            ],
        );
        let wt = dir.path().join("wt");
        run_git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "codex/issue-7-x",
                wt.to_str().unwrap(),
            ],
        );
        std::fs::create_dir(wt.join("sub")).unwrap();

        let git_dir = std::fs::canonicalize(clone.join(".git")).unwrap();
        assert_eq!(
            repository(&wt.join("sub")),
            Some((git_dir.clone(), "codex/issue-7-x".into()))
        );
        assert_eq!(repository(&clone), Some((git_dir.clone(), "main".into())));

        let snapshot = dir.path().join("claims.json");
        let holder = key(Harness::Claude, "holder");
        let me = key(Harness::Codex, "newcomer");
        // No snapshot yet, or no Claim on this issue: nothing to say.
        assert_eq!(start_warning(&snapshot, &me, &wt), None);
        let elsewhere = dir.path().join("elsewhere");
        write_snapshot(
            &snapshot,
            &[
                claim(&git_dir, 8, holder.clone()),
                claim(&elsewhere, 7, holder.clone()),
            ],
        )
        .unwrap();
        assert_eq!(start_warning(&snapshot, &me, &wt), None);

        write_snapshot(&snapshot, &[claim(&git_dir, 7, holder.clone())]).unwrap();
        let warning = start_warning(&snapshot, &me, &wt.join("sub")).unwrap();
        assert!(
            warning.contains("o/r#7 is claimed by Claude Code session holder"),
            "{warning}"
        );
        // The holder itself, resuming, and a branch naming no issue, are not warned.
        assert_eq!(start_warning(&snapshot, &holder, &wt), None);
        assert_eq!(start_warning(&snapshot, &me, &clone), None);
    }
}
