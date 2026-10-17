//! `geleider mcp`: an MCP server over stdio that lets a Session submit and check
//! Runs. It only forwards to the background process, which runs the Job outside
//! the Harness's sandbox.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::paths::Paths;
use crate::protocol::{self, Reply, Request};

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve(paths: &Paths) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut server = Server::default();
    let call = |request: &Request| protocol::call(paths, request);
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle(message, &call),
            Err(err) => Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            )),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct Server {
    /// The Harness that connected, from `initialize`, recorded as the submitter.
    client: Option<String>,
}

impl Server {
    /// Answers one JSON-RPC message; notifications get no answer.
    fn handle(
        &mut self,
        message: Value,
        call: &dyn Fn(&Request) -> Result<Reply>,
    ) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let id = id?; // a notification, such as `notifications/initialized`

        let result = match method {
            "initialize" => {
                self.client = params["clientInfo"]["name"].as_str().map(str::to_string);
                let requested = params["protocolVersion"].as_str().unwrap_or_default();
                let version = PROTOCOL_VERSIONS
                    .iter()
                    .find(|v| **v == requested)
                    .unwrap_or(&PROTOCOL_VERSIONS[0]);
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "geleider", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Use geleider to run Jobs that need a shared Resource (such as a game \
                        used by integration tests). Submit with submit_run; it returns at once with the Run id \
                        and queue position. geleider's hooks attest the calling Session on each submission, so Codex and OpenCode receive the result automatically; \
                        the reply's `delivery` says whether this one will be. If it is off, check get_run; Claude Code should use geleider run --wait. \
                        usage shows how much of each Subscription (Claude Pro, ChatGPT Plus) is used.",
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => self.call_tool(&params, call),
            _ => {
                return Some(error_response(
                    id,
                    -32601,
                    &format!("unknown method `{method}`"),
                ));
            }
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn call_tool(&self, params: &Value, call: &dyn Fn(&Request) -> Result<Reply>) -> Value {
        let name = params["name"].as_str().unwrap_or_default();
        let args = &params["arguments"];
        match self.tool(name, args, call) {
            Ok(value) => json!({
                "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
                "structuredContent": value,
            }),
            Err(err) => json!({
                "content": [{ "type": "text", "text": format!("{err:#}") }],
                "isError": true,
            }),
        }
    }

    fn tool(
        &self,
        name: &str,
        args: &Value,
        call: &dyn Fn(&Request) -> Result<Reply>,
    ) -> Result<Value> {
        let request = match name {
            "submit_run" => {
                let job = args["job"]
                    .as_str()
                    .context("`job` is required")?
                    .to_string();
                let job_args = match &args["args"] {
                    Value::Null => Vec::new(),
                    value => serde_json::from_value(value.clone())
                        .context("`args` must be a list of strings")?,
                };
                let cwd = match args["cwd"].as_str() {
                    Some(cwd) => PathBuf::from(cwd),
                    None => std::env::current_dir()?,
                };
                let submitter = format!("mcp:{}", self.client.as_deref().unwrap_or("unknown"));
                Request::Submit {
                    cwd,
                    job,
                    args: job_args,
                    submitter,
                    // Never infer a Session from clientInfo, cwd, or server env:
                    // desktop apps may share this process between conversations.
                    // The daemon keeps this only if a hook attested its nonce.
                    session: args
                        .get("session")
                        .filter(|s| !s.is_null())
                        .map(|s| serde_json::from_value(s.clone()))
                        .transpose()
                        .context("`session` must contain harness, id and nonce")?,
                }
            }
            "get_run" => Request::GetRun {
                id: args["id"].as_i64().context("`id` is required")?,
            },
            "list_runs" => Request::ListRuns {
                finished: if args["include_finished"].as_bool().unwrap_or(false) {
                    20
                } else {
                    0
                },
            },
            "status" => Request::Status,
            "usage" => Request::Usage,
            _ => bail!("unknown tool `{name}`"),
        };
        let value = match call(&request)? {
            Reply::Run(run) => serde_json::to_value(*run)?,
            Reply::Runs { runs } => json!({ "runs": runs }),
            Reply::Status(status) => serde_json::to_value(status)?,
            Reply::Usage { subscriptions } => json!({ "subscriptions": subscriptions }),
            Reply::Worktrees(inventory) => serde_json::to_value(inventory)?,
            Reply::Cleaned(cleaned) => serde_json::to_value(cleaned)?,
        };
        Ok(value)
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "submit_run",
            "description": "Queue a Run of a Job declared in geleider's config, in the worktree you are working in. \
                Returns at once with the Run id, state and queue position; the Job runs outside your sandbox \
                once its Resource has a free slot. The reply's `delivery` says whether the result will be sent \
                to you when the Run finishes; if not, poll get_run for it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job": { "type": "string", "description": "Job name, e.g. `ck3-test`." },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "The Job's positional args." },
                    "cwd": { "type": "string", "description": "A directory in your worktree. Defaults to this server's working directory." },
                    "session": { "type": "object", "description": "Set by geleider's Harness hook; do not pass it. Any value you write is replaced, and one no hook attested turns result delivery off.",
                        "properties": { "harness": { "type": "string", "enum": ["claude", "codex", "opencode"] }, "id": { "type": "string", "minLength": 1 }, "nonce": { "type": "string" } }, "required": ["harness", "id"], "additionalProperties": false }
                },
                "required": ["job"]
            }
        },
        {
            "name": "get_run",
            "description": "A Run's state, queue position and, once it has started, the tail of its log. \
                When finished: exit code, duration and full log path.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        },
        {
            "name": "list_runs",
            "description": "Running and queued Runs across all Projects, oldest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_finished": { "type": "boolean", "description": "Also list the 20 most recently finished Runs." }
                }
            }
        },
        {
            "name": "status",
            "description": "Sessions grouped by Project (waiting first), every Resource with its slots and current Lease holders, running and queued Runs, \
                Subscription usage as the usage tool reports it, and Finished worktrees eligible for confirmed CLI cleanup.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "usage",
            "description": "How much of each Subscription (Claude Pro, ChatGPT Plus) is used in each Limit window \
                (5h, 7d): used percentage, reset time (Unix ms) and how old the numbers are. A window marked \
                stale is last known, not current; used_percent is null once it has reset with no data since.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn initialize_list_and_submit() {
        let mut server = Server::default();
        let seen = RefCell::new(Vec::new());
        let call = |request: &Request| -> Result<Reply> {
            seen.borrow_mut()
                .push(serde_json::to_value(request).unwrap());
            bail!("no daemon in this test")
        };

        let init = server
            .handle(
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                       "params": {"protocolVersion": "2025-03-26", "clientInfo": {"name": "codex"}}}),
                &call,
            )
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(
            server
                .handle(
                    json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                    &call
                )
                .is_none()
        );

        let list = server
            .handle(
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                &call,
            )
            .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["submit_run", "get_run", "list_runs", "status", "usage"]
        );

        let result = server
            .handle(
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                       "params": {"name": "submit_run", "arguments": {"job": "ck3-test", "args": ["events"], "cwd": "/wt"}}}),
                &call,
            )
            .unwrap();
        assert_eq!(result["result"]["isError"], true);
        assert_eq!(
            seen.borrow()[0],
            json!({"op": "submit", "cwd": "/wt", "job": "ck3-test", "args": ["events"], "submitter": "mcp:codex", "session": null})
        );

        let unknown = server
            .handle(json!({"jsonrpc": "2.0", "id": 4, "method": "nope"}), &call)
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }
}
