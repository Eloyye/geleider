//! GitHub through `gh`: branch PR history and CI checks, posting a Run's result
//! on its branch's open PR (one comment per Job per PR, edited in place on later
//! Runs), and the issue assignments and comments that show Claims.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command;

/// Branch PR history, shared by CI watching and Finished-worktree discovery.
pub trait PullRequests {
    fn pull_requests(&self, repo: &str, branch: &str) -> Result<Vec<PullRequest>>;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub state: PrState,
    pub head_ref_oid: String,
    pub is_cross_repository: bool,
    pub url: String,
    #[serde(default)]
    pub status_check_rollup: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

impl std::fmt::Display for PrState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PrState::Open => "open",
            PrState::Closed => "closed",
            PrState::Merged => "merged",
        })
    }
}

type CachedPrs = (Instant, std::result::Result<Vec<PullRequest>, String>);

/// Status polling shares this cache with CI watching, including failed queries,
/// so frequent `status` calls do not repeatedly query GitHub while it is offline.
#[derive(Default)]
pub struct CachedPullRequests {
    entries: Mutex<BTreeMap<(String, String), CachedPrs>>,
}

impl CachedPullRequests {
    fn entries(&self) -> MutexGuard<'_, BTreeMap<(String, String), CachedPrs>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PullRequests for CachedPullRequests {
    fn pull_requests(&self, repo: &str, branch: &str) -> Result<Vec<PullRequest>> {
        let key = (repo.to_string(), branch.to_string());
        let cached = {
            let mut entries = self.entries();
            entries.retain(|_, (at, _)| at.elapsed() < crate::ci::INTERVAL);
            entries.get(&key).map(|(_, result)| result.clone())
        };
        // `gh` runs without the lock, so one slow query holds up no other branch.
        let result = cached.unwrap_or_else(|| {
            let fetched = Gh
                .pull_requests(repo, branch)
                .map_err(|err| format!("{err:#}"));
            self.entries()
                .insert(key, (Instant::now(), fetched.clone()));
            fetched
        });
        result.map_err(anyhow::Error::msg)
    }
}

/// How long one `gh` call may take before it is killed.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// What geleider needs from GitHub, so tests can stand in for `gh`.
pub trait GitHub {
    fn open_pr(&self, repo: &str, branch: &str) -> Result<Option<u64>>;
    /// The id of the PR's comment that starts with `marker`, if there is one.
    fn find_comment(&self, repo: &str, pr: u64, marker: &str) -> Result<Option<u64>>;
    /// Comments on an issue or a PR, which share one number space.
    fn create_comment(&self, repo: &str, number: u64, body: &str) -> Result<()>;
    fn edit_comment(&self, repo: &str, comment: u64, body: &str) -> Result<()>;
    fn issue(&self, repo: &str, number: u64) -> Result<Issue>;
    /// The login `gh` is signed in as.
    fn viewer(&self) -> Result<String>;
    /// Adds or removes `login` as an assignee, returning the issue afterwards.
    fn set_assignee(&self, repo: &str, number: u64, login: &str, assigned: bool) -> Result<Issue>;
}

/// What a Claim needs to know about an issue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Issue {
    pub open: bool,
    pub assignees: Vec<String>,
    /// GitHub serves PRs as issues too.
    pub pull_request: bool,
}

impl Issue {
    fn from_json(text: &str) -> Result<Self> {
        #[derive(serde::Deserialize)]
        struct Login {
            login: String,
        }
        #[derive(serde::Deserialize)]
        struct Raw {
            state: String,
            #[serde(default)]
            assignees: Vec<Login>,
            #[serde(default)]
            pull_request: Option<serde_json::Value>,
        }
        let raw: Raw = serde_json::from_str(text).context("decoding the issue from gh")?;
        Ok(Self {
            open: raw.state == "open",
            assignees: raw.assignees.into_iter().map(|l| l.login).collect(),
            pull_request: raw.pull_request.is_some_and(|p| !p.is_null()),
        })
    }
}

/// A repository name is put into GitHub API paths, so it must be exactly
/// `owner/name`, with nothing such as `..` that could change the path.
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

/// The hidden first line that marks a Job's result comment.
pub fn marker(job: &str) -> String {
    format!("<!-- geleider:job={job} -->")
}

#[derive(Debug, PartialEq, Eq)]
pub enum Posted {
    NoBranch,
    NoPr,
    Created(u64),
    Edited(u64),
}

/// Creates or edits the result comment for `job` on the open PR for `branch`.
/// `body` must start with [`marker`].
pub fn post_result(
    github: &dyn GitHub,
    repo: &str,
    branch: Option<&str>,
    job: &str,
    body: &str,
) -> Result<Posted> {
    let Some(branch) = branch else {
        return Ok(Posted::NoBranch);
    };
    let Some(pr) = github.open_pr(repo, branch)? else {
        return Ok(Posted::NoPr);
    };
    match github.find_comment(repo, pr, &marker(job))? {
        Some(comment) => {
            github.edit_comment(repo, comment, body)?;
            Ok(Posted::Edited(pr))
        }
        None => {
            github.create_comment(repo, pr, body)?;
            Ok(Posted::Created(pr))
        }
    }
}

/// [`GitHub`] through the `gh` command, signed in as the user.
pub struct Gh;

impl PullRequests for Gh {
    fn pull_requests(&self, repo: &str, branch: &str) -> Result<Vec<PullRequest>> {
        let out = self.run(
            &[
                "pr",
                "list",
                "--repo",
                repo,
                "--head",
                branch,
                "--state",
                "all",
                "--limit",
                "100",
                "--json",
                "number,state,headRefOid,isCrossRepository,url,statusCheckRollup",
            ],
            None,
        )?;
        let prs: Vec<PullRequest> = serde_json::from_str(&out).context("decoding branch PRs")?;
        // A truncated history cannot prove that there is no open PR for cleanup.
        if prs.len() >= 100 {
            bail!("too many PRs for branch `{branch}` to inspect safely");
        }
        Ok(prs
            .into_iter()
            .filter(|pr| !pr.is_cross_repository)
            .collect())
    }
}

impl Gh {
    fn run(&self, args: &[&str], input: Option<&str>) -> Result<String> {
        let output = command::output_within(
            Command::new("gh").args(args),
            input.map(str::as_bytes),
            GH_TIMEOUT,
        )
        .context("is the GitHub CLI installed and on PATH?")?;
        if !output.status.success() {
            bail!(
                "gh {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

impl GitHub for Gh {
    fn open_pr(&self, repo: &str, branch: &str) -> Result<Option<u64>> {
        let out = self.run(
            &[
                "pr",
                "list",
                "--repo",
                repo,
                "--head",
                branch,
                "--state",
                "open",
                "--json",
                "number",
                "--jq",
                ".[0].number",
            ],
            None,
        )?;
        Ok(out.parse().ok())
    }

    fn find_comment(&self, repo: &str, pr: u64, marker: &str) -> Result<Option<u64>> {
        let jq = format!(
            ".[] | select(.body | startswith({})) | .id",
            serde_json::to_string(marker)?
        );
        let out = self.run(
            &[
                "api",
                "--paginate",
                &format!("repos/{repo}/issues/{pr}/comments"),
                "--jq",
                &jq,
            ],
            None,
        )?;
        Ok(out.lines().next().and_then(|id| id.trim().parse().ok()))
    }

    fn create_comment(&self, repo: &str, number: u64, body: &str) -> Result<()> {
        let input = serde_json::json!({ "body": body }).to_string();
        self.run(
            &[
                "api",
                "-X",
                "POST",
                &format!("repos/{repo}/issues/{number}/comments"),
                "--input",
                "-",
            ],
            Some(&input),
        )?;
        Ok(())
    }

    fn edit_comment(&self, repo: &str, comment: u64, body: &str) -> Result<()> {
        let input = serde_json::json!({ "body": body }).to_string();
        self.run(
            &[
                "api",
                "-X",
                "PATCH",
                &format!("repos/{repo}/issues/comments/{comment}"),
                "--input",
                "-",
            ],
            Some(&input),
        )?;
        Ok(())
    }

    fn issue(&self, repo: &str, number: u64) -> Result<Issue> {
        Issue::from_json(&self.run(&["api", &format!("repos/{repo}/issues/{number}")], None)?)
    }

    fn viewer(&self) -> Result<String> {
        let out = self.run(&["api", "user"], None)?;
        let user: serde_json::Value = serde_json::from_str(&out).context("decoding gh api user")?;
        user["login"]
            .as_str()
            .map(str::to_string)
            .context("gh api user returned no login")
    }

    fn set_assignee(&self, repo: &str, number: u64, login: &str, assigned: bool) -> Result<Issue> {
        let input = serde_json::json!({ "assignees": [login] }).to_string();
        let method = if assigned { "POST" } else { "DELETE" };
        let out = self.run(
            &[
                "api",
                "-X",
                method,
                &format!("repos/{repo}/issues/{number}/assignees"),
                "--input",
                "-",
            ],
            Some(&input),
        )?;
        Issue::from_json(&out)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// One repo's PRs and comments, in memory.
    #[derive(Default)]
    struct FakeGitHub {
        prs: Vec<(String, u64)>,
        comments: RefCell<Vec<(u64, u64, String)>>,
    }

    impl GitHub for FakeGitHub {
        fn open_pr(&self, _repo: &str, branch: &str) -> Result<Option<u64>> {
            Ok(self.prs.iter().find(|(b, _)| b == branch).map(|(_, n)| *n))
        }
        fn find_comment(&self, _repo: &str, pr: u64, marker: &str) -> Result<Option<u64>> {
            Ok(self
                .comments
                .borrow()
                .iter()
                .find(|(_, p, body)| *p == pr && body.starts_with(marker))
                .map(|(id, _, _)| *id))
        }
        fn create_comment(&self, _repo: &str, pr: u64, body: &str) -> Result<()> {
            let mut comments = self.comments.borrow_mut();
            let id = comments.len() as u64 + 100;
            comments.push((id, pr, body.to_string()));
            Ok(())
        }
        fn edit_comment(&self, _repo: &str, comment: u64, body: &str) -> Result<()> {
            let mut comments = self.comments.borrow_mut();
            let entry = comments
                .iter_mut()
                .find(|(id, _, _)| *id == comment)
                .unwrap();
            entry.2 = body.to_string();
            Ok(())
        }
        fn issue(&self, _repo: &str, _number: u64) -> Result<Issue> {
            bail!("no issues here")
        }
        fn viewer(&self) -> Result<String> {
            bail!("no issues here")
        }
        fn set_assignee(&self, _: &str, _: u64, _: &str, _: bool) -> Result<Issue> {
            bail!("no issues here")
        }
    }

    #[test]
    fn issues_are_read_from_the_rest_api() {
        let issue = Issue::from_json(
            r#"{"state":"open","assignees":[{"login":"me"}],"pull_request":null,"title":"x"}"#,
        )
        .unwrap();
        assert_eq!(
            issue,
            Issue {
                open: true,
                assignees: vec!["me".into()],
                pull_request: false
            }
        );
        let pr = Issue::from_json(r#"{"state":"closed","pull_request":{"url":"u"}}"#).unwrap();
        assert!(!pr.open && pr.pull_request && pr.assignees.is_empty());
    }

    #[test]
    fn second_run_edits_the_first_comment_and_jobs_stay_separate() {
        let gh = FakeGitHub {
            prs: vec![("feat/x".into(), 7)],
            ..Default::default()
        };
        let body = |job: &str, text: &str| format!("{}\n{text}", marker(job));
        let post = |job: &str, text: &str| {
            post_result(&gh, "o/r", Some("feat/x"), job, &body(job, text)).unwrap()
        };

        assert_eq!(post("ck3-test", "failed"), Posted::Created(7));
        assert_eq!(post("ck3-test", "passed"), Posted::Edited(7));
        assert_eq!(post("lint", "passed"), Posted::Created(7));

        let comments = gh.comments.borrow();
        assert_eq!(comments.len(), 2);
        assert!(comments[0].2.ends_with("passed"));
    }

    #[test]
    fn no_branch_or_no_pr_means_no_comment() {
        let gh = FakeGitHub::default();
        assert_eq!(
            post_result(&gh, "o/r", None, "j", "b").unwrap(),
            Posted::NoBranch
        );
        assert_eq!(
            post_result(&gh, "o/r", Some("main"), "j", "b").unwrap(),
            Posted::NoPr
        );
        assert!(gh.comments.borrow().is_empty());
    }
}
