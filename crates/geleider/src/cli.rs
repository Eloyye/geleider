//! The command-line side of submitting and checking Runs.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::lease::Holder;
use crate::paths::{self, Paths};
use crate::protocol::{self, Request, RunInfo, Status};
use crate::report;
use crate::store::{RunState, now_ms};
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
    };
    let info = protocol::call(paths, &request)?.into_run()?;
    if !wait {
        if json {
            println!("{}", serde_json::to_string_pretty(&info)?);
        } else {
            println!("{}", one_line(&info));
        }
        return Ok(0);
    }
    eprintln!("{}", one_line(&info));
    wait_for(paths, info.run.id, json)
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
    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
    } else if runs.is_empty() {
        println!("No runs queued or running.");
    } else {
        for info in &runs {
            println!("{}", one_line(info));
        }
    }
    Ok(0)
}

pub fn status(paths: &Paths, json: bool) -> Result<i32> {
    let status = protocol::call(paths, &Request::Status)?.into_status()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print!("{}", render_status(&status));
    }
    Ok(0)
}

pub fn usage(paths: &Paths, json: bool) -> Result<i32> {
    let subscriptions = protocol::call(paths, &Request::Usage)?.into_usage()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&subscriptions)?);
    } else {
        print!("{}", render_usage(&subscriptions, now_ms()));
    }
    Ok(0)
}

fn render_status(status: &Status) -> String {
    let mut out = String::from("Resources\n");
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
    if json {
        println!("{}", serde_json::to_string_pretty(info)?);
        return Ok(());
    }
    let run = &info.run;
    println!("{}", one_line(info));
    println!("  worktree   {}", paths::display(&run.worktree));
    if let Some(code) = run.exit_code {
        println!("  exit code  {code}");
    }
    if let Some(ms) = run.duration_ms() {
        println!("  duration   {}", report::duration(ms));
    }
    if let Some(error) = &run.error {
        println!("  error      {error}");
    }
    println!(
        "  log        {}",
        paths::display(&PathBuf::from(&run.log_path))
    );
    if let Some(tail) = info.log_tail.as_deref().filter(|t| !t.is_empty()) {
        println!("\n{tail}");
    }
    Ok(())
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
