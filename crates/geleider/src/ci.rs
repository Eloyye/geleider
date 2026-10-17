//! User-only CI notifications. No Session delivery or automatic fixes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::Config;
use crate::github::{PrState, PullRequest};
use crate::session::Session;

/// At most one branch query every 15 seconds, and each branch at most once per
/// five minutes (240 gh invocations/hour even with many Projects/Sessions).
pub const TICK: Duration = Duration::from_secs(15);
pub const INTERVAL: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct Poller {
    checked: BTreeMap<(String, String), Instant>,
}

impl Poller {
    pub fn next(
        &mut self,
        config: &Config,
        sessions: &[Session],
        now: Instant,
        interval: Duration,
    ) -> Option<(String, String)> {
        let targets: BTreeSet<_> = sessions
            .iter()
            .filter_map(|s| {
                let project = config.projects.get(s.project.as_deref()?)?;
                Some((project.repo.clone(), s.branch.clone()?))
            })
            .collect();
        self.checked.retain(|key, _| targets.contains(key));
        let key = targets
            .into_iter()
            .filter(|key| {
                self.checked
                    .get(key)
                    .is_none_or(|at| now.duration_since(*at) >= interval)
            })
            .min_by_key(|key| self.checked.get(key).copied())?;
        // Failed gh calls are paced too.
        self.checked.insert(key.clone(), now);
        Some(key)
    }
}

pub struct Failure {
    pub id: String,
    pub name: String,
}

/// gh returns CheckRuns and legacy commit StatusContexts in the same rollup.
/// Include the commit and attempt timestamps: repeated polls/restarts stay quiet,
/// while a new failing commit or a failed rerun of the same commit notifies.
pub fn failures(pr: &PullRequest) -> Vec<Failure> {
    if pr.state != PrState::Open || pr.is_cross_repository {
        return Vec::new();
    }
    pr.status_check_rollup
        .iter()
        .filter_map(|check| {
            let (name, attempt) = failed_attempt(check)?;
            Some(Failure {
                id: json!([pr.head_ref_oid, check["__typename"], name, attempt]).to_string(),
                name: name.to_string(),
            })
        })
        .collect()
}

/// A failed check's name and what identifies this run of it, or `None` if it
/// has not failed (yet).
fn failed_attempt(check: &Value) -> Option<(&str, Value)> {
    match check["__typename"].as_str()? {
        "CheckRun" => {
            let failed = matches!(
                check["conclusion"].as_str(),
                Some("FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
            );
            if check["status"] != "COMPLETED" || !failed {
                return None;
            }
            let attempt = json!([
                check["detailsUrl"],
                check["startedAt"],
                check["completedAt"]
            ]);
            Some((check["name"].as_str()?, attempt))
        }
        "StatusContext" => {
            if !matches!(check["state"].as_str(), Some("FAILURE" | "ERROR")) {
                return None;
            }
            let attempt = json!([check["targetUrl"], check["createdAt"]]);
            Some((check["context"].as_str()?, attempt))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn failures_notify_once_across_polls_and_restart_but_new_attempts_notify() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let logs = dir.path().join("logs");
        let mut pr: PullRequest = serde_json::from_value(json!({
            "number":6, "state":"OPEN", "headRefOid":"sha1", "isCrossRepository":false,
            "url":"https://github.com/o/r/pull/6", "statusCheckRollup":[
                {"__typename":"CheckRun", "name":"test", "status":"COMPLETED",
                 "conclusion":"FAILURE", "detailsUrl":"job/1", "completedAt":"t1"},
                {"__typename":"StatusContext", "context":"lint", "state":"ERROR",
                 "targetUrl":"lint/1", "createdAt":"t1"},
                {"__typename":"CheckRun", "name":"pending", "status":"IN_PROGRESS", "conclusion":"FAILURE"},
                {"__typename":"CheckRun", "name":"cancelled", "status":"COMPLETED", "conclusion":"CANCELLED"}
            ]
        })).unwrap();
        let claim = |store: &Store, pr: &PullRequest| -> usize {
            failures(pr)
                .iter()
                .filter(|f| store.new_ci_failure("o/r", pr.number, &f.id).unwrap())
                .count()
        };
        let store = Store::open(&db, logs.clone()).unwrap();
        assert_eq!(claim(&store, &pr), 2);
        assert_eq!(claim(&store, &pr), 0);
        drop(store);
        let store = Store::open(&db, logs).unwrap();
        assert_eq!(claim(&store, &pr), 0);
        pr.status_check_rollup[0]["completedAt"] = json!("t2");
        assert_eq!(claim(&store, &pr), 1);
        pr.head_ref_oid = "sha2".into();
        assert_eq!(claim(&store, &pr), 2);
        pr.state = PrState::Merged;
        assert_eq!(claim(&store, &pr), 0);
    }

    #[test]
    fn polls_only_session_branches_fairly_and_paces_retries() {
        let config = Config::parse("[projects.p]\nrepo='o/r'\npath='/tmp/p'").unwrap();
        let session = |id: &str, branch: &str, project: &str| {
            serde_json::from_value(json!({
                "harness":"codex", "id":id, "project":project, "worktree":"/tmp/p",
                "branch":branch, "state":"idle", "last_activity":0
            }))
            .unwrap()
        };
        let sessions = [
            session("s1", "a", "p"),
            session("s2", "a", "p"),
            session("s3", "b", "p"),
            session("s4", "other", "unconfigured"),
        ];
        let mut poller = Poller::default();
        let now = Instant::now();
        assert_eq!(
            poller.next(&config, &sessions, now, INTERVAL),
            Some(("o/r".into(), "a".into()))
        );
        assert_eq!(
            poller.next(&config, &sessions, now + TICK, INTERVAL),
            Some(("o/r".into(), "b".into()))
        );
        assert_eq!(
            poller.next(&config, &sessions, now + 2 * TICK, INTERVAL),
            None
        );
        assert_eq!(
            poller.next(&config, &sessions, now + INTERVAL, INTERVAL),
            Some(("o/r".into(), "a".into()))
        );
    }
}
