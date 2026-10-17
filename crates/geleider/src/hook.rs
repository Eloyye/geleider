//! Best-effort hook ingress. Never waits on the daemon, git, or a network service.

use std::io::{Read, Write};
use std::os::unix::net::UnixDatagram;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::paths::Paths;
use crate::session::{self, Harness, SessionKey};

/// The longest a Harness waits on stdin before the hook gives up.
const INPUT_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_INPUT_BYTES: u64 = 1024 * 1024;
/// The daemon's receive buffer; larger datagrams are not sent.
pub const MAX_DATAGRAM_BYTES: usize = 8192;

pub fn run(harness: Harness, event: &str) {
    if let Ok(paths) = Paths::from_env() {
        let _ = forward(&paths, harness, event);
    }
}

fn forward(paths: &Paths, harness: Harness, event: &str) -> Result<()> {
    let input: Value = serde_json::from_slice(&read_input()?)?;

    // Per-call identity works even when the Harness shares an MCP server across
    // Sessions. stdout is reserved for the Harness hook contract, not diagnostics.
    if event == "PreToolUse"
        && session::is_submit_tool(input["tool_name"].as_str().unwrap_or_default())
    {
        let args = annotate_submission(harness, &input)?;
        let output =
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": args}});
        let _ = writeln!(std::io::stdout(), "{output}");
    }
    let Some(activity) = session::activity(harness, event, &input) else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(&activity)?;
    if bytes.len() > MAX_DATAGRAM_BYTES {
        return Ok(());
    }
    let socket = UnixDatagram::unbound()?;
    socket.set_nonblocking(true)?;
    socket.send_to(&bytes, paths.hook_socket())?;
    Ok(())
}

/// The submit_run arguments with the Session the Harness says made this call.
/// A `session` the model wrote itself is always replaced or dropped, never
/// passed on: otherwise one conversation could have its Run result, and the
/// Job output in it, queued as a prompt in another.
fn annotate_submission(harness: Harness, input: &Value) -> Result<Map<String, Value>> {
    let mut args = input["tool_input"].as_object().cloned().unwrap_or_default();
    args.remove("session");
    if let Some(key) = session::identity(harness, input) {
        args.insert("session".into(), serde_json::to_value(key)?);
    }
    if let Some(cwd) = input["cwd"].as_str() {
        args.entry("cwd").or_insert(json!(cwd));
    }
    Ok(args)
}

/// Bounds both size and time, including a pipe whose writer never closes stdin.
/// A reader left blocked is ended with the process, which exits right after.
fn read_input() -> Result<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = std::io::stdin()
            .take(MAX_INPUT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(read);
    });
    let bytes = receiver
        .recv_timeout(INPUT_TIMEOUT)
        .context("hook input timed out")??;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        bail!("hook input too large")
    }
    Ok(bytes)
}

/// Shell execution supplies the originating id; MCP uses the per-call hook
/// annotation instead, because its server process can be shared by Sessions.
pub fn shell_session() -> Option<SessionKey> {
    SessionKey::new(Harness::Codex, &std::env::var("CODEX_THREAD_ID").ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_written_session_never_survives_annotation() {
        let input = json!({"cwd":"/wt","tool_input":{"job":"j","session":{"harness":"codex","id":"victim"}}});
        let args = annotate_submission(Harness::Codex, &input).unwrap();
        assert!(!args.contains_key("session"));
        assert_eq!(args["cwd"], "/wt");

        let mut input = input;
        input["session_id"] = json!("caller");
        let args = annotate_submission(Harness::Codex, &input).unwrap();
        assert_eq!(args["session"], json!({"harness":"codex","id":"caller"}));
        assert_eq!(args["job"], "j");
    }
}
