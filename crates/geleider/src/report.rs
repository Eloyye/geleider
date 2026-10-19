//! Telling people about Runs: log tails, PR comment text and macOS notifications.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

use crate::github;
use crate::store::{Run, RunState};
use crate::text;

/// Lines of log shown in results and PR comments.
pub const TAIL_LINES: usize = 40;

/// The last `lines` lines of a log file, reading at most its last 64 KiB.
pub fn log_tail(path: &Path, lines: usize) -> String {
    const MAX: u64 = 64 * 1024;
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(MAX);
    if file.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = text.lines().collect();
    if from > 0 && !all.is_empty() {
        all.remove(0); // probably cut mid-line
    }
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// How long ago, in its largest unit only: `40s`, `4m`, `2h`, `3d`.
pub fn age(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// A local time such as `14:05`, with the weekday when it is not today and the
/// date when it is not within a week: `Fri 09:00`, `Oct 21 09:00`.
pub fn clock(ms: i64, now: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (Some(at), Some(today)) = (local_time(ms), local_time(now)) else {
        let secs = ms.div_euclid(1000).rem_euclid(86_400);
        return format!("{:02}:{:02} UTC", secs / 3600, secs % 3600 / 60);
    };
    let time = format!("{:02}:{:02}", at.tm_hour, at.tm_min);
    if (at.tm_year, at.tm_yday) == (today.tm_year, today.tm_yday) {
        time
    } else if (ms - now).abs() < 6 * 86_400_000 {
        format!("{} {time}", DAYS[at.tm_wday as usize % 7])
    } else {
        format!("{} {} {time}", MONTHS[at.tm_mon as usize % 12], at.tm_mday)
    }
}

/// The local calendar time at `ms`, or `None` if the C library cannot represent
/// it. The standard library has no time zones, and pulling in a date crate for
/// one `localtime_r` call is not worth it.
fn local_time(ms: i64) -> Option<libc::tm> {
    let secs = libc::time_t::try_from(ms.div_euclid(1000)).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for the call; localtime_r fills `tm` and
    // returns it, or returns null and leaves it unspecified.
    let filled = unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) };
    // SAFETY: a non-null return means `tm` was fully written.
    (!filled.is_null()).then(|| unsafe { tm.assume_init() })
}

/// e.g. `playmode-test combat`.
pub fn title(run: &Run) -> String {
    std::iter::once(run.job.as_str())
        .chain(run.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// e.g. `passed`, `failed (exit 3)`.
pub fn outcome(run: &Run) -> String {
    match (run.state, run.exit_code) {
        (RunState::Succeeded, _) => "passed".into(),
        (RunState::Failed, Some(code)) => format!("failed (exit {code})"),
        (RunState::Failed, None) => "failed".into(),
        (RunState::Lost, _) => "lost (geleider restarted while it ran)".into(),
        (state, _) => state.to_string(),
    }
}

pub fn comment_body(run: &Run, tail: &str) -> String {
    let icon = match run.state {
        RunState::Succeeded => "✅",
        RunState::Failed => "❌",
        _ => "⚠️",
    };
    // The args, the error and the log come from the Session, and this comment is
    // posted as the user: none of it may become links, mentions or HTML.
    let mut body = format!(
        "{}\n### {icon} {} {}\n\nRun #{} · {} · exit code {}",
        github::marker(&run.job),
        text::code_span(&title(run)),
        outcome(run),
        run.id,
        duration(run.duration_ms().unwrap_or(0)),
        run.exit_code.map_or("unknown".into(), |c| c.to_string()),
    );
    if let Some(error) = &run.error {
        body.push_str(&format!("\n\n> {}", text::code_span(error)));
    }
    if !tail.is_empty() {
        let fence = text::code_fence(tail);
        body.push_str(&format!(
            "\n\n<details><summary>Last {TAIL_LINES} lines of the log</summary>\n\n{fence}text\n{tail}\n{fence}\n\n</details>"
        ));
    }
    body.push_str(&format!(
        "\n\n<sub>Posted by geleider; edited in place on later runs. Full log: `{}`</sub>",
        run.log_path.display()
    ));
    body
}

pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, message: &str);
}

/// macOS Notification Center, through `osascript`.
pub struct MacNotifier;

impl Notifier for MacNotifier {
    #[allow(
        clippy::disallowed_macros,
        reason = "the background process notifies; it logs to stderr"
    )]
    fn notify(&self, title: &str, message: &str) {
        let script = format!(
            "display notification {} with title {}",
            applescript_string(message),
            applescript_string(title)
        );
        if let Err(err) = Command::new("osascript").args(["-e", &script]).output() {
            eprintln!("geleider: notification failed: {err}");
        }
    }
}

fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_takes_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let text: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let tail = log_tail(&path, 3);
        assert_eq!(tail, "line 98\nline 99\nline 100");
        assert_eq!(log_tail(&dir.path().join("missing"), 3), "");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(4_200), "4s");
        assert_eq!(duration(192_000), "3m 12s");
        assert_eq!(duration(3_723_000), "1h 02m");
        assert_eq!(duration(273_600_000), "3d 04h");
    }

    #[test]
    fn ages_show_one_unit() {
        assert_eq!(age(4_200), "4s");
        assert_eq!(age(192_000), "3m");
        assert_eq!(age(3_723_000), "1h");
        assert_eq!(age(273_600_000), "3d");
        assert_eq!(age(-5), "0s");
    }

    #[test]
    fn clock_names_the_day_only_when_needed() {
        let now = crate::store::now_ms();
        assert_eq!(clock(now, now).len(), "14:05".len());
        assert_eq!(clock(now + 3 * 86_400_000, now).len(), "Fri 14:05".len());
        assert_eq!(clock(now + 20 * 86_400_000, now).split(' ').count(), 3);
    }

    #[test]
    fn comments_show_args_and_errors_as_code_only() {
        let run = Run {
            id: 1,
            project: "p".into(),
            repo: "o/r".into(),
            job: "playmode-test".into(),
            args: vec!["x` [click](https://e.vil) @org/team `".into()],
            resource: "unity".into(),
            worktree: "/wt".into(),
            branch: None,
            argv: Vec::new(),
            submitter: "cli".into(),
            session: None,
            state: RunState::Failed,
            submitted_at: 0,
            started_at: Some(0),
            ended_at: Some(0),
            pid: None,
            pid_started: None,
            exit_code: None,
            error: Some("starting x\n\n<img src=x>".into()),
            log_path: "/log".into(),
        };
        let body = comment_body(&run, "");
        assert!(
            body.contains(
                "### ❌ `` playmode-test x` [click](https://e.vil) @org/team ` `` failed"
            ),
            "{body}"
        );
        assert!(body.contains("> `starting x  <img src=x>`"), "{body}");
    }

    #[test]
    fn applescript_strings_are_escaped() {
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
