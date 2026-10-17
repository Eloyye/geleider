//! Best-effort hook ingress. Never waits on the daemon, git, or a network service.

use std::io::{Read, Write};
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::attest::{Attestation, Destination, Nonce};
use crate::claim;
use crate::paths::Paths;
use crate::session::{self, Activity, Harness};

/// The longest a Harness waits on stdin before the hook gives up.
const INPUT_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_INPUT_BYTES: u64 = 1024 * 1024;
/// The daemon's receive buffer; larger datagrams are not sent.
pub const MAX_DATAGRAM_BYTES: usize = 8192;

/// One message to the daemon's hook socket.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "datagram", rename_all = "snake_case")]
pub enum Datagram {
    Activity(Activity),
    Attestation(Attestation),
}

pub fn run(harness: Harness, event: &str) {
    if let Ok(paths) = Paths::from_env() {
        let _ = forward(&paths, harness, event);
    }
}

fn forward(paths: &Paths, harness: Harness, event: &str) -> Result<()> {
    let input: Value = serde_json::from_slice(&read_input()?)?;

    // Per-call identity works even when the Harness shares an MCP server across
    // Sessions. stdout is reserved for the Harness hook contract, not diagnostics.
    if matches!(event, "PreToolUse" | "tool.before")
        && session::is_attested_tool(input["tool_name"].as_str().unwrap_or_default())
    {
        let (args, attestation) = annotate_submission(harness, &input, Nonce::generate())?;
        // Sent before the call goes ahead, so the daemon has it by the time the
        // submission arrives. Without it, the Run is kept but not delivered.
        if let Some(attestation) = attestation {
            let _ = send(paths, &Datagram::Attestation(attestation));
        }
        reply("PreToolUse", "updatedInput", json!(args));
    }
    // Claude Code and Codex add a SessionStart hook's `additionalContext` to the
    // conversation. Only files are read, never the daemon or git.
    if event == "SessionStart"
        && harness != Harness::Opencode
        && let (Some(key), Some(cwd)) = (session::identity(harness, &input), input["cwd"].as_str())
        && let Some(warning) = claim::start_warning(&paths.claims(), &key, Path::new(cwd))
    {
        reply("SessionStart", "additionalContext", json!(warning));
    }
    match session::activity(harness, event, &input) {
        Some(activity) => send(paths, &Datagram::Activity(activity)),
        None => Ok(()),
    }
}

/// Answers the Harness with one `hookSpecificOutput` field for `event`.
fn reply(event: &str, field: &str, value: Value) {
    let output = json!({"hookSpecificOutput": {"hookEventName": event, field: value}});
    let _ = writeln!(std::io::stdout(), "{output}");
}

fn send(paths: &Paths, datagram: &Datagram) -> Result<()> {
    let bytes = serde_json::to_vec(datagram)?;
    if bytes.len() > MAX_DATAGRAM_BYTES {
        return Ok(());
    }
    let socket = UnixDatagram::unbound()?;
    socket.set_nonblocking(true)?;
    socket.send_to(&bytes, paths.hook_socket())?;
    Ok(())
}

/// The submit_run, claim or release arguments with the Session the Harness says made this call,
/// and the attestation that lets the daemon trust it. A `session` the model
/// wrote itself is always replaced or dropped, never passed on: otherwise one
/// conversation could have its Run result, and the Job output in it, queued as
/// a prompt in another.
fn annotate_submission(
    harness: Harness,
    input: &Value,
    nonce: Option<Nonce>,
) -> Result<(Map<String, Value>, Option<Attestation>)> {
    let mut args = input["tool_input"].as_object().cloned().unwrap_or_default();
    args.remove("session");
    let mut attestation = None;
    if let Some(key) = session::identity(harness, input) {
        let destination = Destination {
            key: key.clone(),
            nonce: nonce.clone(),
        };
        args.insert("session".into(), serde_json::to_value(destination)?);
        attestation = nonce.map(|nonce| Attestation {
            session: key,
            nonce,
        });
    }
    if let Some(cwd) = input["cwd"].as_str() {
        args.entry("cwd").or_insert(json!(cwd));
    }
    Ok((args, attestation))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_written_session_never_survives_annotation() {
        let input = json!({"cwd":"/wt","tool_input":{"job":"j","session":{"harness":"codex","id":"victim"}}});
        let (args, attestation) =
            annotate_submission(Harness::Codex, &input, Nonce::generate()).unwrap();
        assert!(!args.contains_key("session"));
        assert!(attestation.is_none());
        assert_eq!(args["cwd"], "/wt");

        let mut input = input;
        input["session_id"] = json!("caller");
        let nonce = Nonce::generate().unwrap();
        let (args, attestation) =
            annotate_submission(Harness::Codex, &input, Some(nonce.clone())).unwrap();
        assert_eq!(
            args["session"],
            json!({"harness":"codex","id":"caller","nonce":nonce})
        );
        assert_eq!(args["job"], "j");
        let attestation = attestation.unwrap();
        assert_eq!(attestation.session.id, "caller");
        assert_eq!(attestation.nonce, nonce);
        // Without a random source the destination is passed on unattested.
        let (args, attestation) = annotate_submission(Harness::Codex, &input, None).unwrap();
        assert_eq!(args["session"], json!({"harness":"codex","id":"caller"}));
        assert!(attestation.is_none());
    }

    #[test]
    fn datagrams_say_what_they_carry() {
        let attestation = Datagram::Attestation(Attestation {
            session: crate::session::SessionKey::new(Harness::Opencode, "ses_1").unwrap(),
            nonce: Nonce::generate().unwrap(),
        });
        let bytes = serde_json::to_vec(&attestation).unwrap();
        assert!(bytes.len() < MAX_DATAGRAM_BYTES);
        assert!(matches!(
            serde_json::from_slice(&bytes).unwrap(),
            Datagram::Attestation(_)
        ));
        let forged =
            json!({"datagram":"attestation","session":{"harness":"codex","id":"-x"},"nonce":"0"});
        assert!(serde_json::from_value::<Datagram>(forged).is_err());
    }
}
