//! The command-line side of submitting and checking Runs, and of Claims.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::claim::{ClaimStatus, IssueRef};
use crate::cleanup::Inventory;
use crate::lease::Holder;
use crate::paths::{self, Paths};
use crate::protocol::{self, Request, RunInfo, Status};
use crate::report;
use crate::session::{Harness, SessionKey, SessionState};
use crate::store::{RunState, now_ms};
use crate::text;
use crate::usage::{SubscriptionUsage, WindowView};

/// Finished Runs `geleider queue --all` shows.
const RECENT: usize = 20;

pub fn submit(
    paths: &Paths,
    job: String,
    args: Vec<String>,
    wait: bool,
    json: bool,
) -> Result<i32> {
    let request = Request::Submit {
        cwd: std::env::current_dir()?,
        job,
        args,
        submitter: "cli".into(),
        // A shell's environment, such as `CODEX_THREAD_ID`, is the model's to
        // choose, so it never names where a result goes. Use `--wait` instead.
        session: None,
    };
    let info = protocol::call(paths, &request)?.into_run()?;
    if !wait {
        print_as(json, &info, || one_line(&info) + "\n")?;
        return Ok(0);
    }
    eprintln!("{}", one_line(&info));
    wait_for(paths, info.run.id, json)
}

/// The OpenCode plugin's result mailbox: at most one finished Run for `id` as
/// `[{run_id, message}]`, after acknowledging `ack` if given.
pub fn session_results(paths: &Paths, harness: Harness, id: &str, ack: Option<i64>) -> Result<i32> {
    let session = SessionKey::new(harness, id).context("invalid Session id")?;
    let runs = protocol::call(paths, &Request::SessionResults { session, ack })?.into_runs()?;
    let messages: Vec<_> = runs
        .iter()
        .map(|r| serde_json::json!({"run_id": r.run.id, "message": crate::delivery::message(&r.run)}))
        .collect();
    println!("{}", serde_json::to_string(&messages)?);
    Ok(0)
}

/// Blocks until Run `id` ends, prints its result and returns its exit code, so a
/// Harness can run this in the background and be woken when it finishes.
pub fn wait_for(paths: &Paths, id: i64, json: bool) -> Result<i32> {
    let mut last_state = None;
    let mut unreachable_since: Option<Instant> = None;
    loop {
        match protocol::call(paths, &Request::GetRun { id }).and_then(|r| r.into_run()) {
            Ok(info) => {
                unreachable_since = None;
                if info.run.state.is_finished() {
                    print_result(&info, json)?;
                    return Ok(exit_code(&info));
                }
                if last_state.is_some_and(|s| s != info.run.state) {
                    eprintln!("{}", one_line(&info));
                }
                last_state = Some(info.run.state);
            }
            // The background process may be restarting; launchd brings it back.
            Err(err) => {
                let since = *unreachable_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(60) {
                    return Err(err);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub fn show(paths: &Paths, id: i64, json: bool) -> Result<i32> {
    let info = protocol::call(paths, &Request::GetRun { id })?.into_run()?;
    print_result(&info, json)?;
    Ok(0)
}

pub fn queue(paths: &Paths, all: bool, json: bool) -> Result<i32> {
    let finished = if all { RECENT } else { 0 };
    let runs = protocol::call(paths, &Request::ListRuns { finished })?.into_runs()?;
    print_as(json, &runs, || {
        if runs.is_empty() {
            return "No runs queued or running.\n".into();
        }
        runs.iter().map(|info| one_line(info) + "\n").collect()
    })?;
    Ok(0)
}

pub fn status(paths: &Paths, json: bool) -> Result<i32> {
    let status = protocol::call(paths, &Request::Status)?.into_status()?;
    print_as(json, &status, || render_status(&status))?;
    Ok(0)
}

pub fn clean(paths: &Paths) -> Result<i32> {
    // A fresh scan may query several Projects; leave time for those network calls.
    let timeout = Duration::from_secs(300);
    let mut inventory =
        protocol::call_within(paths, &Request::FinishedWorktrees, timeout)?.into_worktrees()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    inventory
        .finished_worktrees
        .retain(|w| !cwd.starts_with(&w.worktree));
    print!("{}", render_finished(&inventory));
    if inventory.finished_worktrees.is_empty() {
        return Ok(0);
    }
    print!(
        "\nRemove these {} worktrees? [y/N] ",
        inventory.finished_worktrees.len()
    );
    std::io::stdout().flush()?;
    if !confirmed(&mut std::io::stdin().lock())? {
        println!("Cancelled; no worktrees removed.");
        return Ok(0);
    }
    let cleaned = protocol::call_within(
        paths,
        &Request::Clean {
            worktrees: inventory.finished_worktrees,
            cwd,
        },
        timeout,
    )?
    .into_cleaned()?;
    for path in &cleaned.removed {
        println!("Removed {}", shown_path(path));
    }
    for reason in &cleaned.skipped {
        eprintln!("Skipped {}", text::one_line(reason));
    }
    Ok(i32::from(!cleaned.skipped.is_empty()))
}

fn confirmed(input: &mut impl BufRead) -> Result<bool> {
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn render_finished(inventory: &Inventory) -> String {
    let mut out = String::from("Finished worktrees\n");
    if inventory.finished_worktrees.is_empty() {
        out.push_str("  (none)\n");
    }
    // Worktree paths and branch names are chosen by Sessions and are shown before
    // a confirmation prompt, so each stays on its own line with no escapes.
    for wt in &inventory.finished_worktrees {
        out.push_str(&format!(
            "  {}  {}  PR #{} ({})  {}\n",
            wt.project,
            text::one_line(&wt.branch),
            wt.pr,
            wt.pr_state,
            shown_path(&wt.worktree)
        ));
    }
    for warning in &inventory.cleanup_warnings {
        out.push_str(&format!("  Could not check {}\n", text::one_line(warning)));
    }
    out
}

fn shown_path(path: &Path) -> String {
    text::one_line(&paths::display(path))
}

/// Claims `issue` for the Session working in this worktree. The CLI names no
/// Session: the daemon takes the only live one in the worktree.
pub fn claim(paths: &Paths, issue: &str, json: bool) -> Result<i32> {
    let request = Request::Claim {
        cwd: std::env::current_dir()?,
        issue: IssueRef::parse(issue)?,
        session: None,
    };
    claim_reply(paths, &request, json)
}

pub fn release(paths: &Paths, issue: &str, force: bool, json: bool) -> Result<i32> {
    let request = Request::Release {
        cwd: std::env::current_dir()?,
        issue: IssueRef::parse(issue)?,
        session: None,
        force,
    };
    claim_reply(paths, &request, json)
}

fn claim_reply(paths: &Paths, request: &Request, json: bool) -> Result<i32> {
    let (claim, message) = protocol::call(paths, request)?.into_claim()?;
    let value = serde_json::json!({ "claim": claim, "message": message });
    print_as(json, &value, || format!("{message}\n"))?;
    Ok(0)
}

pub fn usage(paths: &Paths, json: bool) -> Result<i32> {
    let subscriptions = protocol::call(paths, &Request::Usage)?.into_usage()?;
    print_as(json, &subscriptions, || {
        render_usage(&subscriptions, now_ms())
    })?;
    Ok(0)
}

/// Prints `value` as JSON for scripts, or `text` for people. Branches, paths,
/// args and logs in `text` come from Sessions, so terminal escapes in them are
/// dropped; JSON escapes them itself.
fn print_as(json: bool, value: &impl Serialize, text: impl FnOnce() -> String) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        print!("{}", text::printable(&text()));
    }
    Ok(())
}

fn render_status(status: &Status) -> String {
    let mut out = String::from("Sessions\n");
    let mut project = None;
    let live: Vec<_> = status
        .sessions
        .iter()
        .filter(|s| s.state != SessionState::Ended)
        .collect();
    if live.is_empty() {
        out.push_str("  (none)\n");
    }
    for session in live {
        let name = session
            .project
            .as_deref()
            .unwrap_or("(unconfigured Project)");
        if project != Some(name) {
            out.push_str(&format!("  {name}\n"));
            project = Some(name);
        }
        out.push_str(&format!(
            "    {:<8} {:<7} {}  {}  {}  active {} ago\n",
            session.state.as_str(),
            session.key.harness.as_str(),
            session.key.id,
            session.branch.as_deref().unwrap_or("(detached)"),
            paths::display(&session.worktree),
            report::duration(now_ms() - session.last_activity)
        ));
    }
    out.push_str(&render_claims(&status.claims, now_ms()));
    out.push_str("\nResources\n");
    if status.resources.is_empty() {
        out.push_str("  (none configured)\n");
    }
    for resource in &status.resources {
        out.push_str(&format!(
            "  {}  {}/{} slots used  (watching `{}`)\n",
            resource.name,
            resource.holders.len(),
            resource.slots,
            resource.process
        ));
        for holder in &resource.holders {
            let line = match holder {
                Holder::Run { id } => match status.runs.iter().find(|r| r.run.id == *id) {
                    Some(info) => format!(
                        "run #{id}  {}  {}",
                        report::title(&info.run),
                        info.run.project
                    ),
                    None => format!("run #{id}"),
                },
                Holder::Process { pid, name } => {
                    format!("{name} (pid {pid}, not started by geleider)")
                }
            };
            out.push_str(&format!("    {line}\n"));
        }
    }
    for (heading, state) in [("Running", RunState::Running), ("Queued", RunState::Queued)] {
        let runs: Vec<&RunInfo> = status
            .runs
            .iter()
            .filter(|r| r.run.state == state)
            .collect();
        out.push_str(&format!("\n{heading}\n"));
        if runs.is_empty() {
            out.push_str("  (none)\n");
        }
        for info in runs {
            out.push_str(&format!("  {}\n", one_line(info)));
        }
    }
    if !status.subscriptions.is_empty() {
        out.push('\n');
        out.push_str(&render_usage(&status.subscriptions, now_ms()));
    }
    out.push('\n');
    out.push_str(&render_finished(&status.finished));
    out
}

/// e.g. `Eloyye/geleider#5  codex 019a…  codex/issue-5-x  claimed 3h 10m ago, active 2m 05s ago`.
fn render_claims(claims: &[ClaimStatus], now: i64) -> String {
    let mut out = String::from("\nClaims\n");
    if claims.is_empty() {
        out.push_str("  (none)\n");
    }
    for status in claims {
        let claim = &status.claim;
        let quiet = report::duration(now - status.last_activity);
        let activity = match (status.quiet, status.session_state) {
            (true, state) => format!(
                "QUIET for {quiet}{}: check on it, or `geleider release {} --force`",
                if state == Some(SessionState::Ended) {
                    ", Session ended"
                } else {
                    ""
                },
                claim.name()
            ),
            (false, Some(SessionState::Ended)) => format!("Session ended {quiet} ago"),
            (false, _) => format!("active {quiet} ago"),
        };
        out.push_str(&format!(
            "  {}  {} {}  {}  claimed {} ago, {activity}\n",
            claim.name(),
            claim.session.harness.as_str(),
            claim.session.id,
            claim.branch.as_deref().unwrap_or("(detached)"),
            report::duration(now - claim.claimed_at),
        ));
    }
    out
}

pub fn render_usage(subscriptions: &[SubscriptionUsage], now: i64) -> String {
    let mut out = String::from("Subscriptions\n");
    for usage in subscriptions {
        out.push_str(&format!("  {}  ({})\n", usage.plan, usage.source));
        if usage.windows.is_empty() {
            out.push_str(&format!("    {}\n", usage.subscription.no_data_hint()));
        }
        for window in &usage.windows {
            out.push_str(&format!("    {}\n", window_line(window, now)));
        }
    }
    out
}

/// e.g. `5h  23% used, resets 14:05 (in 2h 10m)  updated 3m ago`.
fn window_line(window: &WindowView, now: i64) -> String {
    let state = match (window.used_percent, window.resets_at) {
        (Some(used), Some(at)) => format!(
            "{used:>3.0}% used, resets {} (in {})",
            report::clock(at, now),
            report::duration(at - now)
        ),
        (Some(used), None) => format!("{used:>3.0}% used"),
        (None, Some(at)) => format!("reset at {}, no data since", report::clock(at, now)),
        (None, None) => "unknown".into(),
    };
    let age = report::duration(window.age_ms);
    let freshness = if window.stale {
        format!(
            "last known at {} ({age} ago)",
            report::clock(window.observed_at, now)
        )
    } else {
        format!("updated {age} ago")
    };
    format!("{:<3} {state}  {freshness}", window.window)
}

/// e.g. `#12 running  ck3-test events  queendom  feat/x  for 3m 12s`.
pub fn one_line(info: &RunInfo) -> String {
    let run = &info.run;
    let detail = match run.state {
        RunState::Queued => {
            let place = info.queue_position.map_or(String::new(), |p| {
                format!("#{p} in line for {}, ", run.resource)
            });
            format!(
                "{place}waiting {}",
                report::duration(now_ms() - run.submitted_at)
            )
        }
        RunState::Running => format!("for {}", report::duration(run.duration_ms().unwrap_or(0))),
        _ => format!(
            "{} in {}",
            report::outcome(run),
            report::duration(run.duration_ms().unwrap_or(0))
        ),
    };
    format!(
        "#{} {:<9} {}  {}  {}  {detail}",
        run.id,
        run.state,
        report::title(run),
        run.project,
        run.branch.as_deref().unwrap_or("(detached)"),
    )
}

fn print_result(info: &RunInfo, json: bool) -> Result<()> {
    print_as(json, info, || render_result(info))
}

/// A Run's one-line summary, its details and, once it has finished, its log tail.
fn render_result(info: &RunInfo) -> String {
    let run = &info.run;
    let mut out = format!("{}\n", one_line(info));
    out.push_str(&format!("  worktree   {}\n", paths::display(&run.worktree)));
    if let Some(code) = run.exit_code {
        out.push_str(&format!("  exit code  {code}\n"));
    }
    if let Some(ms) = run.duration_ms() {
        out.push_str(&format!("  duration   {}\n", report::duration(ms)));
    }
    if let Some(error) = &run.error {
        out.push_str(&format!("  error      {error}\n"));
    }
    out.push_str(&format!(
        "  log        {}\n",
        paths::display(&PathBuf::from(&run.log_path))
    ));
    if let Some(tail) = info.log_tail.as_deref().filter(|t| !t.is_empty()) {
        out.push_str(&format!("\n{tail}\n"));
    }
    out
}

/// The Run's own exit code; 1 when it has none (lost, or never started).
fn exit_code(info: &RunInfo) -> i32 {
    match (info.run.state, info.run.exit_code) {
        (RunState::Succeeded, _) => 0,
        (_, Some(code)) if code != 0 => code,
        _ => 1,
    }
}

pub fn parse_run_id(text: &str) -> Result<i64> {
    match text.trim_start_matches('#').parse() {
        Ok(id) => Ok(id),
        Err(_) => bail!("`{text}` is not a run id"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_requires_an_explicit_yes() {
        for answer in ["", "\n", "no\n", "maybe\n", "true\n"] {
            assert!(!confirmed(&mut answer.as_bytes()).unwrap());
        }
        for answer in ["y\n", "yes\n", "YES\n"] {
            assert!(confirmed(&mut answer.as_bytes()).unwrap());
        }
    }
}
