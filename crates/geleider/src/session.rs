//! Harness identities and the Session state reported by lifecycle hooks.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The stand-in for a prompt the Harness reported without an id of its own.
const ANONYMOUS_PROMPT: &str = "user";
/// Outstanding prompts kept per Session; the oldest is dropped beyond this.
const MAX_PENDING_PROMPTS: usize = 32;
const MAX_ID_LEN: usize = 256;

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

    /// The product name, as Claim comments show it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Opencode => "OpenCode",
        }
    }
}

/// Which Session a Run's result goes to. Every way in (hook stdin, the hook
/// socket, MCP arguments, the daemon socket) deserializes through [`SessionKey::new`],
/// so an id is never empty, never an option to a command it is passed to, and
/// never holds terminal escapes for `geleider status` to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedKey")]
pub struct SessionKey {
    pub harness: Harness,
    pub id: String,
}

#[derive(Deserialize)]
struct UncheckedKey {
    harness: Harness,
    id: String,
}

impl TryFrom<UncheckedKey> for SessionKey {
    type Error = String;

    fn try_from(key: UncheckedKey) -> Result<Self, String> {
        Self::new(key.harness, &key.id).ok_or_else(|| format!("invalid Session id {:?}", key.id))
    }
}

impl SessionKey {
    /// Harness Session ids are UUIDs or ids such as `ses_01J…`.
    pub fn new(harness: Harness, id: &str) -> Option<Self> {
        let id = id.trim();
        let valid = !id.is_empty()
            && id.len() <= MAX_ID_LEN
            && !id.starts_with('-')
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
        valid.then(|| Self {
            harness,
            id: id.to_string(),
        })
    }
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
    /// OpenCode's selected provider; the other Harnesses have a fixed vendor.
    #[serde(default)]
    pub provider_id: Option<String>,
    /// Cross-review Sessions never identify a PR's writing vendor.
    #[serde(default)]
    pub review: bool,
}

impl Session {
    /// `next`, built from `activity` alone, completed with what `previous` knew.
    /// Outstanding permission and question prompts keep a Session waiting while
    /// other tools finish. `None` when `previous` is newer: datagrams can arrive
    /// out of order.
    pub fn after(
        previous: Option<Session>,
        mut next: Session,
        activity: &Activity,
    ) -> Option<Self> {
        if let Some(previous) = previous {
            if previous.last_activity > next.last_activity {
                return None;
            }
            if next.owner_pid.is_none() {
                next.owner_pid = previous.owner_pid;
                next.owner_started = previous.owner_started;
            }
            next.provider_id = next.provider_id.or(previous.provider_id);
            next.review |= previous.review;
            if !activity.reset {
                next.pending_prompts = previous.pending_prompts;
            }
        }
        if let Some(resolved) = &activity.resolved {
            next.pending_prompts
                .retain(|p| p != resolved && p != ANONYMOUS_PROMPT);
        }
        if let Some(prompt) = &activity.wait_for {
            next.wait_on(prompt);
        }
        if !next.pending_prompts.is_empty() {
            next.state = SessionState::Waiting;
        }
        Some(next)
    }

    fn wait_on(&mut self, prompt: &str) {
        let prompts = &mut self.pending_prompts;
        if prompts.iter().any(|p| p == prompt) {
            return;
        }
        if prompt == ANONYMOUS_PROMPT {
            // Only stands in while no identified prompt is known.
            if prompts.is_empty() {
                prompts.push(prompt.into());
            }
            return;
        }
        prompts.retain(|p| p != ANONYMOUS_PROMPT);
        if prompts.len() >= MAX_PENDING_PROMPTS {
            prompts.remove(0);
        }
        prompts.push(prompt.into());
    }
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
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub review: bool,
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
        "message.updated" | "session.step.started" => SessionState::Working,
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
    let tool_id = || prompt_id(&input["tool_use_id"]);
    let wait_for = (state == SessionState::Waiting).then(|| match event {
        "permission.asked" => prompt_id(&input["id"]),
        "form.created" => prompt_id(&input["form"]["id"]),
        _ => tool_id(),
    });
    let resolved = match event {
        "PostToolUse" | "PostToolUseFailure" | "tool.after" => Some(tool_id()),
        "permission.replied" => Some(prompt_id(&input["requestID"])),
        "form.replied" | "form.cancelled" => Some(prompt_id(&input["id"])),
        _ => None,
    };
    Some(Activity {
        key,
        cwd,
        state,
        at: crate::store::now_ms(),
        wait_for,
        resolved,
        owner_pid: owner_pid(input),
        provider_id: input["provider_id"].as_str().map(str::to_owned),
        review: false,
        reset: matches!(state, SessionState::Idle | SessionState::Ended)
            || event == "UserPromptSubmit",
    })
}

pub fn identity(harness: Harness, input: &Value) -> Option<SessionKey> {
    SessionKey::new(harness, input["session_id"].as_str()?)
}

fn prompt_id(value: &Value) -> String {
    value.as_str().unwrap_or(ANONYMOUS_PROMPT).to_string()
}

/// The Harness process, so a Session it never ended can be ended once it exits.
fn owner_pid(input: &Value) -> Option<u32> {
    let reported = input["owner_pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok());
    let parent = || std::env::var("GELEIDER_HARNESS_PID").ok()?.parse().ok();
    reported.or_else(parent).filter(|p| *p > 1)
}

fn is_question(tool: &str) -> bool {
    tool == "AskUserQuestion"
        || tool == "question"
        || tool.ends_with("request_user_input")
        || tool.ends_with("request_user_input_async")
}

/// Only these tools' arguments are annotated with the calling Session by a
/// PreToolUse hook: where a Run's result goes, and who claims or releases an issue.
pub fn is_attested_tool(tool: &str) -> bool {
    let name = tool
        .strip_prefix("mcp__geleider__")
        .or_else(|| tool.strip_prefix("geleider_"));
    matches!(name, Some("submit_run" | "claim" | "release"))
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

    #[test]
    fn only_geleiders_session_naming_tools_are_attested() {
        for tool in [
            "mcp__geleider__submit_run",
            "geleider_claim",
            "mcp__geleider__release",
        ] {
            assert!(is_attested_tool(tool), "{tool}");
        }
        for tool in [
            "mcp__geleider__status",
            "geleider_get_run",
            "mcp__other__claim",
            "claim",
        ] {
            assert!(!is_attested_tool(tool), "{tool}");
        }
    }

    #[test]
    fn session_ids_cannot_carry_options_or_terminal_escapes() {
        for id in ["019a-uuid", "ses_01JABC", "thread.1:2", "  padded  "] {
            assert!(SessionKey::new(Harness::Codex, id).is_some(), "{id}");
        }
        let long = "a".repeat(MAX_ID_LEN + 1);
        for id in [
            "",
            "   ",
            "--help",
            "a b",
            "a\nb",
            "\u{1b}[2J",
            "a/b",
            &long,
        ] {
            assert!(SessionKey::new(Harness::Codex, id).is_none(), "{id:?}");
        }
        // Every deserialization path validates, not only the hook's.
        let forged = json!({"harness":"codex","id":"--dangerously-bypass"});
        assert!(serde_json::from_value::<SessionKey>(forged).is_err());
        let datagram = json!({"key":{"harness":"codex","id":"\u{1b}]0;x"},"cwd":"/wt",
            "state":"idle","at":0,"wait_for":null,"resolved":null,"reset":true,"owner_pid":null});
        assert!(serde_json::from_value::<Activity>(datagram).is_err());
    }

    fn event(input: Value, event: &str) -> (Session, Activity) {
        let activity = activity(Harness::Codex, event, &input).unwrap();
        let session = Session {
            key: activity.key.clone(),
            project: None,
            worktree: activity.cwd.clone(),
            branch: None,
            state: activity.state,
            last_activity: activity.at,
            pending_prompts: Vec::new(),
            owner_pid: None,
            owner_started: None,
            provider_id: None,
            review: false,
        };
        (session, activity)
    }

    #[test]
    fn provider_and_review_metadata_survive_ordinary_hooks_and_old_records() {
        let input = json!({"session_id":"s","cwd":"/wt","provider_id":"openai"});
        let activity = activity(Harness::Opencode, "session.step.started", &input).unwrap();
        assert_eq!(activity.provider_id.as_deref(), Some("openai"));
        let (mut previous, _) = event(input.clone(), "UserPromptSubmit");
        previous.provider_id = activity.provider_id;
        previous.review = true;
        let (next, activity) = event(input, "Stop");
        let next = Session::after(Some(previous), next, &activity).unwrap();
        assert_eq!(next.provider_id.as_deref(), Some("openai"));
        assert!(next.review);
        let mut old = serde_json::to_value(next).unwrap();
        old.as_object_mut().unwrap().remove("review");
        old.as_object_mut().unwrap().remove("provider_id");
        let old: Session = serde_json::from_value(old).unwrap();
        assert!(!old.review && old.provider_id.is_none());
    }

    #[test]
    fn prompts_keep_a_session_waiting_until_each_is_resolved() {
        let base = json!({"session_id":"s","cwd":"/wt"});
        let with = |id: &str| {
            let mut input = base.clone();
            input["tool_use_id"] = json!(id);
            input
        };
        let step = |previous: Option<Session>, (next, activity): (Session, Activity)| {
            Session::after(previous, next, &activity).unwrap()
        };
        let s = step(None, event(base.clone(), "UserPromptSubmit"));
        let s = step(Some(s), event(with("p1"), "PermissionRequest"));
        let s = step(Some(s), event(with("p2"), "PermissionRequest"));
        let s = step(Some(s), event(with("other"), "PostToolUse"));
        assert_eq!(s.state, SessionState::Waiting);
        assert_eq!(s.pending_prompts, ["p1", "p2"]);
        let s = step(Some(s), event(with("p1"), "PostToolUse"));
        assert_eq!(s.state, SessionState::Waiting);
        let s = step(Some(s), event(with("p2"), "PostToolUse"));
        assert_eq!(s.state, SessionState::Working);
        // An anonymous prompt gives way to an identified one.
        let s = step(Some(s), event(base.clone(), "PermissionRequest"));
        assert_eq!(s.pending_prompts, [ANONYMOUS_PROMPT]);
        let s = step(Some(s), event(with("p3"), "PermissionRequest"));
        assert_eq!(s.pending_prompts, ["p3"]);
        // Stop clears everything outstanding.
        let s = step(Some(s), event(base.clone(), "Stop"));
        assert_eq!(s.state, SessionState::Idle);
        assert!(s.pending_prompts.is_empty());

        let (mut older, activity) = event(base, "Stop");
        older.last_activity = s.last_activity - 1;
        assert!(Session::after(Some(s), older, &activity).is_none());
    }

    #[test]
    fn pending_prompts_are_bounded() {
        let mut session = event(json!({"session_id":"s","cwd":"/wt"}), "Stop").0;
        for i in 0..MAX_PENDING_PROMPTS + 5 {
            session.wait_on(&format!("p{i}"));
        }
        assert_eq!(session.pending_prompts.len(), MAX_PENDING_PROMPTS);
        assert_eq!(session.pending_prompts[0], "p5");
    }
}
