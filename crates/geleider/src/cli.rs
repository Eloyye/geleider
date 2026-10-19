//! The command-line side of submitting and checking Runs, and of Claims.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::claim::IssueRef;
use crate::cleanup::{FinishedWorktree, Inventory};
use crate::config::Config;
use crate::install;
use crate::lease::Holder;
use crate::paths::{self, Paths};
use crate::protocol::{self, Request, RunInfo, Status};
use crate::report;
use crate::session::{Harness, Session, SessionKey, SessionState};
use crate::store::{RunState, now_ms};
use crate::text;
use crate::ui::{self, Line, Span, Style, Term};
use crate::usage::{Subscription, SubscriptionUsage, WindowView};

/// Finished Runs `geleider queue --all` shows.
const RECENT: usize = 20;

pub fn submit(
    term: &mut Term,
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
        print_as(term, json, &info, || one_line(&info) + "\n")?;
        return Ok(0);
    }
    term.say_err(&one_line(&info));
    wait_for(term, paths, info.run.id, json)
}

/// The OpenCode plugin's result mailbox: at most one finished Run for `id` as
/// `[{run_id, message}]`, after acknowledging `ack` if given.
#[allow(
    clippy::disallowed_macros,
    reason = "the OpenCode plugin reads this JSON, not a person"
)]
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
pub fn wait_for(term: &mut Term, paths: &Paths, id: i64, json: bool) -> Result<i32> {
    let mut last_state = None;
    let mut unreachable_since: Option<Instant> = None;
    loop {
        match protocol::call(paths, &Request::GetRun { id }).and_then(|r| r.into_run()) {
            Ok(info) => {
                unreachable_since = None;
                if info.run.state.is_finished() {
                    print_result(term, &info, json)?;
                    return Ok(exit_code(&info));
                }
                if last_state.is_some_and(|s| s != info.run.state) {
                    term.say_err(&one_line(&info));
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

pub fn show(term: &mut Term, paths: &Paths, id: i64, json: bool) -> Result<i32> {
    let info = protocol::call(paths, &Request::GetRun { id })?.into_run()?;
    print_result(term, &info, json)?;
    Ok(0)
}

pub fn queue(term: &mut Term, paths: &Paths, all: bool, json: bool) -> Result<i32> {
    let finished = if all { RECENT } else { 0 };
    let runs = protocol::call(paths, &Request::ListRuns { finished })?.into_runs()?;
    print_as(term, json, &runs, || {
        if runs.is_empty() {
            return "No runs queued or running.\n".into();
        }
        runs.iter().map(|info| one_line(info) + "\n").collect()
    })?;
    Ok(0)
}

pub fn status(term: &mut Term, paths: &Paths, json: bool) -> Result<i32> {
    let status = protocol::call(paths, &Request::Status)?.into_status()?;
    if json {
        print_json(&status)?;
    } else {
        term.show(&render_status(&status, now_ms(), term.width()));
    }
    Ok(0)
}

pub fn clean(term: &mut Term, paths: &Paths, targets: &[String], yes: bool) -> Result<i32> {
    // A fresh scan may query several Projects; leave time for those network calls.
    let timeout = Duration::from_secs(300);
    let mut inventory =
        protocol::call_within(paths, &Request::FinishedWorktrees, timeout)?.into_worktrees()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    inventory
        .finished_worktrees
        .retain(|w| !cwd.starts_with(&w.worktree));
    let finished = std::mem::take(&mut inventory.finished_worktrees);
    let chosen = if !targets.is_empty() {
        inventory.finished_worktrees = resolve_targets(&finished, targets, &cwd)?;
        term.say(&render_finished(&inventory));
        inventory.finished_worktrees
    } else if finished.len() > 1 {
        term.say("Finished worktrees");
        for warning in &inventory.cleanup_warnings {
            term.say(&format!("  Could not check {}", text::one_line(warning)));
        }
        // Every row starts checked; uncheck the worktrees to keep. Without a
        // terminal nothing is asked, and the confirmation below decides.
        let rows: Vec<Line> = finished
            .iter()
            .map(|wt| vec![Span::plain(finished_row(wt))])
            .collect();
        let Some(checked) =
            term.pick_many("Uncheck worktrees to keep", &rows, &vec![true; rows.len()])?
        else {
            term.say("Cancelled; no worktrees removed.");
            return Ok(0);
        };
        if checked.len() < finished.len() {
            term.say("To keep a worktree for good, run `git worktree lock <path>`.");
        }
        checked.into_iter().map(|i| finished[i].clone()).collect()
    } else {
        inventory.finished_worktrees = finished;
        term.say(&render_finished(&inventory));
        if inventory.finished_worktrees.is_empty() {
            return Ok(0);
        }
        inventory.finished_worktrees
    };
    let Some(question) = removal_question(chosen.len()) else {
        term.say("Nothing selected; no worktrees removed.");
        return Ok(0);
    };
    if !yes && !term.confirm(&question)? {
        term.say("Cancelled; no worktrees removed.");
        return Ok(0);
    }
    let cleaned = protocol::call_within(
        paths,
        &Request::Clean {
            worktrees: chosen,
            cwd,
        },
        timeout,
    )?
    .into_cleaned()?;
    for path in &cleaned.removed {
        term.say(&format!("Removed {}", shown_path(path)));
    }
    for reason in &cleaned.skipped {
        term.say_err(&format!("Skipped {}", text::one_line(reason)));
    }
    Ok(i32::from(!cleaned.skipped.is_empty()))
}

/// The Finished worktrees that `clean <target>…` names, each by its PR
/// (`owner/name#N`, or `#N` when only one Project has it) or its worktree path.
/// A target that matches none, or several, is an error, so nothing is removed.
fn resolve_targets(
    finished: &[FinishedWorktree],
    targets: &[String],
    cwd: &Path,
) -> Result<Vec<FinishedWorktree>> {
    let mut chosen: Vec<FinishedWorktree> = Vec::new();
    for target in targets {
        let matches: Vec<&FinishedWorktree> = match IssueRef::parse(target) {
            Ok(pr) => finished
                .iter()
                .filter(|w| {
                    w.pr == pr.number
                        && pr
                            .repo
                            .as_ref()
                            .is_none_or(|repo| repo.eq_ignore_ascii_case(pr_repo(w)))
                })
                .collect(),
            Err(_) => {
                let path = cwd.join(target);
                let path = path.canonicalize().unwrap_or(path);
                finished.iter().filter(|w| w.worktree == path).collect()
            }
        };
        let wt = match matches[..] {
            [wt] => wt,
            [] => bail!("{} is not a Finished worktree", text::one_line(target)),
            _ => bail!(
                "{} matches several Finished worktrees; name one: {}",
                text::one_line(target),
                matches
                    .iter()
                    .map(|w| format!("{}#{}", pr_repo(w), w.pr))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        if !chosen.contains(wt) {
            chosen.push(wt.clone());
        }
    }
    Ok(chosen)
}

/// The question asked about the chosen worktrees, or none when nothing is chosen.
fn removal_question(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("Remove this worktree?".into()),
        n => Some(format!("Remove these {n} worktrees?")),
    }
}

/// `owner/name` from the PR's GitHub URL.
fn pr_repo(wt: &FinishedWorktree) -> &str {
    let path = wt.pr_url.trim_start_matches("https://github.com/");
    path.rsplitn(3, '/').nth(2).unwrap_or(path)
}

/// Restarts the background process so it runs the installed binary. Running Runs
/// keep running, but end as `lost`, so they are listed and confirmed first.
pub fn restart(term: &mut Term, paths: &Paths, yes: bool) -> Result<i32> {
    match protocol::call(paths, &Request::ListRuns { finished: 0 }).and_then(|r| r.into_runs()) {
        Ok(runs) => {
            let running: Vec<_> = runs
                .iter()
                .filter(|info| info.run.state == RunState::Running)
                .collect();
            if !running.is_empty() && !yes {
                term.say("Running Runs keep running but end as `lost`, with no exit code:");
                for info in &running {
                    term.say(&format!("  {}", one_line(info)));
                }
                if !term.confirm("Restart the background process?")? {
                    term.say("Cancelled; the background process was not restarted.");
                    return Ok(0);
                }
            }
        }
        // An unreachable or mismatched background process is why one restarts.
        Err(err) => term.say_err(&format!("note: {}", text::one_line(&format!("{err:#}")))),
    }

    let exe = install::restart_service()?;
    // The new process answers once it has the lock and the socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    let answered = loop {
        match protocol::call_within(
            paths,
            &Request::ListRuns { finished: 0 },
            Duration::from_secs(2),
        ) {
            Ok(_) => break Ok(()),
            Err(err) if Instant::now() >= deadline => break Err(err),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    answered
        .with_context(|| format!("the restarted background process ({exe}) is not answering"))?;
    term.say(&format!("Restarted the background process from {exe}."));
    let current = std::env::current_exe().and_then(|p| p.canonicalize());
    if current.is_ok_and(|current| current != Path::new(&exe)) {
        term.say("note: that is not this binary; `geleider install` points the service at it.");
    }
    Ok(0)
}

fn render_finished(inventory: &Inventory) -> String {
    let mut out = String::from("Finished worktrees\n");
    if inventory.cleanup_pending {
        out.push_str("  (still being checked; `geleider clean` checks now)\n");
    } else if inventory.finished_worktrees.is_empty() {
        out.push_str("  (none)\n");
    }
    // Worktree paths and branch names are chosen by Sessions and are shown before
    // a confirmation prompt, so each stays on its own line with no escapes.
    for wt in &inventory.finished_worktrees {
        out.push_str(&format!("  {}\n", finished_row(wt)));
    }
    for warning in &inventory.cleanup_warnings {
        out.push_str(&format!("  Could not check {}\n", text::one_line(warning)));
    }
    out
}

fn finished_row(wt: &FinishedWorktree) -> String {
    format!(
        "{}  #{} {}  {}  {}",
        wt.project,
        wt.pr,
        wt.pr_state,
        text::one_line(&wt.branch),
        shown_path(&wt.worktree)
    )
}

fn shown_path(path: &Path) -> String {
    text::one_line(&paths::display(path))
}

/// Claims `issue` for the Session working in this worktree. The CLI names no
/// Session: the daemon takes the only live one in the worktree.
pub fn claim(term: &mut Term, paths: &Paths, issue: &str, json: bool) -> Result<i32> {
    let request = Request::Claim {
        cwd: std::env::current_dir()?,
        issue: IssueRef::parse(issue)?,
        session: None,
    };
    claim_reply(term, paths, &request, json)
}

pub fn release(
    term: &mut Term,
    paths: &Paths,
    issue: &str,
    force: bool,
    json: bool,
) -> Result<i32> {
    let request = Request::Release {
        cwd: std::env::current_dir()?,
        issue: IssueRef::parse(issue)?,
        session: None,
        force,
    };
    claim_reply(term, paths, &request, json)
}

fn claim_reply(term: &mut Term, paths: &Paths, request: &Request, json: bool) -> Result<i32> {
    let (claim, message) = protocol::call(paths, request)?.into_claim()?;
    let value = serde_json::json!({ "claim": claim, "message": message });
    print_as(term, json, &value, || format!("{message}\n"))?;
    Ok(0)
}

pub fn usage(term: &mut Term, paths: &Paths, json: bool) -> Result<i32> {
    let subscriptions = protocol::call(paths, &Request::Usage)?.into_usage()?;
    print_as(term, json, &subscriptions, || {
        render_usage(&subscriptions, now_ms())
    })?;
    Ok(0)
}

/// Prints `value` as JSON for scripts, or `text` for people. Branches, paths,
/// args and logs in `text` come from Sessions, so [`Term`] drops terminal
/// escapes in them; JSON escapes them itself.
fn print_as(
    term: &mut Term,
    json: bool,
    value: &impl Serialize,
    text: impl FnOnce() -> String,
) -> Result<()> {
    if json {
        print_json(value)
    } else {
        term.say(&text());
        Ok(())
    }
}

#[allow(
    clippy::disallowed_macros,
    reason = "`--json` is read by programs, and serde escapes what Sessions wrote"
)]
fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Characters of a Session id `status` shows: enough to tell Sessions apart.
const SHORT_ID_CHARS: usize = 8;
/// From here a Limit window's gauge is yellow.
const BUSY_PERCENT: f64 = 60.0;
/// From here a Limit window's gauge is red.
const NEAR_LIMIT_PERCENT: f64 = 85.0;
/// From this terminal width the panels sit in two columns.
const TWO_COLUMNS: usize = 110;
/// Narrower terminals wrap the panels rather than squeeze them further.
const MIN_WIDTH: usize = 60;

/// `geleider status` as panels: usage across the top, then Sessions beside
/// Resources and Runs and Claims and cleanup, stacked when narrower than
/// [`TWO_COLUMNS`]. Branches, paths, args and process names come from Sessions,
/// so each is put on one line before it is shown.
fn render_status(status: &Status, now: i64, width: usize) -> Vec<Line> {
    let width = width.max(MIN_WIDTH);
    let two = width >= TWO_COLUMNS;
    let left_width = if two { width * 11 / 20 } else { width };
    let right_width = if two { width - left_width - 1 } else { width };

    let mut out = Vec::new();
    if !status.subscriptions.is_empty() {
        out.extend(ui::boxed("Usage", &usage_panel(status, now, two), width));
    }
    let live = live_sessions(status);
    let sessions = ui::boxed(
        &format!("Sessions · {}", live.len()),
        &sessions_panel(status, &live, now),
        left_width,
    );
    let mut right = ui::boxed("Resources & Runs", &runs_panel(status, now), right_width);
    right.extend(ui::boxed(
        "Claims & cleanup",
        &claims_panel(status, &live, now),
        right_width,
    ));
    if two {
        out.extend(ui::beside(&sessions, &right, left_width));
    } else {
        out.extend(sessions);
        out.extend(right);
    }
    out
}

/// Waiting Sessions first, then working, then idle; the most recent first in each.
fn live_sessions(status: &Status) -> Vec<&Session> {
    let mut live: Vec<&Session> = status
        .sessions
        .iter()
        .filter(|s| s.state != SessionState::Ended)
        .collect();
    let rank = |s: &Session| match s.state {
        SessionState::Waiting => 0,
        SessionState::Working => 1,
        _ => 2,
    };
    live.sort_by_key(|s| (rank(s), std::cmp::Reverse(s.last_activity)));
    live
}

/// e.g. `Claude Max    5h ████░░░░░░ 23%   ↺ 14:53`, with a Subscription's windows on
/// one line when `wide`, else one line each.
fn usage_panel(status: &Status, now: i64, wide: bool) -> Vec<Line> {
    let cells = if wide { 16 } else { 10 };
    let plan_width = status
        .subscriptions
        .iter()
        .map(|u| u.plan.chars().count())
        .max()
        .unwrap_or(0);
    let note = |window: &WindowView| {
        let mut note = window
            .resets_at
            .map_or(String::new(), |at| format!("↺ {}", report::clock(at, now)));
        if window.stale {
            note.push_str(&format!(", {} old", report::age(window.age_ms)));
        }
        note
    };
    let note_width = status
        .subscriptions
        .iter()
        .flat_map(|u| &u.windows)
        .map(|w| note(w).chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for usage in &status.subscriptions {
        let mut line = vec![Span::styled(Style::Bold, usage.plan.clone()).padded(plan_width + 2)];
        if usage.windows.is_empty() {
            line.push(Span::styled(Style::Dim, "no data yet"));
        }
        for (i, window) in usage.windows.iter().enumerate() {
            if i > 0 && !wide {
                lines.push(std::mem::replace(
                    &mut line,
                    vec![Span::plain(" ".repeat(plan_width + 2))],
                ));
            }
            line.push(Span::plain(format!("{} ", window.window)));
            line.extend(ui::gauge(
                window.used_percent,
                cells,
                BUSY_PERCENT,
                NEAR_LIMIT_PERCENT,
            ));
            let used = window
                .used_percent
                .map_or("?".into(), |u| format!("{u:.0}%"));
            line.push(Span::plain(format!(" {used:<4} ")));
            line.push(Span::styled(Style::Dim, note(window)).padded(note_width + 3));
        }
        lines.push(line);
    }
    lines
}

/// Two lines a Session: where and on what, then its state, age and id.
fn sessions_panel(status: &Status, live: &[&Session], now: i64) -> Vec<Line> {
    if live.is_empty() {
        return vec![vec![Span::styled(Style::Dim, "(none)")]];
    }
    let subject_width = live
        .iter()
        .map(|s| session_subject(s).chars().count())
        .max()
        .unwrap_or(0);
    let claims: Vec<String> = live.iter().map(|s| claims_of(status, s)).collect();
    let claims_width = claims.iter().map(|c| c.chars().count()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for (session, claims) in live.iter().zip(claims) {
        let style = match session.state {
            SessionState::Waiting => Style::Yellow,
            SessionState::Working => Style::Green,
            _ => Style::Dim,
        };
        let dot = match session.state {
            SessionState::Waiting => "◐",
            SessionState::Working => "●",
            _ => "○",
        };
        let mut first = vec![
            Span::styled(style, format!("{dot} ")),
            Span::shown(&session_subject(session))
                .with(Style::Bold)
                .padded(subject_width + 2),
            Span::plain(session.key.harness.as_str()).padded(10),
        ];
        if claims_width > 0 {
            first.push(Span::styled(Style::Magenta, claims).padded(claims_width + 2));
        }
        first.push(Span::shown(&shown_branch(session.branch.as_deref())));
        lines.push(first);
        let prompts = session.pending_prompts.len();
        let open = if prompts > 1 {
            format!(", {prompts} prompts open")
        } else {
            String::new()
        };
        lines.push(vec![
            Span::plain("  "),
            Span::styled(
                style,
                format!(
                    "{} for {}{open}",
                    session.state.as_str(),
                    report::age(now - session.last_activity)
                ),
            ),
            Span::styled(Style::Dim, format!(" · {}", short_id(&session.key))),
        ]);
    }
    lines
}

/// Each Resource's slots, then the Runs holding or waiting for it and any
/// process holding it that geleider did not start.
fn runs_panel(status: &Status, now: i64) -> Vec<Line> {
    let mut names: Vec<&str> = status.resources.iter().map(|r| r.name.as_str()).collect();
    // A Run can outlive its Resource's config.
    for info in &status.runs {
        if !names.contains(&info.run.resource.as_str()) {
            names.push(&info.run.resource);
        }
    }
    if names.is_empty() {
        return vec![vec![Span::styled(Style::Dim, "(no Resources configured)")]];
    }
    let name_width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    let titles: Vec<String> = status
        .runs
        .iter()
        .map(|r| text::one_line(&report::title(&r.run)))
        .collect();
    let title_width = titles.iter().map(|t| t.chars().count()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for name in names {
        let mut line = vec![Span::styled(Style::Bold, name).padded(name_width + 2)];
        if let Some(resource) = status.resources.iter().find(|r| r.name == name) {
            let used = resource.holders.len();
            let slots = resource.slots as usize;
            let style = if used >= slots {
                Style::Yellow
            } else {
                Style::Green
            };
            line.push(Span::styled(
                style,
                "■".repeat(used.min(slots)) + &"□".repeat(slots.saturating_sub(used)),
            ));
            line.push(Span::styled(Style::Dim, format!(" {used}/{slots}")));
        }
        lines.push(line);
        for (info, title) in status.runs.iter().zip(&titles) {
            let run = &info.run;
            if run.resource != name {
                continue;
            }
            let (mark, style, time) = match run.state {
                RunState::Running => (
                    "▶",
                    Style::Cyan,
                    report::duration(run.started_at.map_or(0, |at| now - at)),
                ),
                _ => (
                    "⋯",
                    Style::Blue,
                    match info.queue_position {
                        Some(1) => "next".into(),
                        Some(n) => format!("{n} in line"),
                        None => "queued".into(),
                    },
                ),
            };
            lines.push(vec![
                Span::styled(style, format!("  {mark} ")),
                Span::styled(Style::Dim, format!("#{}", run.id)).padded(6),
                Span::plain(title.clone()).padded(title_width + 2),
                Span::styled(style, time).padded(11),
                Span::styled(Style::Dim, run.project.clone()),
            ]);
        }
        for holder in status
            .resources
            .iter()
            .filter(|r| r.name == name)
            .flat_map(|r| &r.holders)
        {
            if let Holder::Process { pid, name } = holder {
                lines.push(vec![Span::styled(
                    Style::Yellow,
                    format!(
                        "  ◆ {} pid {pid}, not started by geleider",
                        text::one_line(name)
                    ),
                )]);
            }
        }
    }
    lines
}

/// Claims, quiet ones with how to release them, then Finished worktrees.
fn claims_panel(status: &Status, live: &[&Session], now: i64) -> Vec<Line> {
    let mut lines = Vec::new();
    let names: Vec<String> = status
        .claims
        .iter()
        .map(|c| format!("{}#{}", c.claim.project, c.claim.issue))
        .collect();
    let finished = &status.finished;
    let worktrees: Vec<String> = finished
        .finished_worktrees
        .iter()
        .map(|wt| format!("{} #{} {}", wt.project, wt.pr, wt.pr_state))
        .collect();
    let name_width = names
        .iter()
        .chain(&worktrees)
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0);
    for (claim, name) in status.claims.iter().zip(names) {
        let holder = &claim.claim.session;
        let ended = claim.session_state == Some(SessionState::Ended)
            || !live.iter().any(|s| &s.key == holder);
        let age = report::age(now - claim.last_activity);
        let (mark, activity) = match (claim.quiet, ended) {
            (true, true) => (
                Span::styled(Style::Red, "! "),
                Span::styled(Style::Red, format!("quiet {age}, Session ended")),
            ),
            (true, false) => (
                Span::styled(Style::Red, "! "),
                Span::styled(Style::Red, format!("quiet {age}")),
            ),
            (false, true) => (
                Span::plain("  "),
                Span::styled(Style::Dim, format!("Session ended {age} ago")),
            ),
            (false, false) => (
                Span::plain("  "),
                Span::styled(Style::Dim, format!("{age} ago")),
            ),
        };
        lines.push(vec![
            mark,
            Span::styled(Style::Magenta, name).padded(name_width + 2),
            Span::plain(who(holder)).padded(SHORT_ID_CHARS + 10),
            activity,
        ]);
        if claim.quiet {
            lines.push(vec![
                Span::plain("  "),
                Span::styled(
                    Style::Cyan,
                    format!("geleider release {} --force", claim.claim.name()),
                ),
            ]);
        }
    }
    for (wt, label) in finished.finished_worktrees.iter().zip(worktrees) {
        lines.push(vec![
            Span::styled(Style::Blue, "✓ "),
            Span::plain(label).padded(name_width + 2),
            Span::shown(&wt.branch).with(Style::Dim),
        ]);
    }
    if !finished.finished_worktrees.is_empty() {
        lines.push(vec![
            Span::plain("  "),
            Span::styled(Style::Cyan, "geleider clean"),
            Span::styled(
                Style::Dim,
                " lets you choose which Finished worktrees to remove",
            ),
        ]);
    }
    if finished.cleanup_pending {
        lines.push(vec![Span::styled(
            Style::Dim,
            "Finished worktrees still being checked",
        )]);
    }
    for warning in &finished.cleanup_warnings {
        lines.push(vec![Span::styled(
            Style::Yellow,
            format!("! could not check {}", text::one_line(warning)),
        )]);
    }
    if lines.is_empty() {
        lines.push(vec![Span::styled(Style::Dim, "(none)")]);
    }
    lines
}

/// The Project a Session works in, or its worktree outside any Project.
fn session_subject(session: &Session) -> String {
    match &session.project {
        Some(project) => project.clone(),
        None => shown_path(&session.worktree),
    }
}

fn short_id(key: &SessionKey) -> String {
    key.id.chars().take(SHORT_ID_CHARS).collect()
}

/// e.g. `codex 019a2b3c`.
fn who(key: &SessionKey) -> String {
    format!("{} {}", key.harness.as_str(), short_id(key))
}

/// The issues a Session holds: `#42`, or `owner/name#42` outside its Project.
fn claims_of(status: &Status, session: &Session) -> String {
    status
        .claims
        .iter()
        .filter(|c| c.claim.session == session.key)
        .map(|c| {
            if session.project.as_deref() == Some(c.claim.project.as_str()) {
                format!("#{}", c.claim.issue)
            } else {
                c.claim.name()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn shown_branch(branch: Option<&str>) -> String {
    branch.map_or("(detached)".into(), text::one_line)
}

/// Shows a Subscription's Limit windows, including missing ones.
pub fn print_usage(
    term: &mut Term,
    config: &Config,
    usage: &[SubscriptionUsage],
    subscription: Subscription,
) {
    let usage = usage
        .iter()
        .find(|u| u.subscription == subscription)
        .cloned()
        .unwrap_or_else(|| SubscriptionUsage {
            subscription,
            plan: config.subscriptions.plan(subscription),
            source: subscription.source().into(),
            windows: Vec::new(),
        });
    term.say(&render_usage(&[usage], now_ms()));
    if !config.subscriptions.get(subscription).enabled {
        term.say("  Usage tracking is disabled for this Subscription.");
    }
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

/// e.g. `#12 running  playmode-test combat  lanternfall  feat/x  for 3m 12s`.
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

fn print_result(term: &mut Term, info: &RunInfo, json: bool) -> Result<()> {
    print_as(term, json, info, || render_result(info))
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
    use serde_json::{Value, json};

    use crate::github::PrState;

    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;

    fn session(
        harness: &str,
        id: &str,
        project: Option<&str>,
        branch: &str,
        state: &str,
        ago: i64,
    ) -> Value {
        json!({
            "harness": harness, "id": id, "project": project, "worktree": "/tmp/scratch",
            "branch": branch, "state": state, "last_activity": NOW - ago,
        })
    }

    fn claim(
        issue: u64,
        project: &str,
        holder: (&str, &str),
        state: &str,
        active: i64,
        quiet: bool,
    ) -> Value {
        json!({
            "repo": format!("Eloyye/{project}"), "issue": issue, "project": project,
            "session": {"harness": holder.0, "id": holder.1}, "worktree": "/wt", "branch": "b",
            "git_dir": "/wt/.git", "assignee": "Eloyye", "claimed_at": NOW - 2 * HOUR,
            "session_state": state, "last_activity": NOW - active, "quiet": quiet,
        })
    }

    fn run(
        id: i64,
        arg: &str,
        state: &str,
        started: Option<i64>,
        position: Option<usize>,
    ) -> Value {
        json!({
            "id": id, "project": "lanternfall", "repo": "Eloyye/lanternfall", "job": "playmode-test",
            "args": [arg], "resource": "unity", "worktree": "/wt", "branch": "b", "argv": [],
            "submitter": "mcp", "session": null, "state": state, "submitted_at": NOW - 2 * MIN,
            "started_at": started.map(|s| NOW - s), "ended_at": null, "pid": null,
            "pid_started": null, "exit_code": null, "error": null, "log_path": "/log",
            "queue_position": position, "log_tail": null,
        })
    }

    fn window(window: &str, minutes: i64, used: f64, age: i64, stale: bool) -> Value {
        json!({
            "window": window, "minutes": minutes, "used_percent": used, "resets_at": NOW + 40 * MIN,
            "observed_at": NOW - age, "age_ms": age, "stale": stale, "reset_since_observed": false,
        })
    }

    /// Two Projects on a busy afternoon.
    fn busy() -> Status {
        let waiting = ("claude", "7f3a1c2e-9b4d-4e1a-a0c3-5d6e7f8a9b0c");
        let working = ("codex", "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b");
        serde_json::from_value(json!({
            "sessions": [
                session(waiting.0, waiting.1, Some("lanternfall"), "claude/issue-42", "waiting", 4 * MIN),
                session(working.0, working.1, Some("lanternfall"), "codex/issue-38", "working", 20_000),
                session("claude", "5e21d0aa-1f2b", Some("geleider"), "claude/ui", "working", 5_000),
                session("opencode", "ses_01JB7Q2W8N4K6M", Some("geleider"), "feat/colors", "idle", 42 * MIN),
                session("claude", "c0ffee00-1234", None, "main", "idle", 3 * HOUR),
                session("codex", "ended-session", Some("geleider"), "old", "ended", HOUR),
            ],
            "claims": [
                claim(42, "lanternfall", waiting, "waiting", 4 * MIN, false),
                claim(38, "lanternfall", working, "working", 20_000, false),
                claim(18, "geleider", ("codex", "019a0000-dead"), "ended", 27 * HOUR, true),
                claim(7, "geleider", ("codex", "ended-session"), "ended", 2 * HOUR, false),
            ],
            "resources": [
                {"name": "unity", "slots": 2, "process": "Unity", "holders": [
                    {"kind": "run", "id": 57}, {"kind": "process", "pid": 4121, "name": "Unity"},
                ]},
            ],
            "runs": [
                run(57, "combat", "running", Some(3 * MIN + 12_000), None),
                run(58, "save-load", "queued", None, Some(1)),
            ],
            "subscriptions": [
                {"subscription": "claude", "plan": "Claude Max", "source": "s", "windows": [
                    window("5h", 300, 23.0, MIN, false), window("7d", 10_080, 41.0, MIN, false),
                ]},
                {"subscription": "chatgpt", "plan": "ChatGPT Plus", "source": "s", "windows": [
                    window("5h", 300, 87.0, MIN, false), window("7d", 10_080, 62.0, 25 * MIN, true),
                ]},
            ],
            "finished_worktrees": [
                {"project": "lanternfall", "worktree": "/wt/31", "branch": "codex/issue-31",
                 "head": "a1b2c3d", "pr": 33, "pr_state": "MERGED", "pr_url": "https://github.com/x/y/pull/33"},
            ],
        }))
        .unwrap()
    }

    /// What the panels hold, line by line, without borders and with runs of
    /// spaces collapsed, so tests do not depend on column widths.
    fn contents(text: &str) -> Vec<String> {
        text.lines()
            .flat_map(|line| line.split('│').map(str::to_string).collect::<Vec<_>>())
            .map(|cell| cell.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|cell| !cell.is_empty() && !cell.starts_with('╭') && !cell.starts_with('╰'))
            .collect()
    }

    /// `status` as `geleider status` shows it on a `width`-column terminal.
    fn shown(status: &Status, width: usize, term: Term) -> String {
        let mut term = term;
        term.show(&render_status(status, NOW, width));
        term.stdout().to_string()
    }

    fn plain(status: &Status, width: usize) -> String {
        shown(status, width, Term::scripted([], width))
    }

    fn titles(text: &str) -> Vec<String> {
        text.split("╭─ ")
            .skip(1)
            .map(|rest| rest.split(" ─").next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn status_panels_stack_on_narrow_terminals() {
        let text = plain(&busy(), 80);
        for line in text.lines() {
            assert_eq!(line.chars().count(), 80, "{line:?} in\n{text}");
        }
        assert_eq!(
            titles(&text),
            [
                "Usage",
                "Sessions · 5",
                "Resources & Runs",
                "Claims & cleanup"
            ]
        );
        let resets = report::clock(NOW + 40 * MIN, NOW);
        let expected = [
            format!("Claude Max 5h ██░░░░░░░░ 23% ↺ {resets}"),
            format!("7d ████░░░░░░ 41% ↺ {resets}"),
            format!("ChatGPT Plus 5h █████████░ 87% ↺ {resets}"),
            format!("7d ██████░░░░ 62% ↺ {resets}, 25m old"),
            "◐ lanternfall claude #42 claude/issue-42".into(),
            "waiting for 4m · 7f3a1c2e".into(),
            "● geleider claude claude/ui".into(),
            "working for 5s · 5e21d0aa".into(),
            "● lanternfall codex #38 codex/issue-38".into(),
            "working for 20s · 019a2b3c".into(),
            "○ geleider opencode feat/colors".into(),
            "idle for 42m · ses_01JB".into(),
            "○ /tmp/scratch claude main".into(),
            "idle for 3h · c0ffee00".into(),
            "unity ■■ 2/2".into(),
            "▶ #57 playmode-test combat 3m 12s lanternfall".into(),
            "⋯ #58 playmode-test save-load next lanternfall".into(),
            "◆ Unity pid 4121, not started by geleider".into(),
            "lanternfall#42 claude 7f3a1c2e 4m ago".into(),
            "lanternfall#38 codex 019a2b3c 20s ago".into(),
            "! geleider#18 codex 019a0000 quiet 1d, Session ended".into(),
            "geleider release Eloyye/geleider#18 --force".into(),
            "geleider#7 codex ended-se Session ended 2h ago".into(),
            "✓ lanternfall #33 merged codex/issue-31".into(),
            "geleider clean lets you choose which Finished worktrees to remove".into(),
        ];
        assert_eq!(contents(&text), expected, "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
    }

    #[test]
    fn status_panels_sit_side_by_side_on_wide_terminals() {
        let text = plain(&busy(), 120);
        for line in text.lines() {
            assert!(line.chars().count() <= 120, "{line:?} in\n{text}");
        }
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[1].contains("5h ") && lines[1].contains("7d "),
            "{text}"
        );
        assert!(
            lines[4].starts_with("╭─ Sessions · 5 ") && lines[4].contains("╭─ Resources & Runs "),
            "{text}"
        );
        assert!(
            text.contains("│ ◐ lanternfall   claude    #42  claude/issue-42"),
            "{text}"
        );
    }

    #[test]
    fn quiet_status_has_empty_panels() {
        let status: Status = serde_json::from_value(json!({"resources": [], "runs": []})).unwrap();
        let text = plain(&status, 20);
        assert_eq!(
            titles(&text),
            ["Sessions · 0", "Resources & Runs", "Claims & cleanup"]
        );
        assert_eq!(
            contents(&text),
            ["(none)", "(no Resources configured)", "(none)"]
        );
        // Too narrow a terminal gets the narrowest panels rather than broken ones.
        assert!(
            text.lines().all(|l| l.chars().count() == MIN_WIDTH),
            "{text}"
        );
    }

    #[test]
    fn status_colours_only_its_own_words() {
        let mut status = busy();
        status.sessions[0].branch = Some("x\u{1b}]52;c;aGk=\u{7}\u{1b}[8mhidden\nfake line".into());
        let shown = shown(&status, 80, Term::scripted([], 80).colored());
        assert!(shown.contains("\u{1b}[33m◐\u{1b}[0m"), "{shown}");
        assert!(
            !shown.contains("\u{1b}]") && !shown.contains("\u{1b}[8m"),
            "{shown}"
        );
        assert!(
            shown.contains("x ]52;c;aGk=  [8mhidden fake line"),
            "{shown}"
        );
    }

    fn finished(repo: &str, pr: u64, path: &str) -> FinishedWorktree {
        FinishedWorktree {
            project: repo.rsplit('/').next().unwrap().into(),
            worktree: path.into(),
            branch: format!("issue-{pr}"),
            head: "abc".into(),
            pr,
            pr_state: PrState::Merged,
            pr_url: format!("https://github.com/{repo}/pull/{pr}"),
        }
    }

    fn targets(finished: &[FinishedWorktree], args: &[&str]) -> Result<Vec<PathBuf>> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        Ok(resolve_targets(finished, &args, Path::new("/work"))?
            .into_iter()
            .map(|w| w.worktree)
            .collect())
    }

    #[test]
    fn a_clean_target_names_a_finished_worktree_by_its_path() {
        let all = [
            finished("me/a", 3, "/work/a-3"),
            finished("me/b", 4, "/work/b-4"),
        ];
        assert_eq!(
            targets(&all, &["/work/b-4"]).unwrap(),
            [PathBuf::from("/work/b-4")]
        );
        assert_eq!(
            targets(&all, &["a-3/"]).unwrap(),
            [PathBuf::from("/work/a-3")]
        );
        let err = targets(&all, &["/work/c-5"]).unwrap_err().to_string();
        assert!(
            err.contains("/work/c-5") && err.contains("not a Finished worktree"),
            "{err}"
        );
    }

    #[test]
    fn a_clean_target_names_a_finished_worktree_by_its_pr_unless_that_is_ambiguous() {
        let all = [
            finished("me/a", 3, "/work/a-3"),
            finished("me/b", 3, "/work/b-3"),
            finished("me/b", 4, "/work/b-4"),
        ];
        assert_eq!(
            targets(&all, &["4", "#4", "me/b#4"]).unwrap(),
            [PathBuf::from("/work/b-4")]
        );
        assert_eq!(
            targets(&all, &["Me/A#3"]).unwrap(),
            [PathBuf::from("/work/a-3")]
        );
        let err = targets(&all, &["#3"]).unwrap_err().to_string();
        assert!(
            err.contains("#3") && err.contains("me/a#3") && err.contains("me/b#3"),
            "{err}"
        );
        let err = targets(&all, &["me/a#4"]).unwrap_err().to_string();
        assert!(err.contains("not a Finished worktree"), "{err}");
    }

    #[test]
    fn clean_asks_about_exactly_the_checked_worktrees() {
        assert_eq!(removal_question(0), None);
        assert_eq!(
            removal_question(1).as_deref(),
            Some("Remove this worktree?")
        );
        assert_eq!(
            removal_question(3).as_deref(),
            Some("Remove these 3 worktrees?")
        );
    }
}
