//! Runs, kept in SQLite so the queue survives the background process restarting.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

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
}

pub struct Store {
    conn: Connection,
    logs: PathBuf,
}

const COLUMNS: &str = "id, project, repo, job, args, resource, worktree, branch, argv, submitter, \
     state, submitted_at, started_at, ended_at, pid, pid_started, exit_code, error, log_path";

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
                 log_path     TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS runs_by_state ON runs (state, id);
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
        std::fs::create_dir_all(&logs)?;
        Ok(Self { conn, logs })
    }

    pub fn insert(&self, run: NewRun) -> Result<Run> {
        self.conn.execute(
            "INSERT INTO runs (project, repo, job, args, resource, worktree, branch, argv,
                               submitter, state, submitted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10)",
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

    fn query(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Run>> {
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
        }
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
