//! Messages between the CLI or MCP server and the background process: one JSON
//! request and one JSON reply per connection, each on a line, over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::attest::Destination;
use crate::claim::{Claim, ClaimStatus, IssueRef};
use crate::cleanup::{Cleaned, FinishedWorktree, Inventory};
use crate::lease::Holder;
use crate::paths::Paths;
use crate::session::{Activity, Session, SessionKey};
use crate::store::Run;
use crate::usage::{Reading, Subscription, SubscriptionUsage};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Synchronous lifecycle tracking for an explicitly requested Cross-review.
    ReviewActivity {
        activity: Activity,
    },
    /// Queue a Run of `job` in the worktree containing `cwd`.
    Submit {
        cwd: PathBuf,
        job: String,
        args: Vec<String>,
        submitter: String,
        /// Where the result should go. Only a destination a hook attested is kept.
        #[serde(default)]
        session: Option<Destination>,
    },
    GetRun {
        id: i64,
    },
    /// Queued and running Runs, plus the `finished` most recent finished ones.
    ListRuns {
        finished: usize,
    },
    Status,
    FinishedWorktrees,
    /// The exact list the CLI displayed and the user confirmed.
    Clean {
        worktrees: Vec<FinishedWorktree>,
        cwd: PathBuf,
    },
    SessionResults {
        session: SessionKey,
        ack: Option<i64>,
    },
    /// What a Harness just reported about a Subscription's Limit windows.
    ReportUsage {
        subscription: Subscription,
        readings: Vec<Reading>,
    },
    /// Every Subscription's Limit windows.
    Usage,
    /// Claim `issue` for the calling Session, from the worktree containing `cwd`.
    Claim {
        cwd: PathBuf,
        issue: IssueRef,
        /// The calling Session. Only one a hook attested is believed; otherwise
        /// it is the only live Session in the worktree.
        #[serde(default)]
        session: Option<Destination>,
    },
    /// End the Claim on `issue`. Only its holder may, unless `force`.
    Release {
        cwd: PathBuf,
        issue: IssueRef,
        #[serde(default)]
        session: Option<Destination>,
        #[serde(default)]
        force: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Recorded,
    Run(Box<RunInfo>),
    Runs {
        runs: Vec<RunInfo>,
    },
    Status(Status),
    Worktrees(Inventory),
    Cleaned(Cleaned),
    Usage {
        subscriptions: Vec<SubscriptionUsage>,
    },
    Claim {
        claim: Claim,
        /// What happened, for people and agents.
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    #[serde(flatten)]
    pub run: Run,
    /// Place in line for its Resource, from 1, while queued.
    pub queue_position: Option<usize>,
    /// The end of the log, once the Run has finished.
    pub log_tail: Option<String>,
    /// Whether the result will be sent to the submitting Session, in replies to a submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub sessions: Vec<Session>,
    #[serde(default)]
    pub claims: Vec<ClaimStatus>,
    pub resources: Vec<ResourceStatus>,
    /// Running, then queued Runs.
    pub runs: Vec<RunInfo>,
    #[serde(default)]
    pub subscriptions: Vec<SubscriptionUsage>,
    #[serde(default, flatten)]
    pub finished: Inventory,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResourceStatus {
    pub name: String,
    pub slots: u32,
    pub process: String,
    pub holders: Vec<Holder>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Envelope {
    Ok(Reply),
    Error(String),
}

pub fn encode_reply(reply: Result<Reply>) -> String {
    let envelope = match reply {
        Ok(reply) => Envelope::Ok(reply),
        Err(err) => Envelope::Error(format!("{err:#}")),
    };
    serde_json::to_string(&envelope).unwrap_or_else(|err| {
        let error = Envelope::Error(format!("encoding the reply: {err}"));
        serde_json::to_string(&error).unwrap_or_default()
    })
}

/// Sends one request to the background process and waits for its reply.
pub fn call(paths: &Paths, request: &Request) -> Result<Reply> {
    call_within(paths, request, Duration::from_secs(30))
}

pub fn call_within(paths: &Paths, request: &Request, timeout: Duration) -> Result<Reply> {
    let socket = paths.socket();
    let mut stream = UnixStream::connect(&socket).map_err(|err| {
        anyhow!(
            "cannot reach the geleider background process at {} ({err}). \
             Start it with `geleider install`, or run `geleider daemon` in a terminal.",
            socket.display()
        )
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;

    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .context("reading reply from the background process")?;
    if reply.is_empty() {
        bail!("the background process closed the connection without replying");
    }
    match serde_json::from_str(&reply).context("decoding reply")? {
        Envelope::Ok(reply) => Ok(reply),
        Envelope::Error(message) => Err(anyhow!(message)),
    }
}

impl Reply {
    pub fn into_worktrees(self) -> Result<Inventory> {
        match self {
            Reply::Worktrees(inventory) => Ok(inventory),
            other => other.unexpected(),
        }
    }

    pub fn into_cleaned(self) -> Result<Cleaned> {
        match self {
            Reply::Cleaned(cleaned) => Ok(cleaned),
            other => other.unexpected(),
        }
    }
    pub fn into_run(self) -> Result<RunInfo> {
        match self {
            Reply::Run(run) => Ok(*run),
            other => other.unexpected(),
        }
    }

    pub fn into_runs(self) -> Result<Vec<RunInfo>> {
        match self {
            Reply::Runs { runs } => Ok(runs),
            other => other.unexpected(),
        }
    }

    pub fn into_usage(self) -> Result<Vec<SubscriptionUsage>> {
        match self {
            Reply::Usage { subscriptions } => Ok(subscriptions),
            other => other.unexpected(),
        }
    }

    pub fn into_claim(self) -> Result<(Claim, String)> {
        match self {
            Reply::Claim { claim, message } => Ok((claim, message)),
            other => other.unexpected(),
        }
    }

    pub fn into_status(self) -> Result<Status> {
        match self {
            Reply::Status(status) => Ok(status),
            other => other.unexpected(),
        }
    }

    fn unexpected<T>(&self) -> Result<T> {
        let kind = match self {
            Reply::Recorded => "recorded",
            Reply::Run(_) => "run",
            Reply::Runs { .. } => "runs",
            Reply::Status(_) => "status",
            Reply::Worktrees(_) => "worktrees",
            Reply::Cleaned(_) => "cleaned",
            Reply::Usage { .. } => "usage",
            Reply::Claim { .. } => "claim",
        };
        bail!("unexpected `{kind}` reply from the background process")
    }
}
