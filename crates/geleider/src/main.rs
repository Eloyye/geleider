mod attest;
mod ci;
mod claim;
mod cleanup;
mod cli;
mod command;
mod config;
mod daemon;
mod delivery;
mod dispatch;
mod git;
mod github;
mod hook;
mod hooks_install;
mod import;
mod install;
mod lease;
mod mcp;
mod panel;
mod paths;
mod procs;
mod protocol;
mod report;
mod review;
mod session;
mod statusline;
mod store;
mod text;
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
    /// Panels for Subscription usage, Sessions and their Claims, Resources and their
    /// Runs, and Claims and Finished worktrees.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List Finished worktrees and remove them only after explicit confirmation.
    Clean,
    /// Cross-review a PR with the other vendor, after showing usage and confirming.
    Review {
        /// PR number, #number, or a GitHub PR URL in the current Project.
        pr: String,
        /// Required when the writing Session's vendor is unknown.
        #[arg(long, value_enum)]
        with: Option<review::Reviewer>,
        /// Start after displaying usage without waiting for a confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Start a new background Session on each of the Project's issues or PRs from a
    /// Profile, each in a new worktree with the Project's Setup, after showing usage
    /// and confirming. Without numbers, pick from the open ones.
    Dispatch {
        /// Issues or PRs: `12`, `#12` or `owner/name#12`.
        numbers: Vec<String>,
        /// A Profile from geleider's config; defaults to the Project's default_profile.
        #[arg(long)]
        profile: Option<String>,
        /// Text added after the Profile's prompt template.
        #[arg(long, conflicts_with = "prompt_file")]
        prompt: Option<String>,
        /// A file whose text is added after the Profile's prompt template.
        #[arg(long)]
        prompt_file: Option<std::path::PathBuf>,
        /// Start with only the --prompt or --prompt-file text, without the template.
        #[arg(long)]
        no_template: bool,
        /// Start after showing what will start, without waiting for a confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Turn Claude Desktop's session presets into Profiles and each Project's Codex
    /// setup script into its Setup, after showing them and confirming.
    Import,
    /// Claim an issue or PR for the Session working in this worktree: assigns it to you
    /// on GitHub and comments naming the Session. Fails, naming the holder, if
    /// another Session holds it or it is assigned already.
    Claim {
        /// `5`, `#5` or `owner/name#5`.
        issue: String,
        #[arg(long)]
        json: bool,
    },
    /// End a Claim: unassigns you and comments that the Claim was released.
    Release {
        /// `5`, `#5`, or `owner/name#5` from outside the Project.
        issue: String,
        /// Release a Claim another Session holds.
        #[arg(long)]
        force: bool,
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
    /// Restart the installed background process so it runs the installed binary,
    /// after listing the running Runs that would end as `lost` and confirming.
    Restart {
        /// Restart without waiting for a confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
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
            // Errors can quote paths and branches a Session chose.
            eprintln!("geleider: {}", text::printable(&format!("{err:#}")));
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
        Cmd::Clean => cli::clean(&paths),
        Cmd::Review { pr, with, yes } => review::run(&paths, &pr, with, yes),
        Cmd::Dispatch {
            numbers,
            profile,
            prompt,
            prompt_file,
            no_template,
            yes,
        } => dispatch::run(
            &paths,
            dispatch::Options {
                numbers,
                profile,
                prompt,
                prompt_file,
                no_template,
                yes,
            },
        ),
        Cmd::Import => import::run(&paths),
        Cmd::Claim { issue, json } => cli::claim(&paths, &issue, json),
        Cmd::Release { issue, force, json } => cli::release(&paths, &issue, force, json),
        // Handled above, before anything that can fail.
        Cmd::Hook { .. } => Ok(0),
        Cmd::SessionResults { harness, id, ack } => cli::session_results(&paths, harness, &id, ack),
        Cmd::Usage { json } => cli::usage(&paths, json),
        Cmd::Statusline { wrap } => statusline::run(&paths, wrap.as_deref()),
        Cmd::Daemon => daemon::run(paths).map(|()| 0),
        Cmd::Restart { yes } => cli::restart(&paths, yes),
        Cmd::Mcp => mcp::serve(&paths).map(|()| 0),
        Cmd::Install => install::install(&paths).map(|()| 0),
        Cmd::Uninstall => install::uninstall().map(|()| 0),
    }
}
