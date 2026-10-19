//! `geleider import`: a one-off copy of Claude Desktop's session presets into
//! Profiles, and of each Project's Codex setup script into its Setup. It shows
//! what it found and writes geleider's config only after an explicit yes.
//!
//! The presets file is undocumented, so reading it is best effort. A setup
//! script runs outside any sandbox once imported, so it is shown in full and
//! refused if it holds characters a terminal would not show (ADR 0006).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::Config;
use crate::paths::{self, Paths};
use crate::text;
use crate::ui::Term;

/// A Profile found in Claude Desktop's presets.
#[derive(Debug, Clone, PartialEq)]
struct FoundProfile {
    name: String,
    model: Option<String>,
    effort: Option<String>,
    fast: bool,
    permission_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct FoundSetup {
    project: String,
    script: String,
}

pub fn run(term: &mut Term, paths: &Paths) -> Result<i32> {
    let existing = match std::fs::read_to_string(&paths.config) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", paths.config.display())),
    };
    let config =
        Config::parse(&existing).with_context(|| format!("in {}", paths.config.display()))?;
    let presets_file = presets_path()?;
    let (profiles, skipped) = match std::fs::read_to_string(&presets_file) {
        Ok(text) => presets(&text, &config),
        Err(_) => (Vec::new(), Vec::new()),
    };
    let (setups, refused) = setups(&config);

    let mut out = String::new();
    if !profiles.is_empty() || !skipped.is_empty() {
        out.push_str(&format!(
            "Profiles from Claude Desktop's session presets ({})\n",
            paths::display(&presets_file)
        ));
        for p in &profiles {
            out.push_str(&format!("  [profiles.{}]  {}\n", p.name, describe(p)));
        }
        for why in &skipped {
            out.push_str(&format!("  skipped {why}\n"));
        }
    }
    if !setups.is_empty() || !refused.is_empty() {
        out.push_str("Setup from each Project's .codex/environments/environment.toml\n");
        for s in &setups {
            out.push_str(&format!("  project {}: setup =\n", s.project));
            for line in s.script.lines() {
                out.push_str(&format!("      {line}\n"));
            }
        }
        for why in &refused {
            out.push_str(&format!("  skipped {why}\n"));
        }
    }
    if profiles.is_empty() && setups.is_empty() {
        out.push_str("Nothing to import.\n");
        term.say(&out);
        return Ok(0);
    }
    term.say(&out);
    if !term.confirm(&format!(
        "Write these to {}?",
        paths::display(&paths.config)
    ))? {
        term.say("Cancelled; nothing written.");
        return Ok(0);
    }
    let updated = write(&existing, &profiles, &setups)?;
    Config::parse(&updated).context("the imported config would not load")?;
    if let Some(parent) = paths.config.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&paths.config, updated)
        .with_context(|| format!("writing {}", paths.config.display()))?;
    term.say(&format!("Wrote {}.", paths::display(&paths.config)));
    Ok(0)
}

fn presets_path() -> Result<PathBuf> {
    Ok(paths::home_dir()?.join("Library/Application Support/Claude/ccd-session-presets.json"))
}

fn describe(p: &FoundProfile) -> String {
    let mut parts = vec!["harness = \"claude\"".to_string()];
    if let Some(model) = &p.model {
        parts.push(format!("model = {model:?}"));
    }
    if let Some(effort) = &p.effort {
        parts.push(format!("effort = {effort:?}"));
    }
    if p.fast {
        parts.push("fast = true".into());
    }
    if let Some(mode) = &p.permission_mode {
        parts.push(format!("permission_mode = {mode:?}"));
    }
    parts.join(", ")
}

/// Profiles from the presets JSON, skipping ones named like an existing
/// Profile, and why any were skipped.
fn presets(text: &str, config: &Config) -> (Vec<FoundProfile>, Vec<String>) {
    let mut found: Vec<FoundProfile> = Vec::new();
    let mut skipped = Vec::new();
    let Ok(json) = serde_json::from_str::<Value>(text) else {
        return (
            found,
            vec!["the presets file, which could not be read as JSON".into()],
        );
    };
    let accounts = json["byAccount"]
        .as_object()
        .into_iter()
        .flat_map(|a| a.values());
    for preset in accounts.flat_map(|a| a["presets"].as_array().into_iter().flatten()) {
        let Some(label) = preset["name"].as_str() else {
            continue;
        };
        let name = slug(label);
        if name.is_empty() {
            skipped.push(format!(
                "preset {:?}: no usable name",
                text::one_line(label)
            ));
            continue;
        }
        if config.profiles.contains_key(&name) || found.iter().any(|p| p.name == name) {
            skipped.push(format!(
                "preset {:?}: Profile `{name}` exists",
                text::one_line(label)
            ));
            continue;
        }
        let settings = &preset["settings"];
        let string = |key: &str| settings[key].as_str().map(str::to_string);
        let profile = FoundProfile {
            name,
            model: string("modelId"),
            effort: string("effort"),
            fast: settings["fastMode"] == true,
            permission_mode: string("permissionMode"),
        };
        // Only settings geleider accepts are imported; the check is the config's own.
        let candidate = write("", std::slice::from_ref(&profile), &[]).ok();
        match candidate.as_deref().map(Config::parse) {
            Some(Ok(_)) => found.push(profile),
            Some(Err(err)) => skipped.push(format!(
                "preset {:?}: {}",
                text::one_line(label),
                text::one_line(&format!("{err:#}"))
            )),
            None => {}
        }
    }
    (found, skipped)
}

/// `Opus · xhigh` → `opus-xhigh`.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Each Project without a Setup whose clone has a Codex setup script:
/// `[setup.darwin]`, else `[setup]`.
fn setups(config: &Config) -> (Vec<FoundSetup>, Vec<String>) {
    let mut found = Vec::new();
    let mut refused = Vec::new();
    for (name, project) in &config.projects {
        if project.setup.is_some() {
            continue;
        }
        let Ok(clone) = project.clone_path() else {
            continue;
        };
        let Some(script) = codex_setup(&clone) else {
            continue;
        };
        if text::has_hidden(&script) {
            refused.push(format!(
                "project {name}: its setup script holds characters a terminal does not show"
            ));
            continue;
        }
        found.push(FoundSetup {
            project: name.clone(),
            script,
        });
    }
    (found, refused)
}

fn codex_setup(clone: &Path) -> Option<String> {
    let text = std::fs::read_to_string(clone.join(".codex/environments/environment.toml")).ok()?;
    let table: toml::Table = text.parse().ok()?;
    let setup = table.get("setup")?;
    [
        setup.get("darwin").and_then(|d| d.get("script")),
        setup.get("script"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|s| s.as_str())
    .map(str::trim)
    .find(|s| !s.is_empty())
    .map(str::to_string)
}

/// `existing` with the found Profiles and Setups added, its formatting kept.
fn write(existing: &str, profiles: &[FoundProfile], setups: &[FoundSetup]) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = existing.parse()?;
    if !profiles.is_empty() && !doc.contains_key("profiles") {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc.insert("profiles", toml_edit::Item::Table(table));
    }
    for p in profiles {
        let mut table = toml_edit::Table::new();
        table.insert("harness", toml_edit::value("claude"));
        for (key, value) in [
            ("model", &p.model),
            ("effort", &p.effort),
            ("permission_mode", &p.permission_mode),
        ] {
            if let Some(value) = value {
                table.insert(key, toml_edit::value(value.as_str()));
            }
        }
        if p.fast {
            table.insert("fast", toml_edit::value(true));
        }
        doc["profiles"][p.name.as_str()] = toml_edit::Item::Table(table);
    }
    for s in setups {
        doc["projects"][s.project.as_str()]["setup"] = toml_edit::value(s.script.as_str());
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_become_claude_profiles_and_bad_or_taken_ones_are_skipped() {
        let config = Config::parse("[profiles.taken]\nharness = \"codex\"\n").unwrap();
        let json = r#"{"byAccount":{"a":{"presets":[
            {"name":"Opus · xhigh","settings":{"modelId":"claude-opus-5-5","effort":"xhigh","fastMode":true,"worktree":true}},
            {"name":"Taken","settings":{}},
            {"name":"Weird","settings":{"effort":"enormous"}},
            {"settings":{}}
        ]}}}"#;
        let (found, skipped) = presets(json, &config);
        assert_eq!(
            found,
            [FoundProfile {
                name: "opus-xhigh".into(),
                model: Some("claude-opus-5-5".into()),
                effort: Some("xhigh".into()),
                fast: true,
                permission_mode: None,
            }]
        );
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(presets("not json", &config).1[0].contains("JSON"));
    }

    #[test]
    fn codex_setup_prefers_darwin_and_writing_keeps_the_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".codex/environments");
        std::fs::create_dir_all(&env).unwrap();
        std::fs::write(
            env.join("environment.toml"),
            "version = 1\n[setup]\nscript = \"make\"\n[setup.darwin]\nscript = \"brew bundle\\nmake\"\n",
        )
        .unwrap();
        assert_eq!(
            codex_setup(dir.path()).as_deref(),
            Some("brew bundle\nmake")
        );

        let existing = format!(
            "# mine\nnotifications = false\n[projects.demo]\nrepo = \"o/r\"\npath = \"{}\"\n",
            dir.path().display()
        );
        let config = Config::parse(&existing).unwrap();
        let (found, refused) = setups(&config);
        assert!(refused.is_empty());
        let profile = FoundProfile {
            name: "fast".into(),
            model: None,
            effort: None,
            fast: true,
            permission_mode: None,
        };
        let written = write(&existing, &[profile], &found).unwrap();
        assert!(written.starts_with("# mine\n"), "{written}");
        let config = Config::parse(&written).unwrap();
        assert_eq!(
            config.projects["demo"].setup.as_deref(),
            Some("brew bundle\nmake")
        );
        assert!(config.profiles["fast"].fast);

        std::fs::write(
            env.join("environment.toml"),
            "[setup]\nscript = \"make\u{202e}\"\n",
        )
        .unwrap();
        let (found, refused) = setups(&Config::parse(&existing).unwrap());
        assert!(found.is_empty() && refused.len() == 1);
    }
}
