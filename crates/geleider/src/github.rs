//! Posting a Run's result on its branch's open PR: one comment per Job per PR,
//! edited in place on later Runs.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// What geleider needs from GitHub, so tests can stand in for `gh`.
pub trait GitHub {
    fn open_pr(&self, repo: &str, branch: &str) -> Result<Option<u64>>;
    /// The id of the PR's comment that starts with `marker`, if there is one.
    fn find_comment(&self, repo: &str, pr: u64, marker: &str) -> Result<Option<u64>>;
    fn create_comment(&self, repo: &str, pr: u64, body: &str) -> Result<()>;
    fn edit_comment(&self, repo: &str, comment: u64, body: &str) -> Result<()>;
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

impl Gh {
    fn run(&self, args: &[&str], input: Option<&str>) -> Result<String> {
        let mut child = Command::new("gh")
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running gh (is the GitHub CLI installed and on PATH?)")?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(input.as_bytes())?;
        }
        let output = child.wait_with_output()?;
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

    fn create_comment(&self, repo: &str, pr: u64, body: &str) -> Result<()> {
        let input = serde_json::json!({ "body": body }).to_string();
        self.run(
            &[
                "api",
                "-X",
                "POST",
                &format!("repos/{repo}/issues/{pr}/comments"),
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
