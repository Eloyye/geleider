//! `geleider statusline`: Claude Code's status-line command. It forwards the
//! session's `rate_limits` to the background process, then prints either the
//! status line it wraps, unchanged, or a line of its own.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::paths::Paths;
use crate::protocol::{self, Request};
use crate::report;
use crate::store::now_ms;
use crate::usage::{self, Reading, Subscription};

/// Claude Code waits on the status line, so geleider never holds it up for long.
const REPORT_TIMEOUT: Duration = Duration::from_millis(500);

pub fn run(paths: &Paths, wrap: Option<&str>) -> Result<i32> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let json: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    let readings = usage::claude_readings(&json);
    if !readings.is_empty() {
        // Without the background process the status line still shows.
        let _ = protocol::call_within(
            paths,
            &Request::ReportUsage {
                subscription: Subscription::Claude,
                readings: readings.clone(),
            },
            REPORT_TIMEOUT,
        );
    }
    match wrap {
        Some(command) => run_wrapped(command, &input),
        None => {
            println!("{}", own_line(&json, &readings, now_ms()));
            Ok(0)
        }
    }
}

/// Runs the user's own status line with the same input, through a shell as Claude
/// Code would, so its output and exit code are what Claude Code gets.
fn run_wrapped(command: &str, input: &[u8]) -> Result<i32> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .spawn()
        .context("starting the wrapped status line")?;
    if let Some(mut stdin) = child.stdin.take() {
        // A script that does not read its input closes the pipe early.
        let _ = stdin.write_all(input);
    }
    Ok(child.wait()?.code().unwrap_or(1))
}

/// e.g. `Opus · ctx 23% · 5h 23% until 14:05 · 7d 41% until Fri 09:00`.
fn own_line(json: &Value, readings: &[Reading], now: i64) -> String {
    let mut parts = Vec::new();
    if let Some(model) = json["model"]["display_name"].as_str() {
        parts.push(model.to_string());
    }
    if let Some(context) = json["context_window"]["used_percentage"].as_f64() {
        parts.push(format!("ctx {context:.0}%"));
    }
    for reading in readings {
        let mut part = format!(
            "{} {:.0}%",
            usage::short_label(reading.minutes),
            reading.used_percent
        );
        if let Some(at) = reading.resets_at {
            part.push_str(&format!(" until {}", report::clock(at, now)));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        parts.push("geleider".into());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn own_line_shows_model_context_and_limits() {
        let json = json!({
            "model": { "display_name": "Opus" },
            "context_window": { "used_percentage": 23.4 },
            "rate_limits": { "five_hour": { "used_percentage": 23.5 }, "seven_day": { "used_percentage": 41.2 } }
        });
        let line = own_line(&json, &usage::claude_readings(&json), 0);
        assert_eq!(line, "Opus · ctx 23% · 5h 24% · 7d 41%");
        assert_eq!(own_line(&Value::Null, &[], 0), "geleider");
    }

    #[test]
    fn wrapped_status_line_gets_the_same_input() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seen");
        let command = format!("cat > '{}'; exit 4", out.display());
        assert_eq!(run_wrapped(&command, b"{\"a\":1}").unwrap(), 4);
        assert_eq!(std::fs::read_to_string(out).unwrap(), "{\"a\":1}");
    }
}
