//! Lifecycle hook config merges; only commands installed by geleider are removed.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::install::Merge;
use crate::session::Harness;

/// Events both Claude Code and Codex report.
const COMMON: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
];

/// The lifecycle events geleider registers a command hook for.
fn events(harness: Harness) -> impl Iterator<Item = &'static str> {
    let extra: &[&str] = match harness {
        Harness::Claude => &["Notification", "PostToolUseFailure", "StopFailure"],
        Harness::Codex => &["Interrupt"],
        // OpenCode reports through its plugin instead.
        Harness::Opencode => &[],
    };
    COMMON.iter().chain(extra).copied()
}

pub fn claude_add(text: Option<&str>, exe: &Path) -> Result<Merge> {
    add(text, exe, Harness::Claude)
}

pub fn codex_add(text: Option<&str>, exe: &Path) -> Result<Merge> {
    add(text, exe, Harness::Codex)
}

pub fn remove(text: Option<&str>) -> Result<Merge> {
    let Some(text) = text else {
        return Ok(Merge::Unchanged);
    };
    let mut doc: Value = serde_json::from_str(text)?;
    let before = doc.clone();
    strip(&mut doc);
    if doc == before {
        return Ok(Merge::Unchanged);
    }
    Ok(Merge::Write(serde_json::to_string_pretty(&doc)? + "\n"))
}

fn ours(hook: &Value) -> bool {
    let Some(command) = hook["command"].as_str() else {
        return false;
    };
    let Ok(words) = shell_words::split(command) else {
        return false;
    };
    let words = if words
        .first()
        .is_some_and(|w| w == "GELEIDER_HARNESS_PID=$PPID")
    {
        &words[1..]
    } else {
        &words[..]
    };
    words.len() == 4
        && Path::new(&words[0])
            .file_name()
            .is_some_and(|n| n == "geleider")
        && words[1] == "hook"
        && matches!(words[2].as_str(), "claude" | "codex")
}

/// Removes geleider's hook commands, and any group, event or `hooks` table that
/// only they made non-empty. Everything else is left as it was.
fn strip(doc: &mut Value) {
    let Some(root) = doc.as_object_mut() else {
        return;
    };
    let Some(events) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    let had_events = !events.is_empty();
    events.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            return true;
        };
        let had_groups = !groups.is_empty();
        groups.retain_mut(|group| {
            let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let had_ours = hooks.iter().any(ours);
            hooks.retain(|h| !ours(h));
            !had_ours || !hooks.is_empty()
        });
        !had_groups || !groups.is_empty()
    });
    if had_events && events.is_empty() {
        root.remove("hooks");
    }
}

fn add(text: Option<&str>, exe: &Path, harness: Harness) -> Result<Merge> {
    let mut doc: Value = serde_json::from_str(text.unwrap_or("{}"))?;
    let original = doc.clone();
    strip(&mut doc);
    let hooks = doc
        .as_object_mut()
        .context("expected JSON object")?
        .entry("hooks")
        .or_insert(json!({}))
        .as_object_mut()
        .context("hooks is not an object")?;
    let exe = shell_words::quote(exe.to_str().context("non-UTF-8 executable path")?);
    for event in events(harness) {
        let groups = hooks
            .entry(event)
            .or_insert(json!([]))
            .as_array_mut()
            .context("hook event is not an array")?;
        let command = format!(
            "GELEIDER_HARNESS_PID=$PPID {exe} hook {} {event}",
            harness.as_str()
        );
        groups.push(json!({"hooks":[{"type":"command", "command":command, "timeout":1}]}));
    }
    if doc == original {
        return Ok(Merge::Unchanged);
    }
    Ok(Merge::Write(serde_json::to_string_pretty(&doc)? + "\n"))
}

const PLUGIN_MARKER: &str = "// Installed by geleider: Harness Session hooks v2\n";

pub fn opencode_add(text: Option<&str>, exe: &Path) -> Result<Merge> {
    if text.is_some_and(|t| !t.starts_with(PLUGIN_MARKER)) {
        return Ok(Merge::Foreign);
    }
    let plugin = include_str!("../plugins/opencode.ts").replace(
        "__GELEIDER_EXE__",
        &serde_json::to_string(exe.to_str().context("non-UTF-8 executable path")?)?,
    );
    if text == Some(&plugin) {
        Ok(Merge::Unchanged)
    } else {
        Ok(Merge::Write(plugin))
    }
}

pub fn opencode_remove(text: Option<&str>) -> Result<Merge> {
    match text {
        Some(t) if t.starts_with(PLUGIN_MARKER) => Ok(Merge::RemoveFile),
        Some(_) => Ok(Merge::Foreign),
        None => Ok(Merge::Unchanged),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(merge: Merge) -> String {
        match merge {
            Merge::Write(text) => text,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn merges_and_removes_only_owned_hooks() {
        let original = r#"{"model":"custom", "hooks":{"Stop":[{"matcher":"x","hooks":[{"type":"command","command":"notify-user"}]}]}}"#;
        let added =
            written(claude_add(Some(original), Path::new("/Applications/A B/geleider")).unwrap());
        let doc: Value = serde_json::from_str(&added).unwrap();
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"],
            "notify-user"
        );
        assert_eq!(
            claude_add(Some(&added), Path::new("/Applications/A B/geleider")).unwrap(),
            Merge::Unchanged
        );
        let removed = written(remove(Some(&added)).unwrap());
        assert_eq!(
            serde_json::from_str::<Value>(&removed).unwrap(),
            serde_json::from_str::<Value>(original).unwrap()
        );
        let moved = written(codex_add(Some(&added), Path::new("/new/geleider")).unwrap());
        assert!(!moved.contains("A B"));
        assert!(moved.contains("notify-user"));
    }

    #[test]
    fn uninstall_preserves_foreign_handlers_inside_an_owned_group() {
        let added = written(codex_add(None, Path::new("/bin/geleider")).unwrap());
        let mut doc: Value = serde_json::from_str(&added).unwrap();
        doc["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"command","command":"my-stop-hook"}));
        let removed = written(remove(Some(&doc.to_string())).unwrap());
        assert!(removed.contains("my-stop-hook"));
        assert!(!removed.contains("geleider hook"));
    }
}
