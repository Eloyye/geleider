mod cli;
mod config;
mod daemon;
mod git;
mod github;
mod install;
mod lease;
mod mcp;
mod paths;
mod procs;
mod protocol;
mod report;
mod store;

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
    /// Resources with their slots and current holders, plus running and queued Runs.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Run the background process in the foreground.
    Daemon,
    /// Serve MCP over stdio for a Harness.
    Mcp,
    /// Start the background process at login and register the MCP server in
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
        Cmd::Daemon => daemon::run(paths).map(|()| 0),
        Cmd::Mcp => mcp::serve(&paths).map(|()| 0),
        Cmd::Install => install::install(&paths).map(|()| 0),
        Cmd::Uninstall => install::uninstall().map(|()| 0),
    }
}
