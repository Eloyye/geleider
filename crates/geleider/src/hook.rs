//! Best-effort hook ingress. Never waits on the daemon, git, or a network service.

use std::io::{Read, Write};
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::paths::Paths;
use crate::session::{self, Harness, SessionKey};

pub fn run(paths: &Paths, harness: Harness, event: &str) {
    let _ = forward(paths, harness, event);
}

fn forward(paths: &Paths, harness: Harness, event: &str) -> Result<()> {
    let input: Value = serde_json::from_slice(&read_input()?)?;

    // Per-call identity works even when the Harness shares an MCP server across
    // Sessions. stdout is reserved for the Harness hook contract, not diagnostics.
    if event == "PreToolUse"
        && session::is_submit_tool(input["tool_name"].as_str().unwrap_or_default())
    {
        let mut args = input["tool_input"].as_object().cloned().unwrap_or_default();
        if let Some(key) = session::identity(harness, &input) {
            args.insert("session".into(), serde_json::to_value(key)?);
        }
        if let Some(cwd) = input["cwd"].as_str() {
            args.entry("cwd").or_insert(json!(cwd));
        }
        let output =
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": args}});
        let _ = writeln!(std::io::stdout(), "{output}");
    }
    let Some(activity) = session::activity(harness, event, &input) else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(&activity)?;
    if bytes.len() > 8192 {
        return Ok(());
    }
    let socket = UnixDatagram::unbound()?;
    socket.set_nonblocking(true)?;
    socket.send_to(&bytes, paths.hook_socket())?;
    Ok(())
}

/// Bound both size and time, including a pipe whose writer never closes stdin.
fn read_input() -> Result<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut bytes = Vec::new();
    let mut stdin = std::io::stdin();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("hook input timed out")
        }
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd points to one initialized pollfd for the duration of poll.
        if unsafe { libc::poll(&mut fd, 1, remaining.as_millis().max(1) as i32) } <= 0 {
            bail!("hook input timed out");
        }
        let mut chunk = [0; 8192];
        let read = stdin.read(&mut chunk)?;
        if read == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > 1024 * 1024 {
            bail!("hook input too large")
        }
    }
}

/// Shell execution supplies the originating id; MCP uses the per-call hook
/// annotation instead, because its server process can be shared by Sessions.
pub fn shell_session() -> Option<SessionKey> {
    std::env::var("CODEX_THREAD_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .map(|id| SessionKey {
            harness: Harness::Codex,
            id,
        })
}
