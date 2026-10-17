//! Run results go to the exact submitting Session, never to a worktree match.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::report::{self, TAIL_LINES};
use crate::session::Harness;
use crate::store::Run;

pub fn message(run: &Run) -> String {
    let mut text = format!(
        "geleider Run #{}: {} {}. Exit code: {}. Duration: {}.\nWorktree: {}\nFull log: {}",
        run.id,
        report::title(run),
        report::outcome(run),
        run.exit_code.map_or("unknown".into(), |c| c.to_string()),
        report::duration(run.duration_ms().unwrap_or(0)),
        run.worktree.display(),
        run.log_path.display(),
    );
    if let Some(error) = &run.error {
        text.push_str(&format!("\nError: {error}"));
    }
    let tail = report::log_tail(&run.log_path, TAIL_LINES);
    if !tail.is_empty() {
        text.push_str(&format!("\n\nLast log lines (Job output):\n{tail}"));
    }
    text
}

pub fn codex(run: &Run) -> Result<()> {
    let Some(session) = &run.session else {
        return Ok(());
    };
    if session.harness != Harness::Codex {
        return Ok(());
    }
    let mut child = Command::new("codex")
        .args(["queue", "--thread", &session.id, "--message", &message(run)])
        .current_dir(&run.worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("codex queue exited {status}")
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("codex queue timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
