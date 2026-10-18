//! How much of each Subscription is used in each Limit window. The numbers come
//! only from what each Harness itself reports: Claude Code's status-line input and
//! the `rate_limits` entries in Codex's session logs. Nothing here calls a usage
//! endpoint, reads a login token or estimates from token counts.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths;

/// Data older than this is shown as last known, not as current.
pub const STALE_AFTER_MS: i64 = 10 * 60 * 1000;
/// A reset noticed later than this after it happened, say after the background
/// process was down, passes without a notification.
const LATE_RESET_MS: i64 = 60 * 60 * 1000;
/// Reset times this close together belong to the same window; Codex's can drift by
/// a second between readings.
const SAME_RESET_MS: i64 = 60 * 1000;

/// The Subscriptions geleider can read usage for. OpenCode Go exposes no
/// documented usage source, so it is not one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subscription {
    /// Claude Pro or Max, as used by Claude Code.
    Claude,
    /// ChatGPT Plus, shared by Codex and OpenCode's OpenAI login.
    Chatgpt,
}

impl Subscription {
    pub const ALL: [Subscription; 2] = [Subscription::Claude, Subscription::Chatgpt];

    pub fn as_str(self) -> &'static str {
        match self {
            Subscription::Claude => "claude",
            Subscription::Chatgpt => "chatgpt",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "claude" => Subscription::Claude,
            "chatgpt" => Subscription::Chatgpt,
            other => bail!("unknown subscription `{other}`"),
        })
    }

    pub fn default_plan(self) -> &'static str {
        match self {
            Subscription::Claude => "Claude Pro",
            Subscription::Chatgpt => "ChatGPT Plus",
        }
    }

    /// What to do when there are no numbers yet.
    pub fn no_data_hint(self) -> &'static str {
        match self {
            Subscription::Claude => {
                "no data yet: run a Claude Code CLI session (the desktop app does not run status lines)"
            }
            Subscription::Chatgpt => "no data yet: run a Codex session",
        }
    }

    /// Where the numbers come from, and what has to run for them to update.
    pub fn source(self) -> &'static str {
        match self {
            Subscription::Claude => "from Claude Code's status line; CLI sessions only",
            Subscription::Chatgpt => "from Codex's session logs",
        }
    }
}

/// One Limit window as a Harness reported it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// Length of the window: 300 for 5 hours, 10080 for 7 days.
    pub minutes: i64,
    pub used_percent: f64,
    /// Unix time in milliseconds.
    pub resets_at: Option<i64>,
}

/// The latest known state of one Limit window, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub subscription: Subscription,
    pub minutes: i64,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
    /// When the Harness reported it, in Unix milliseconds.
    pub observed_at: i64,
    /// The reset at `resets_at` has been notified, or passed without needing to be.
    pub reset_handled: bool,
}

/// Folds a new reading into what is stored for its window. Several Claude Code
/// sessions report at once, and an idle one can re-run its status line with
/// numbers from hours ago, so a reading never moves a window backwards: within one
/// window the used percentage only grows, and a reading for an earlier window is
/// dropped. Returns `None` when nothing changes.
pub fn merge(
    old: Option<&Window>,
    subscription: Subscription,
    reading: Reading,
    observed_at: i64,
) -> Option<Window> {
    let fresh = Window {
        subscription,
        minutes: reading.minutes,
        used_percent: reading.used_percent,
        resets_at: reading.resets_at,
        observed_at,
        reset_handled: false,
    };
    let Some(old) = old else { return Some(fresh) };
    let same_window = match (old.resets_at, reading.resets_at) {
        (Some(a), Some(b)) if (a - b).abs() <= SAME_RESET_MS => true,
        (Some(a), Some(b)) if b < a => return None, // an earlier window
        (Some(_), Some(_)) => false,
        (None, None) => true,
        _ => false,
    };
    if !same_window {
        return Some(fresh);
    }
    if reading.used_percent < old.used_percent || observed_at < old.observed_at {
        return None;
    }
    let merged = Window {
        reset_handled: old.reset_handled,
        ..fresh
    };
    (merged != *old).then_some(merged)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResetCheck {
    /// Not reset yet, or already handled.
    Pending,
    Notify,
    /// Reset, but too little used or noticed too late to be worth a notification.
    Quiet,
}

/// Whether the window's reset has just happened and is worth telling the user
/// about: only windows that reached `threshold` percent, so a barely used 5-hour
/// window does not notify every 5 hours.
pub fn check_reset(window: &Window, now: i64, threshold: Option<f64>) -> ResetCheck {
    let Some(resets_at) = window.resets_at else {
        return ResetCheck::Pending;
    };
    if window.reset_handled || resets_at > now {
        return ResetCheck::Pending;
    }
    match threshold {
        Some(t) if window.used_percent >= t && now - resets_at < LATE_RESET_MS => {
            ResetCheck::Notify
        }
        _ => ResetCheck::Quiet,
    }
}

/// One Subscription's Limit windows, as `geleider status` and the MCP server show
/// them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionUsage {
    pub subscription: Subscription,
    pub plan: String,
    pub source: String,
    /// Shortest window first. Empty until the Harness has reported anything.
    pub windows: Vec<WindowView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowView {
    /// e.g. `5h`, `7d`.
    pub window: String,
    pub minutes: i64,
    /// `None` once the window has reset with no reading since, since the new
    /// window's usage is unknown.
    pub used_percent: Option<f64>,
    /// Unix time in milliseconds.
    pub resets_at: Option<i64>,
    /// When the Harness reported these numbers, in Unix milliseconds.
    pub observed_at: i64,
    pub age_ms: i64,
    /// Older than ten minutes, or out of date because the window has reset: last
    /// known, not current.
    pub stale: bool,
    pub reset_since_observed: bool,
}

pub fn view(window: &Window, now: i64) -> WindowView {
    let reset = window.resets_at.is_some_and(|r| r <= now);
    let age_ms = (now - window.observed_at).max(0);
    WindowView {
        window: short_label(window.minutes),
        minutes: window.minutes,
        used_percent: (!reset).then_some(window.used_percent),
        resets_at: window.resets_at,
        observed_at: window.observed_at,
        age_ms,
        stale: reset || age_ms > STALE_AFTER_MS,
        reset_since_observed: reset,
    }
}

/// e.g. `5h`, `7d`.
pub fn short_label(minutes: i64) -> String {
    match minutes {
        m if m > 0 && m % 1440 == 0 => format!("{}d", m / 1440),
        m if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// e.g. `5-hour`, `7-day`.
pub fn long_label(minutes: i64) -> String {
    match minutes {
        m if m > 0 && m % 1440 == 0 => format!("{}-day", m / 1440),
        m if m > 0 && m % 60 == 0 => format!("{}-hour", m / 60),
        m => format!("{m}-minute"),
    }
}

// Claude Code: the JSON its status line receives on stdin.

/// `rate_limits.five_hour` and `rate_limits.seven_day`. Each is absent until the
/// session's first API response, for plans without them, and once it resets.
pub fn claude_readings(input: &Value) -> Vec<Reading> {
    [("five_hour", 300), ("seven_day", 10_080)]
        .into_iter()
        .filter_map(|(key, minutes)| {
            let window = &input["rate_limits"][key];
            Some(Reading {
                minutes,
                used_percent: window["used_percentage"].as_f64()?,
                resets_at: window["resets_at"].as_i64().map(|s| s * 1000),
            })
        })
        .collect()
}

// Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, one JSON item per line.
// After every turn Codex writes an `event_msg` whose payload is a `token_count`
// carrying the `rate_limits` the server sent with that response.

/// Bytes read from the end of a session log before falling back to the whole file.
const TAIL_BYTES: u64 = 1024 * 1024;
/// Day folders and session logs looked at, newest first. Eight days cover the
/// 7-day window.
const DAYS: usize = 8;
const LOGS: usize = 5;

pub fn codex_sessions_dir() -> Result<PathBuf> {
    Ok(codex_home()?.join("sessions"))
}

/// `$CODEX_HOME`, or `~/.codex` when it is unset or empty.
pub fn codex_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => paths::home_dir()?.join(".codex"),
    })
}

/// The newest `rate_limits` entry in Codex's session logs and when it was written.
pub fn latest_codex(sessions: &Path) -> Option<(Vec<Reading>, i64)> {
    newest_logs(sessions)
        .into_iter()
        .take(LOGS)
        .find_map(|log| latest_in_log(&log))
}

/// Session logs in the newest day folders, most recently written first.
fn newest_logs(sessions: &Path) -> Vec<PathBuf> {
    fn children(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        out.sort();
        out.reverse();
        out
    }
    let mut days = Vec::new();
    'years: for year in children(sessions).into_iter().filter(|p| p.is_dir()) {
        for month in children(&year).into_iter().filter(|p| p.is_dir()) {
            for day in children(&month).into_iter().filter(|p| p.is_dir()) {
                days.push(day);
                if days.len() == DAYS {
                    break 'years;
                }
            }
        }
    }
    // Older versions of Codex wrote logs straight into `sessions/`.
    days.push(sessions.to_path_buf());

    let mut logs: Vec<(SystemTime, PathBuf)> = days
        .iter()
        .flat_map(|day| children(day))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        })
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    logs.sort();
    logs.into_iter().rev().map(|(_, p)| p).collect()
}

fn latest_in_log(path: &Path) -> Option<(Vec<Reading>, i64)> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if from > 0 && !lines.is_empty() {
        lines.remove(0); // probably cut mid-line
    }
    if let Some(found) = lines.iter().rev().find_map(|l| codex_line(l)) {
        return Some(found);
    }
    if from == 0 {
        return None;
    }
    // A long turn's output can push the last entry out of the tail.
    let file = File::open(path).ok()?;
    BufReader::new(file)
        .lines()
        .map_while(|l| l.ok())
        .filter_map(|l| codex_line(&l))
        .last()
}

/// The readings in one session-log line, if it is a `token_count` event with the
/// main Codex limits. Other limit ids are per-model limits, not the Subscription's.
fn codex_line(line: &str) -> Option<(Vec<Reading>, i64)> {
    if !line.contains("\"rate_limits\"") {
        return None;
    }
    let item: Value = serde_json::from_str(line).ok()?;
    let payload = &item["payload"];
    if item["type"] != "event_msg" || payload["type"] != "token_count" {
        return None;
    }
    let limits = &payload["rate_limits"];
    if !limits.is_object() || !matches!(limits["limit_id"].as_str(), None | Some("codex")) {
        return None;
    }
    let observed_at = item["timestamp"].as_str().and_then(parse_timestamp)?;
    let readings: Vec<Reading> = [("primary", 300), ("secondary", 10_080)]
        .into_iter()
        .filter_map(|(key, default_minutes)| {
            let window = &limits[key];
            let resets_at = match window["resets_at"].as_i64() {
                Some(secs) => Some(secs * 1000),
                // Older versions of Codex wrote the time left instead.
                None => window["resets_in_seconds"]
                    .as_i64()
                    .map(|secs| observed_at + secs * 1000),
            };
            Some(Reading {
                minutes: window["window_minutes"].as_i64().unwrap_or(default_minutes),
                used_percent: window["used_percent"].as_f64()?,
                resets_at,
            })
        })
        .collect();
    (!readings.is_empty()).then_some((readings, observed_at))
}

/// An RFC 3339 time such as `2026-10-16T09:12:03.512Z`, in Unix milliseconds.
fn parse_timestamp(text: &str) -> Option<i64> {
    let (date, rest) = text.split_once('T')?;
    let mut ymd = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let (clock, offset_ms) = if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else {
        let at = rest.rfind(['+', '-'])?;
        let (clock, offset) = rest.split_at(at);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (h, m) = offset[1..].split_once(':')?;
        (
            clock,
            sign * (h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?) * 60_000,
        )
    };
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut parts = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (parts.next()??, parts.next()??, parts.next()??);
    let millis = format!("{fraction:0<3}").get(..3)?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + h) * 60 + m) * 60_000 + s * 1000 + millis - offset_ms)
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const HOUR: i64 = 3_600_000;

    fn window(used: f64, resets_at: i64, observed_at: i64) -> Window {
        Window {
            subscription: Subscription::Claude,
            minutes: 300,
            used_percent: used,
            resets_at: Some(resets_at),
            observed_at,
            reset_handled: false,
        }
    }

    fn reading(used: f64, resets_at: i64) -> Reading {
        Reading {
            minutes: 300,
            used_percent: used,
            resets_at: Some(resets_at),
        }
    }

    #[test]
    fn reads_claude_status_line_input() {
        let input = json!({
            "model": { "display_name": "Opus" },
            "rate_limits": {
                "five_hour": { "used_percentage": 23.5, "resets_at": 1738425600 },
                "seven_day": { "used_percentage": 41.2, "resets_at": 1738857600 }
            }
        });
        assert_eq!(
            claude_readings(&input),
            vec![
                Reading {
                    minutes: 300,
                    used_percent: 23.5,
                    resets_at: Some(1_738_425_600_000)
                },
                Reading {
                    minutes: 10_080,
                    used_percent: 41.2,
                    resets_at: Some(1_738_857_600_000)
                },
            ]
        );
        // Before the first API response, or on a plan without limits.
        assert!(claude_readings(&json!({ "model": {} })).is_empty());
    }

    #[test]
    fn reads_the_newest_main_codex_limits() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/10/16");
        std::fs::create_dir_all(&day).unwrap();
        let line = |ts: &str, limit_id: &str, used: f64| {
            json!({
                "timestamp": ts,
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": null,
                    "rate_limits": {
                        "limit_id": limit_id,
                        "primary": { "used_percent": used, "window_minutes": 300, "resets_at": 1_760_620_000 },
                        "secondary": { "used_percent": 7.0, "window_minutes": 10080, "resets_at": 1_761_000_000 }
                    }
                }
            })
            .to_string()
        };
        let log = [
            json!({"timestamp": "2026-10-16T09:00:00.000Z", "type": "session_meta", "payload": {}})
                .to_string(),
            line("2026-10-16T09:10:00.000Z", "codex", 10.0),
            line("2026-10-16T09:12:03.512Z", "codex", 12.0),
            // A per-model limit, not the Subscription's.
            line("2026-10-16T09:13:00.000Z", "codex_other", 90.0),
            json!({"timestamp": "2026-10-16T09:14:00.000Z", "type": "response_item", "payload": {"type": "message"}})
                .to_string(),
        ]
        .join("\n");
        std::fs::write(day.join("rollout-2026-10-16T09-00-00-abc.jsonl"), log).unwrap();
        std::fs::write(day.join("notes.txt"), "rate_limits").unwrap();

        let (readings, observed_at) = latest_codex(dir.path()).unwrap();
        assert_eq!(
            observed_at,
            parse_timestamp("2026-10-16T09:12:03.512Z").unwrap()
        );
        assert_eq!(
            readings,
            vec![
                Reading {
                    minutes: 300,
                    used_percent: 12.0,
                    resets_at: Some(1_760_620_000_000)
                },
                Reading {
                    minutes: 10_080,
                    used_percent: 7.0,
                    resets_at: Some(1_761_000_000_000)
                },
            ]
        );
        assert!(latest_codex(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn older_codex_logs_give_time_left() {
        let line = json!({
            "timestamp": "2025-09-01T10:00:00Z",
            "type": "event_msg",
            "payload": { "type": "token_count", "rate_limits": {
                "primary": { "used_percent": 50.0, "window_minutes": 300, "resets_in_seconds": 600 }
            }}
        })
        .to_string();
        let (readings, at) = codex_line(&line).unwrap();
        assert_eq!(readings[0].resets_at, Some(at + 600_000));
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp("2026-10-16T09:12:03.5Z"),
            Some(1_792_141_923_500)
        );
        assert_eq!(
            parse_timestamp("2026-10-16T11:12:03.500+02:00"),
            parse_timestamp("2026-10-16T09:12:03.500Z")
        );
        assert_eq!(parse_timestamp("yesterday"), None);
    }

    #[test]
    fn readings_never_move_a_window_backwards() {
        let old = window(40.0, 10 * HOUR, HOUR);
        let at = 2 * HOUR;
        // Same window, more used: taken.
        let newer = merge(
            Some(&old),
            Subscription::Claude,
            reading(45.0, 10 * HOUR),
            at,
        )
        .unwrap();
        assert_eq!((newer.used_percent, newer.observed_at), (45.0, at));
        // Same window, less used: an idle session's old numbers.
        assert!(
            merge(
                Some(&old),
                Subscription::Claude,
                reading(30.0, 10 * HOUR),
                at
            )
            .is_none()
        );
        // An earlier window: dropped.
        assert!(
            merge(
                Some(&old),
                Subscription::Claude,
                reading(90.0, 5 * HOUR),
                at
            )
            .is_none()
        );
        // A later window: starts over.
        let next = merge(
            Some(&old),
            Subscription::Claude,
            reading(2.0, 15 * HOUR),
            at,
        )
        .unwrap();
        assert_eq!((next.used_percent, next.reset_handled), (2.0, false));
        // Unchanged: nothing to write.
        assert!(
            merge(
                Some(&old),
                Subscription::Claude,
                reading(40.0, 10 * HOUR),
                HOUR
            )
            .is_none()
        );
    }

    #[test]
    fn only_well_used_windows_notify_when_they_reset() {
        let w = window(85.0, 10 * HOUR, HOUR);
        assert_eq!(check_reset(&w, 9 * HOUR, Some(80.0)), ResetCheck::Pending);
        assert_eq!(check_reset(&w, 10 * HOUR, Some(80.0)), ResetCheck::Notify);
        assert_eq!(check_reset(&w, 10 * HOUR, Some(90.0)), ResetCheck::Quiet);
        assert_eq!(check_reset(&w, 10 * HOUR, None), ResetCheck::Quiet);
        // Noticed hours later, say after a restart.
        assert_eq!(check_reset(&w, 13 * HOUR, Some(80.0)), ResetCheck::Quiet);
        let handled = Window {
            reset_handled: true,
            ..w
        };
        assert_eq!(
            check_reset(&handled, 10 * HOUR, Some(80.0)),
            ResetCheck::Pending
        );
    }

    #[test]
    fn old_and_reset_windows_are_not_shown_as_current() {
        let w = window(40.0, 10 * HOUR, HOUR);
        let current = view(&w, HOUR + 60_000);
        assert_eq!((current.used_percent, current.stale), (Some(40.0), false));
        let old = view(&w, 3 * HOUR);
        assert_eq!((old.used_percent, old.stale), (Some(40.0), true));
        assert_eq!(old.age_ms, 2 * HOUR);
        let reset = view(&w, 11 * HOUR);
        assert_eq!(
            (reset.used_percent, reset.stale, reset.reset_since_observed),
            (None, true, true)
        );
        assert_eq!(reset.window, "5h");
        assert_eq!(short_label(10_080), "7d");
        assert_eq!(long_label(10_080), "7-day");
    }
}
