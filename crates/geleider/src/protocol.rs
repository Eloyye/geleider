//! Messages between the CLI or MCP server and the background process: one JSON
//! request and one JSON reply per connection, each on a line, over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::lease::Holder;
use crate::paths::Paths;
use crate::store::Run;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Queue a Run of `job` in the worktree containing `cwd`.
    Submit {
        cwd: PathBuf,
        job: String,
        args: Vec<String>,
        submitter: String,
    },
    GetRun {
        id: i64,
    },
    /// Queued and running Runs, plus the `finished` most recent finished ones.
    ListRuns {
        finished: usize,
    },
    Status,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Run(Box<RunInfo>),
    Runs { runs: Vec<RunInfo> },
    Status(Status),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    #[serde(flatten)]
    pub run: Run,
    /// Place in line for its Resource, from 1, while queued.
    pub queue_position: Option<usize>,
    /// The end of the log, once the Run has finished.
    pub log_tail: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub resources: Vec<ResourceStatus>,
    /// Running, then queued Runs.
    pub runs: Vec<RunInfo>,
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
    serde_json::to_string(&envelope).expect("replies serialize")
}

/// Sends one request to the background process and waits for its reply.
pub fn call(paths: &Paths, request: &Request) -> Result<Reply> {
    let socket = paths.socket();
    let mut stream = UnixStream::connect(&socket).map_err(|err| {
        anyhow!(
            "cannot reach the geleider background process at {} ({err}). \
             Start it with `geleider install`, or run `geleider daemon` in a terminal.",
            socket.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
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
    pub fn into_run(self) -> Result<RunInfo> {
        match self {
            Reply::Run(run) => Ok(*run),
            _ => bail!("unexpected reply from the background process"),
        }
    }

    pub fn into_runs(self) -> Result<Vec<RunInfo>> {
        match self {
            Reply::Runs { runs } => Ok(runs),
            _ => bail!("unexpected reply from the background process"),
        }
    }

    pub fn into_status(self) -> Result<Status> {
        match self {
            Reply::Status(status) => Ok(status),
            _ => bail!("unexpected reply from the background process"),
        }
    }
}
