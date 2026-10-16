//! Telling people about Runs: log tails, PR comment text and macOS notifications.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

use crate::github;
use crate::store::{Run, RunState};

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
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// e.g. `ck3-test events`.
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
    let mut body = format!(
        "{}\n### {icon} `{}` {}\n\nRun #{} · {} · exit code {}",
        github::marker(&run.job),
        title(run),
        outcome(run),
        run.id,
        duration(run.duration_ms().unwrap_or(0)),
        run.exit_code.map_or("unknown".into(), |c| c.to_string()),
    );
    if let Some(error) = &run.error {
        body.push_str(&format!("\n\n> {error}"));
    }
    if !tail.is_empty() {
        // A fence longer than any run of backticks in the log keeps it intact.
        let longest = tail.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(longest.max(2) + 1);
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
    }

    #[test]
    fn applescript_strings_are_escaped() {
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
