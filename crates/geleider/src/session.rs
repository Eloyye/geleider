//! Harness identities and the Session state reported by lifecycle hooks.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Claude,
    Codex,
    Opencode,
}

impl Harness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKey {
    pub harness: Harness,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Working,
    Waiting,
    Idle,
    Ended,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Ended => "ended",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(flatten)]
    pub key: SessionKey,
    pub project: Option<String>,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub state: SessionState,
    pub last_activity: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_prompts: Vec<String>,
    #[serde(default)]
    pub owner_pid: Option<u32>,
    #[serde(default)]
    pub owner_started: Option<String>,
}

/// Small datagram: no prompts, tool inputs, transcripts or credentials leave the hook.
#[derive(Debug, Serialize, Deserialize)]
pub struct Activity {
    pub key: SessionKey,
    pub cwd: PathBuf,
    pub state: SessionState,
    pub at: i64,
    pub wait_for: Option<String>,
    pub resolved: Option<String>,
    pub reset: bool,
    pub owner_pid: Option<u32>,
}

pub fn activity(harness: Harness, event: &str, input: &Value) -> Option<Activity> {
    // Claude's child tool events share the parent's session_id. Their Stop/tool
    // events must not turn a waiting/working parent idle or working.
    if input.get("agent_id").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let state = match event {
        "SessionStart" | "session.created" => SessionState::Idle,
        "SessionEnd" | "session.deleted" => SessionState::Ended,
        "Stop" | "Interrupt" | "StopFailure" | "session.idle" | "session.error" => {
            SessionState::Idle
        }
        "PermissionRequest" | "permission.asked" | "form.created" => SessionState::Waiting,
        "Notification" => match input["notification_type"].as_str()? {
            "permission_prompt" | "elicitation_dialog" => SessionState::Waiting,
            // idle_prompt is a reminder, not a question blocking the Session.
            _ => return None,
        },
        "PreToolUse" | "tool.before" => {
            if is_question(input["tool_name"].as_str().unwrap_or_default()) {
                SessionState::Waiting
            } else {
                SessionState::Working
            }
        }
        "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" | "permission.replied"
        | "form.replied" | "form.cancelled" | "tool.after" => SessionState::Working,
        "session.status" => match input["status"]["type"].as_str()? {
            "idle" => SessionState::Idle,
            "busy" | "retry" => SessionState::Working,
            _ => return None,
        },
        _ => return None,
    };
    let key = identity(harness, input)?;
    let cwd = PathBuf::from(input["cwd"].as_str()?);
    if !cwd.is_absolute() {
        return None;
    }
    let tool_id = || input["tool_use_id"].as_str().unwrap_or("user").to_string();
    let wait_for = (state == SessionState::Waiting).then(|| match event {
        "permission.asked" => input["id"].as_str().unwrap_or("user").to_string(),
        "form.created" => input["form"]["id"].as_str().unwrap_or("user").to_string(),
        _ => tool_id(),
    });
    let resolved = match event {
        "PostToolUse" | "PostToolUseFailure" | "tool.after" => Some(tool_id()),
        "permission.replied" => Some(input["requestID"].as_str().unwrap_or("user").to_string()),
        "form.replied" | "form.cancelled" => {
            Some(input["id"].as_str().unwrap_or("user").to_string())
        }
        _ => None,
    };
    Some(Activity {
        key,
        cwd,
        state,
        at: crate::store::now_ms(),
        wait_for,
        resolved,
        owner_pid: input["owner_pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| *p > 1)
            .or_else(|| {
                std::env::var("GELEIDER_HARNESS_PID")
                    .ok()?
                    .parse()
                    .ok()
                    .filter(|p| *p > 1)
            }),
        reset: matches!(state, SessionState::Idle | SessionState::Ended)
            || event == "UserPromptSubmit",
    })
}

pub fn identity(harness: Harness, input: &Value) -> Option<SessionKey> {
    let id = input["session_id"].as_str()?.trim();
    if id.is_empty() || id.len() > 512 {
        return None;
    }
    Some(SessionKey {
        harness,
        id: id.into(),
    })
}

fn is_question(tool: &str) -> bool {
    tool == "AskUserQuestion"
        || tool == "question"
        || tool.ends_with("request_user_input")
        || tool.ends_with("request_user_input_async")
}

/// Only this tool's arguments may be annotated by a PreToolUse hook.
pub fn is_submit_tool(tool: &str) -> bool {
    matches!(tool, "mcp__geleider__submit_run" | "geleider_submit_run")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn waiting_is_a_prompt_not_an_idle_reminder() {
        let mut input = json!({"session_id":"s", "cwd":"/wt"});
        let state = |event, input: &Value| activity(Harness::Codex, event, input).map(|a| a.state);
        assert_eq!(
            state("PermissionRequest", &input),
            Some(SessionState::Waiting)
        );
        assert_eq!(state("Stop", &input), Some(SessionState::Idle));
        input["tool_name"] = json!("request_user_input");
        assert_eq!(state("PreToolUse", &input), Some(SessionState::Waiting));
        assert_eq!(state("PostToolUse", &input), Some(SessionState::Working));
        input["notification_type"] = json!("idle_prompt");
        assert_eq!(state("Notification", &input), None);
        input["agent_id"] = json!("child");
        assert_eq!(state("Stop", &input), None);
    }
}
