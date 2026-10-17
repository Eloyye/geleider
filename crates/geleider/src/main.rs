mod attest;
mod cli;
mod config;
mod daemon;
mod delivery;
mod git;
mod github;
mod hook;
mod hooks_install;
mod install;
mod lease;
mod mcp;
mod paths;
mod procs;
mod protocol;
mod report;
mod session;
mod statusline;
mod store;
mod usage;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::paths::Paths;

/// Coordinates coding-agent work across Claude Code, Codex and OpenCode on one Mac.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Submit a Run of a Job in the current worktree. Returns at once with the Run id
    /// and queue position.
    Run {
        /// A Job declared for this Project in geleider's config.
        job: String,
        /// The Job's args. Put `--` before any that start with `-`.
        args: Vec<String>,
        /// Block until the Run ends and exit with its exit code.
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// A Run's state and, once it has started, its result and log tail.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Block until a Run ends and exit with its exit code.
    Wait {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Running and queued Runs.
    Queue {
        /// Also list recently finished Runs.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Sessions by Project, Resources and their holders, running and queued Runs,
    /// and how much of each Subscription is used in each Limit window.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Report a Harness lifecycle event from JSON on stdin. Always succeeds.
    Hook {
        harness: session::Harness,
        event: String,
    },
    /// Internal OpenCode plugin result mailbox.
    #[command(hide = true)]
    SessionResults {
        harness: session::Harness,
        id: String,
        #[arg(long)]
        ack: Option<i64>,
    },
    /// How much of each Subscription is used in each Limit window, and when each resets.
    Usage {
        #[arg(long)]
        json: bool,
    },
    /// Claude Code's status line, set by `install`: reads Claude Code's JSON on stdin,
    /// passes its usage limits to geleider and prints a status line.
    Statusline {
        /// A status line to run with the same input and show instead of geleider's own.
        #[arg(long)]
        wrap: Option<String>,
    },
    /// Run the background process in the foreground.
    Daemon,
    /// Serve MCP over stdio for a Harness.
    Mcp,
    /// Start the background process at login and register MCP and Session hooks in
    /// Claude Code, Codex and OpenCode.
    Install,
    /// Undo `install`. Config and Run history are kept.
    Uninstall,
}

fn main() {
    let code = match run(Cli::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("geleider: {err:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    // Hook failures, including a missing HOME/state directory, never escape to a Harness.
    if let Cmd::Hook { harness, ref event } = cli.command {
        hook::run(harness, event);
        return Ok(0);
    }
    let paths = Paths::from_env()?;
    match cli.command {
        Cmd::Run {
            job,
            args,
            wait,
            json,
        } => cli::submit(&paths, job, args, wait, json),
        Cmd::Show { id, json } => cli::show(&paths, cli::parse_run_id(&id)?, json),
        Cmd::Wait { id, json } => cli::wait_for(&paths, cli::parse_run_id(&id)?, json),
        Cmd::Queue { all, json } => cli::queue(&paths, all, json),
        Cmd::Status { json } => cli::status(&paths, json),
        // Handled above, before anything that can fail.
        Cmd::Hook { .. } => Ok(0),
        Cmd::SessionResults { harness, id, ack } => cli::session_results(&paths, harness, &id, ack),
        Cmd::Usage { json } => cli::usage(&paths, json),
        Cmd::Statusline { wrap } => statusline::run(&paths, wrap.as_deref()),
        Cmd::Daemon => daemon::run(paths).map(|()| 0),
        Cmd::Mcp => mcp::serve(&paths).map(|()| 0),
        Cmd::Install => install::install(&paths).map(|()| 0),
        Cmd::Uninstall => install::uninstall().map(|()| 0),
    }
}
