//! Runs, Sessions, Claims and usage, kept in SQLite so they survive the background
//! process restarting.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Params, Row, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::claim::Claim;
use crate::session::{Harness, Session, SessionKey, SessionState};
use crate::usage::{Subscription, Window};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    /// Exited 0.
    Succeeded,
    /// Exited non-zero, was killed, or could not be started.
    Failed,
    /// The background process restarted while the Run was running, so its exit code
    /// is unknown. Its Lease lasted until its process exited.
    Lost,
}

impl RunState {
    fn as_str(self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::Succeeded => "succeeded",
            RunState::Failed => "failed",
            RunState::Lost => "lost",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "queued" => RunState::Queued,
            "running" => RunState::Running,
            "succeeded" => RunState::Succeeded,
            "failed" => RunState::Failed,
            "lost" => RunState::Lost,
            other => bail!("unknown run state `{other}`"),
        })
    }

    pub fn is_finished(self) -> bool {
        !matches!(self, RunState::Queued | RunState::Running)
    }
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: i64,
    pub project: String,
    pub repo: String,
    pub job: String,
    pub args: Vec<String>,
    pub resource: String,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub argv: Vec<String>,
    pub submitter: String,
    pub session: Option<SessionKey>,
    pub state: RunState,
    /// Unix time in milliseconds.
    pub submitted_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub pid: Option<u32>,
    pub pid_started: Option<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub log_path: PathBuf,
}

impl Run {
    pub fn duration_ms(&self) -> Option<i64> {
        let started = self.started_at?;
        Some(self.ended_at.unwrap_or_else(now_ms) - started)
    }
}

pub struct NewRun {
    pub project: String,
    pub repo: String,
    pub job: String,
    pub args: Vec<String>,
    pub resource: String,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub argv: Vec<String>,
    pub submitter: String,
    pub session: Option<SessionKey>,
}

pub struct Store {
    conn: Connection,
    logs: PathBuf,
}

const COLUMNS: &str = "id, project, repo, job, args, resource, worktree, branch, argv, submitter, \
     state, submitted_at, started_at, ended_at, pid, pid_started, exit_code, error, log_path, session";

impl Store {
    pub fn open(path: &Path, logs: PathBuf) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS runs (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 project      TEXT NOT NULL,
                 repo         TEXT NOT NULL,
                 job          TEXT NOT NULL,
                 args         TEXT NOT NULL,
                 resource     TEXT NOT NULL,
                 worktree     TEXT NOT NULL,
                 branch       TEXT,
                 argv         TEXT NOT NULL,
                 submitter    TEXT NOT NULL,
                 state        TEXT NOT NULL,
                 submitted_at INTEGER NOT NULL,
                 started_at   INTEGER,
                 ended_at     INTEGER,
                 pid          INTEGER,
                 pid_started  TEXT,
                 exit_code    INTEGER,
                 error        TEXT,
                 log_path     TEXT NOT NULL DEFAULT '',
                 session      TEXT,
                 delivered    INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS runs_by_state ON runs (state, id);
             CREATE TABLE IF NOT EXISTS ci_failures (
                 repo TEXT NOT NULL, pr INTEGER NOT NULL, failure TEXT NOT NULL,
                 PRIMARY KEY (repo, pr, failure)
             );
             CREATE TABLE IF NOT EXISTS usage (
                 subscription   TEXT NOT NULL,
                 minutes        INTEGER NOT NULL,
                 used_percent   REAL NOT NULL,
                 resets_at      INTEGER,
                 observed_at    INTEGER NOT NULL,
                 reset_handled  INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (subscription, minutes)
             );",
        )?;
        // Upgrade databases created before Session tracking without losing Runs.
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(runs)")?
            .query_map([], |row| row.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        if !columns.iter().any(|c| c == "session") {
            conn.execute_batch("ALTER TABLE runs ADD COLUMN session TEXT;")?;
        }
        if !columns.iter().any(|c| c == "delivered") {
            conn.execute_batch(
                "ALTER TABLE runs ADD COLUMN delivered INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
            harness TEXT NOT NULL, id TEXT NOT NULL, updated_at INTEGER NOT NULL,
            record TEXT NOT NULL, PRIMARY KEY (harness, id)
        );
        CREATE TABLE IF NOT EXISTS claims (
            repo TEXT NOT NULL, issue INTEGER NOT NULL, claimed_at INTEGER NOT NULL,
            record TEXT NOT NULL, PRIMARY KEY (repo, issue)
        );",
        )?;
        std::fs::create_dir_all(&logs)?;
        Ok(Self { conn, logs })
    }

    /// Claims a CI failure once, including across daemon restarts.
    pub fn new_ci_failure(&self, repo: &str, pr: u64, failure: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO ci_failures (repo, pr, failure) VALUES (?1, ?2, ?3)",
            params![repo, pr, failure],
        )? == 1)
    }

    pub fn insert(&self, run: NewRun) -> Result<Run> {
        self.conn.execute(
            "INSERT INTO runs (project, repo, job, args, resource, worktree, branch, argv,
                               submitter, state, submitted_at, session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10, ?11)",
            params![
                run.project,
                run.repo,
                run.job,
                serde_json::to_string(&run.args)?,
                run.resource,
                run.worktree.display().to_string(),
                run.branch,
                serde_json::to_string(&run.argv)?,
                run.submitter,
                now_ms(),
                run.session.map(|s| serde_json::to_string(&s)).transpose()?,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        let log_path = self.logs.join(format!("{id}.log"));
        self.conn.execute(
            "UPDATE runs SET log_path = ?1 WHERE id = ?2",
            params![log_path.display().to_string(), id],
        )?;
        self.require(id)
    }

    pub fn get(&self, id: i64) -> Result<Option<Run>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM runs WHERE id = ?1"),
                [id],
                from_row,
            )
            .optional()?
            .transpose()
    }

    /// Run `id`, which must exist.
    pub fn require(&self, id: i64) -> Result<Run> {
        self.get(id)?.with_context(|| format!("no run #{id}"))
    }

    pub fn with_state(&self, state: RunState) -> Result<Vec<Run>> {
        self.query(
            &format!("SELECT {COLUMNS} FROM runs WHERE state = ?1 ORDER BY id"),
            params![state.as_str()],
        )
    }

    /// Queued and running Runs, then the `finished` most recently ended ones.
    pub fn list(&self, finished: usize) -> Result<Vec<Run>> {
        let mut runs = self.with_state(RunState::Running)?;
        runs.extend(self.with_state(RunState::Queued)?);
        if finished > 0 {
            runs.extend(self.query(
                &format!(
                    "SELECT {COLUMNS} FROM runs WHERE state NOT IN ('queued', 'running')
                     ORDER BY id DESC LIMIT ?1"
                ),
                params![finished as i64],
            )?);
        }
        Ok(runs)
    }

    pub fn mark_running(&self, id: i64, pid: u32, pid_started: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET state = 'running', started_at = ?2, pid = ?3, pid_started = ?4
             WHERE id = ?1",
            params![id, now_ms(), pid, pid_started],
        )?;
        Ok(())
    }

    pub fn mark_ended(
        &self,
        id: i64,
        state: RunState,
        exit_code: Option<i32>,
        error: Option<&str>,
    ) -> Result<()> {
        // A Run that never started gets a zero duration rather than none.
        self.conn.execute(
            "UPDATE runs SET state = ?2, exit_code = ?3, error = ?4, ended_at = ?5,
                             started_at = COALESCE(started_at, ?5)
             WHERE id = ?1",
            params![id, state.as_str(), exit_code, error, now_ms()],
        )?;
        Ok(())
    }

    pub fn update_session(&self, session: &Session) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (harness, id, updated_at, record) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (harness, id) DO UPDATE SET updated_at = excluded.updated_at, record = excluded.record
             WHERE excluded.updated_at >= sessions.updated_at",
            params![session.key.harness.as_str(), session.key.id, session.last_activity, serde_json::to_string(session)?],
        )?;
        Ok(())
    }

    pub fn session(&self, key: &SessionKey) -> Result<Option<Session>> {
        self.record(
            "SELECT record FROM sessions WHERE harness = ?1 AND id = ?2",
            params![key.harness.as_str(), key.id],
        )
    }

    /// Every recorded Session, by Project with waiting ones first.
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut sessions: Vec<Session> = self.records("SELECT record FROM sessions", [])?;
        sessions.sort_by(|a, b| {
            a.project
                .cmp(&b.project)
                .then_with(|| {
                    (a.state != SessionState::Waiting).cmp(&(b.state != SessionState::Waiting))
                })
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.key.id.cmp(&b.key.id))
        });
        Ok(sessions)
    }

    /// Sessions that have not ended, in no particular order.
    pub fn live_sessions(&self) -> Result<Vec<Session>> {
        self.records(
            "SELECT record FROM sessions WHERE json_extract(record, '$.state') <> 'ended'",
            [],
        )
    }

    /// Forgets Sessions that ended before `before` (Unix ms), so history stays bounded.
    pub fn prune_sessions(&self, before: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM sessions WHERE updated_at < ?1 AND json_extract(record, '$.state') = 'ended'",
            params![before],
        )?)
    }

    /// Records a Claim. A held Claim is never replaced: it must end first.
    pub fn insert_claim(&self, claim: &Claim) -> Result<()> {
        self.conn.execute(
            "INSERT INTO claims (repo, issue, claimed_at, record) VALUES (?1, ?2, ?3, ?4)",
            params![
                claim.repo,
                sql_issue(claim.issue)?,
                claim.claimed_at,
                serde_json::to_string(claim)?
            ],
        )?;
        Ok(())
    }

    pub fn claim(&self, repo: &str, issue: u64) -> Result<Option<Claim>> {
        self.record(
            "SELECT record FROM claims WHERE repo = ?1 AND issue = ?2",
            params![repo, sql_issue(issue)?],
        )
    }

    /// Every held Claim, oldest first.
    pub fn claims(&self) -> Result<Vec<Claim>> {
        self.records(
            "SELECT record FROM claims ORDER BY claimed_at, repo, issue",
            [],
        )
    }

    /// Ends `claim`, unless it already ended and the issue was claimed again.
    /// Returns whether it was still held.
    pub fn end_claim(&self, claim: &Claim) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM claims WHERE repo = ?1 AND issue = ?2 AND claimed_at = ?3",
            params![claim.repo, sql_issue(claim.issue)?, claim.claimed_at],
        )? > 0)
    }

    /// Finished Runs whose result a `harness` Session has not acknowledged, oldest first.
    pub fn pending_results(&self, harness: Harness) -> Result<Vec<Run>> {
        self.query(
            &format!(
                "SELECT {COLUMNS} FROM runs WHERE delivered = 0 AND state NOT IN ('queued', 'running')
                 AND json_extract(session, '$.harness') = ?1 ORDER BY id"
            ),
            params![harness.as_str()],
        )
    }

    /// The oldest finished Run whose result `session` has not acknowledged.
    pub fn next_result(&self, session: &SessionKey) -> Result<Option<Run>> {
        let runs = self.query(
            &format!(
                "SELECT {COLUMNS} FROM runs WHERE delivered = 0 AND state NOT IN ('queued', 'running')
                 AND session = ?1 ORDER BY id LIMIT 1"
            ),
            params![serde_json::to_string(session)?],
        )?;
        Ok(runs.into_iter().next())
    }

    pub fn acknowledge(&self, id: i64, session: &SessionKey) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET delivered = 1 WHERE id = ?1 AND session = ?2",
            params![id, serde_json::to_string(session)?],
        )?;
        Ok(())
    }

    /// The latest known state of every Limit window, shortest window first.
    pub fn windows(&self) -> Result<Vec<Window>> {
        let mut stmt = self.conn.prepare(
            "SELECT subscription, minutes, used_percent, resets_at, observed_at, reset_handled
             FROM usage ORDER BY subscription, minutes",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Window {
                    subscription: Subscription::Claude,
                    minutes: row.get(1)?,
                    used_percent: row.get(2)?,
                    resets_at: row.get(3)?,
                    observed_at: row.get(4)?,
                    reset_handled: row.get(5)?,
                },
            ))
        })?;
        let mut windows = Vec::new();
        for row in rows {
            let (subscription, window) = row?;
            windows.push(Window {
                subscription: Subscription::parse(&subscription)?,
                ..window
            });
        }
        Ok(windows)
    }

    pub fn put_window(&self, window: &Window) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO usage
                 (subscription, minutes, used_percent, resets_at, observed_at, reset_handled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                window.subscription.as_str(),
                window.minutes,
                window.used_percent,
                window.resets_at,
                window.observed_at,
                window.reset_handled,
            ],
        )?;
        Ok(())
    }

    /// The one JSON `record` column `sql` selects, if a row matches.
    fn record<T: DeserializeOwned>(&self, sql: &str, params: impl Params) -> Result<Option<T>> {
        let text: Option<String> = self
            .conn
            .query_row(sql, params, |row| row.get(0))
            .optional()?;
        Ok(text.map(|t| serde_json::from_str(&t)).transpose()?)
    }

    /// The JSON `record` column of every row `sql` selects.
    fn records<T: DeserializeOwned>(&self, sql: &str, params: impl Params) -> Result<Vec<T>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    fn query(&self, sql: &str, params: impl Params) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, from_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row??);
        }
        Ok(runs)
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Result<Run>> {
    let args: String = row.get(4)?;
    let argv: String = row.get(8)?;
    let state: String = row.get(10)?;
    let worktree: String = row.get(6)?;
    let log_path: String = row.get(18)?;
    let session: Option<String> = row.get(19)?;
    let id: i64 = row.get(0)?;
    let project = row.get(1)?;
    let repo = row.get(2)?;
    let job = row.get(3)?;
    let resource = row.get(5)?;
    let branch = row.get(7)?;
    let submitter = row.get(9)?;
    let submitted_at = row.get(11)?;
    let started_at = row.get(12)?;
    let ended_at = row.get(13)?;
    let pid = row.get(14)?;
    let pid_started = row.get(15)?;
    let exit_code = row.get(16)?;
    let error = row.get(17)?;
    Ok((|| {
        Ok(Run {
            id,
            project,
            repo,
            job,
            args: serde_json::from_str(&args)?,
            resource,
            worktree: worktree.into(),
            branch,
            argv: serde_json::from_str(&argv)?,
            submitter,
            session: session.map(|s| serde_json::from_str(&s)).transpose()?,
            state: RunState::parse(&state)?,
            submitted_at,
            started_at,
            ended_at,
            pid,
            pid_started,
            exit_code,
            error,
            log_path: log_path.into(),
        })
    })())
}

/// An issue number as SQLite stores it.
fn sql_issue(issue: u64) -> Result<i64> {
    i64::try_from(issue).with_context(|| format!("issue #{issue} is out of range"))
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_run(job: &str) -> NewRun {
        NewRun {
            project: "queendom".into(),
            repo: "Eloyye/queendom".into(),
            job: job.into(),
            args: vec!["events".into()],
            resource: "ck3".into(),
            worktree: "/wt".into(),
            branch: Some("feat/x".into()),
            argv: vec!["/wt/test.sh".into(), "events".into()],
            submitter: "cli".into(),
            session: None,
        }
    }

    #[test]
    fn upgrades_old_run_history_and_preserves_session_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let logs = dir.path().join("runs");
        let store = Store::open(&db, logs.clone()).unwrap();
        let first = store.insert(new_run("old-job")).unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE runs DROP COLUMN session; ALTER TABLE runs DROP COLUMN delivered;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&db, logs.clone()).unwrap();
        assert_eq!(store.get(first.id).unwrap().unwrap().job, "old-job");
        assert!(store.get(first.id).unwrap().unwrap().session.is_none());
        let mut new = new_run("new-job");
        let key = SessionKey::new(Harness::Opencode, "root").unwrap();
        new.session = Some(key.clone());
        let run = store.insert(new).unwrap();
        store
            .mark_ended(run.id, RunState::Succeeded, Some(0), None)
            .unwrap();
        drop(store);
        let store = Store::open(&db, logs).unwrap();
        let pending = store.pending_results(Harness::Opencode).unwrap();
        assert_eq!(pending[0].session, Some(key.clone()));
        assert!(store.pending_results(Harness::Codex).unwrap().is_empty());
        let other = SessionKey::new(Harness::Opencode, "different-session").unwrap();
        assert!(store.next_result(&other).unwrap().is_none());
        store.acknowledge(run.id, &other).unwrap();
        assert_eq!(store.next_result(&key).unwrap().unwrap().id, run.id);
        store.acknowledge(run.id, &key).unwrap();
        assert!(store.pending_results(Harness::Opencode).unwrap().is_empty());
        assert!(store.next_result(&key).unwrap().is_none());
    }

    #[test]
    fn only_long_ended_sessions_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db"), dir.path().join("runs")).unwrap();
        let session = |id: &str, state, last_activity| Session {
            key: SessionKey::new(Harness::Codex, id).unwrap(),
            project: None,
            worktree: "/wt".into(),
            branch: None,
            state,
            last_activity,
            pending_prompts: Vec::new(),
            owner_pid: None,
            owner_started: None,
        };
        store
            .update_session(&session("old-ended", SessionState::Ended, 10))
            .unwrap();
        store
            .update_session(&session("new-ended", SessionState::Ended, 1000))
            .unwrap();
        store
            .update_session(&session("old-idle", SessionState::Idle, 10))
            .unwrap();
        assert_eq!(store.live_sessions().unwrap().len(), 1);
        assert_eq!(store.prune_sessions(500).unwrap(), 1);
        let mut left: Vec<_> = store
            .sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.key.id)
            .collect();
        left.sort();
        assert_eq!(left, ["new-ended", "old-idle"]);
    }

    #[test]
    fn a_held_claim_is_kept_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db"), dir.path().join("runs")).unwrap();
        let claim = Claim {
            repo: "o/r".into(),
            issue: 5,
            project: "demo".into(),
            session: SessionKey::new(Harness::Claude, "abc").unwrap(),
            worktree: "/wt".into(),
            branch: Some("feat/x".into()),
            git_dir: "/clone/.git".into(),
            assignee: "me".into(),
            claimed_at: 10,
        };
        store.insert_claim(&claim).unwrap();
        let other = Claim {
            session: SessionKey::new(Harness::Codex, "def").unwrap(),
            claimed_at: 20,
            ..claim.clone()
        };
        assert!(store.insert_claim(&other).is_err());
        assert_eq!(store.claim("o/r", 5).unwrap(), Some(claim.clone()));
        assert!(store.claim("o/r", 6).unwrap().is_none());
        // An end meant for an earlier Claim on the issue leaves this one alone.
        assert!(!store.end_claim(&other).unwrap());
        assert_eq!(store.claims().unwrap(), std::slice::from_ref(&claim));
        assert!(store.end_claim(&claim).unwrap());
        assert!(store.claims().unwrap().is_empty());
    }

    #[test]
    fn runs_move_through_their_states() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db"), dir.path().join("runs")).unwrap();
        let a = store.insert(new_run("ck3-test")).unwrap();
        let b = store.insert(new_run("ck3-test")).unwrap();
        assert_eq!(a.state, RunState::Queued);
        assert_eq!(a.log_path, dir.path().join("runs/1.log"));
        assert_eq!(store.with_state(RunState::Queued).unwrap().len(), 2);

        store.mark_running(a.id, 42, Some("t")).unwrap();
        store
            .mark_ended(a.id, RunState::Failed, Some(3), None)
            .unwrap();
        let a = store.get(a.id).unwrap().unwrap();
        assert_eq!(
            (a.state, a.exit_code, a.pid),
            (RunState::Failed, Some(3), Some(42))
        );
        assert!(a.duration_ms().unwrap() >= 0);

        let listed: Vec<i64> = store.list(10).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(listed, vec![b.id, a.id]);
        assert_eq!(store.list(0).unwrap().len(), 1);
    }

    #[test]
    fn windows_are_kept_per_subscription_and_length() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db"), dir.path().join("runs")).unwrap();
        let mut window = Window {
            subscription: Subscription::Chatgpt,
            minutes: 10_080,
            used_percent: 12.5,
            resets_at: Some(5_000),
            observed_at: 1_000,
            reset_handled: false,
        };
        store.put_window(&window).unwrap();
        store
            .put_window(&Window {
                subscription: Subscription::Claude,
                minutes: 300,
                resets_at: None,
                ..window.clone()
            })
            .unwrap();
        window.reset_handled = true;
        store.put_window(&window).unwrap();
        let windows = store.windows().unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].subscription, Subscription::Chatgpt);
        assert_eq!(windows[0], window);
        assert_eq!(windows[1].resets_at, None);
    }
}
