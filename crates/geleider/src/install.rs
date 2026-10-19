//! `geleider install` / `uninstall`: the launchd login service for the background
//! process, the MCP server entry in each Harness's config, and Claude Code's status
//! line. Only geleider's own entries are ever added, changed or removed; a status
//! line the user already had is wrapped, not replaced.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hooks_install;
use crate::paths::{self, Paths};
use crate::ui::Term;

const LABEL: &str = "com.github.eloyye.geleider";
const SERVER: &str = "geleider";
/// Claude Code's user settings, under the home directory; its `statusLine` lives here.
const CLAUDE_SETTINGS: &str = ".claude/settings.json";
/// Added to a config file's name for its [`InstallJournal`].
const JOURNAL_SUFFIX: &str = ".geleider-install";

/// The result of merging geleider's entry into a config file.
#[derive(Debug, PartialEq, Eq)]
pub enum Merge {
    Write(String),
    Unchanged,
    RemoveFile,
    /// Uninstall put back the file exactly as it was before install.
    Restored,
    /// A `geleider` entry exists that geleider did not write; it is left alone.
    Foreign,
}

pub fn install(term: &mut Term, paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        term.say_err(&format!(
            "note: registering {} from a build directory; `cargo install --path crates/geleider` \
             gives a stable path.",
            exe.display()
        ));
    }
    let home = paths::home_dir()?;

    write_starter_config(term, &paths.config)?;
    install_service(term, &home, &exe)?;
    for integration in integrations(&home) {
        let outcome = integration.apply(false, |text| (integration.add)(text, &exe));
        if outcome.is_ok()
            && integration.path.ends_with(CLAUDE_SETTINGS)
            && status_line_is_foreign(&integration.path, &exe)
        {
            term.say_err(
                "Claude Code: existing statusLine cannot be wrapped; hooks installed, status line \
                 left alone",
            );
        }
        integration.report(term, outcome, false);
    }
    term.say(
        "Codex: review and trust the installed hooks in /hooks before relying on Session tracking.",
    );
    term.say(&format!(
        "\nDeclare Projects, Resources and Jobs in {} — Jobs come only from that file.",
        paths::display(&paths.config)
    ));
    Ok(())
}

pub fn uninstall(term: &mut Term) -> Result<()> {
    let home = paths::home_dir()?;
    for integration in integrations(&home) {
        if integration.path.exists() {
            let outcome = integration.apply(true, integration.remove);
            integration.report(term, outcome, true);
        }
    }
    let plist = plist_path(&home);
    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    if plist.exists() {
        std::fs::remove_file(&plist)?;
    }
    term.say(&format!(
        "background service: removed {}",
        paths::display(&plist)
    ));
    term.say("Config and Run history are kept; delete them by hand if you want them gone.");
    Ok(())
}

/// Restarts the installed background process, which launchd then starts from the
/// binary its plist names. Returns that binary.
pub fn restart_service() -> Result<String> {
    let plist = plist_path(&paths::home_dir()?);
    let text = std::fs::read_to_string(&plist).map_err(|err| {
        anyhow::anyhow!(
            "the background service is not installed ({}: {err}). Run `geleider install`, \
             or restart `geleider daemon` by hand if you run it in a terminal.",
            paths::display(&plist)
        )
    })?;
    launchctl(&["kickstart", "-k", &format!("{}/{LABEL}", domain())])
        .context("restarting the background service; `geleider install` reloads it")?;
    service_exe(&text).context("reading the binary from the service's plist")
}

/// The binary in a plist [`plist_xml`] wrote.
fn service_exe(plist: &str) -> Option<String> {
    let args = &plist[plist.find("<key>ProgramArguments</key>")?..];
    let start = args.find("<string>")? + "<string>".len();
    let end = start + args[start..].find("</string>")?;
    Some(
        args[start..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

/// What a config file held before geleider first changed it, and what geleider
/// last wrote there. Uninstall puts back a file the user has not edited since,
/// byte for byte; an edited one only loses geleider's entries.
#[derive(Serialize, Deserialize)]
struct InstallJournal {
    original: Option<String>,
    installed: String,
}

impl InstallJournal {
    fn path(config: &Path) -> PathBuf {
        sibling(config, JOURNAL_SUFFIX)
    }

    fn load(config: &Path) -> Result<Option<Self>> {
        read_optional(&Self::path(config))?
            .map(|text| serde_json::from_str(&text).context("reading geleider install journal"))
            .transpose()
    }

    /// Written before the config, so an interrupted install can still be undone.
    fn save(&self, config: &Path) -> Result<()> {
        write_atomically(&Self::path(config), &serde_json::to_string(self)?, false)
    }

    fn discard(config: &Path) -> Result<()> {
        remove_if_present(&Self::path(config))
    }
}

/// A config file geleider adds its entries to: an MCP server, hooks or a plugin.
struct Integration {
    name: &'static str,
    path: PathBuf,
    /// Whether the Harness looks installed, so its config should be touched.
    present: bool,
    add: fn(Option<&str>, &Path) -> Result<Merge>,
    remove: fn(Option<&str>) -> Result<Merge>,
}

impl Integration {
    fn apply(
        &self,
        removing: bool,
        merge: impl FnOnce(Option<&str>) -> Result<Merge>,
    ) -> Result<Option<Merge>> {
        if !self.present {
            return Ok(None);
        }
        let path = &self.path;
        let existing = read_optional(path)?;
        let journal = InstallJournal::load(path)?;
        let untouched = |j: &InstallJournal| existing.as_deref() == Some(j.installed.as_str());

        if removing && let Some(journal) = journal.as_ref().filter(|j| untouched(j)) {
            match &journal.original {
                Some(original) => write_atomically(path, original, false)?,
                None => std::fs::remove_file(path)?,
            }
            InstallJournal::discard(path)?;
            return Ok(Some(Merge::Restored));
        }

        let merged =
            merge(existing.as_deref()).with_context(|| format!("updating {}", path.display()))?;
        match &merged {
            Merge::Write(text) if !removing => {
                let original = match &journal {
                    None => existing.clone(),
                    Some(j) if untouched(j) => j.original.clone(),
                    // Edited since install: what it would be without geleider.
                    Some(_) => match (self.remove)(existing.as_deref())? {
                        Merge::Write(cleaned) => Some(cleaned),
                        Merge::RemoveFile => None,
                        _ => existing.clone(),
                    },
                };
                let installed = text.clone();
                InstallJournal {
                    original,
                    installed,
                }
                .save(path)?;
                write_atomically(path, text, existing.is_some())?;
            }
            Merge::Write(text) => {
                write_atomically(path, text, existing.is_some())?;
                InstallJournal::discard(path)?;
            }
            Merge::RemoveFile => {
                std::fs::remove_file(path)?;
                InstallJournal::discard(path)?;
            }
            Merge::Unchanged | Merge::Foreign | Merge::Restored => {}
        }
        Ok(Some(merged))
    }

    fn report(&self, term: &mut Term, outcome: Result<Option<Merge>>, removing: bool) {
        let said = if self.path.ends_with(CLAUDE_SETTINGS) {
            report_status_line(&self.path, outcome, removing)
        } else {
            let verb = if removing { "removed" } else { "registered" };
            Some(report(self.name, &self.path, outcome, verb))
        };
        if let Some(said) = said {
            term.say(&said);
        }
    }
}

/// The file's text, or `None` if there is no file.
fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

fn integrations(home: &Path) -> Vec<Integration> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let opencode_dir = home.join(".config/opencode");
    let opencode = ["opencode.jsonc", "opencode.json"]
        .iter()
        .map(|f| opencode_dir.join(f))
        .find(|p| p.exists())
        .unwrap_or_else(|| opencode_dir.join("opencode.json"));
    vec![
        Integration {
            name: "Claude Code",
            path: home.join(".claude.json"),
            present: home.join(".claude.json").exists() || home.join(".claude").is_dir(),
            add: claude_add,
            remove: claude_remove,
        },
        Integration {
            name: "Codex",
            path: codex_home.join("config.toml"),
            present: codex_home.is_dir(),
            add: codex_add,
            remove: codex_remove,
        },
        Integration {
            name: "OpenCode",
            path: opencode.clone(),
            present: opencode_dir.is_dir(),
            add: opencode_add,
            remove: opencode_remove,
        },
        Integration {
            name: "Claude Code hooks",
            path: home.join(CLAUDE_SETTINGS),
            present: home.join(".claude").is_dir() || home.join(".claude.json").exists(),
            add: claude_settings_add,
            remove: claude_settings_remove,
        },
        Integration {
            name: "Codex hooks",
            path: codex_home.join("hooks.json"),
            present: codex_home.is_dir(),
            add: hooks_install::codex_add,
            remove: hooks_install::remove,
        },
        Integration {
            name: "OpenCode hooks",
            path: opencode_dir.join("plugins/geleider.ts"),
            present: opencode_dir.is_dir(),
            add: hooks_install::opencode_add,
            remove: hooks_install::opencode_remove,
        },
    ]
}

/// What install or uninstall did to Claude Code's settings, if worth saying.
fn report_status_line(
    path: &Path,
    outcome: Result<Option<Merge>>,
    removing: bool,
) -> Option<String> {
    let path = paths::display(path);
    let said = match outcome {
        Ok(None) => return None,
        Ok(Some(Merge::Write(text))) => {
            let wrapping = !removing && wraps_a_status_line(&text);
            format!(
                "Claude Code: hooks and status line {} in {path}",
                match (removing, wrapping) {
                    (true, _) => "restored",
                    (false, true) => "set, wrapping your existing one",
                    (false, false) => "set",
                }
            )
        }
        Ok(Some(Merge::RemoveFile)) => {
            format!("Claude Code: hooks and status line removed in {path}")
        }
        Ok(Some(Merge::Restored)) => {
            format!("Claude Code: {path} restored to its state before install")
        }
        Ok(Some(Merge::Unchanged)) if !removing => {
            format!("Claude Code: hooks and status line in {path} already up to date")
        }
        Ok(Some(Merge::Unchanged)) => return None,
        Ok(Some(Merge::Foreign)) => format!(
            "Claude Code: {path} has a `statusLine` geleider cannot wrap; left it alone, so \
             Claude usage will not update"
        ),
        Err(err) => format!("Claude Code: {err:#}"),
    };
    Some(said)
}

/// What install or uninstall did to one Harness's config file.
fn report(name: &str, path: &Path, outcome: Result<Option<Merge>>, verb: &str) -> String {
    let path = paths::display(path);
    match outcome {
        Ok(None) => format!("{name}: not installed, skipped"),
        Ok(Some(Merge::Write(_) | Merge::RemoveFile)) => {
            format!("{name}: integration {verb} in {path}")
        }
        Ok(Some(Merge::Restored)) => {
            format!("{name}: {path} restored to its state before install")
        }
        Ok(Some(Merge::Unchanged)) => format!("{name}: {path} already up to date"),
        Ok(Some(Merge::Foreign)) => {
            format!("{name}: {path} has an entry geleider did not write; left it alone")
        }
        Err(err) => format!("{name}: {err:#}"),
    }
}

fn is_ours(command: Option<&str>, args: &[&str]) -> bool {
    let named_geleider = command
        .and_then(|c| Path::new(c).file_name())
        .is_some_and(|n| n == SERVER);
    named_geleider && args == ["mcp"]
}

fn exe_str(exe: &Path) -> Result<&str> {
    exe.to_str().context("geleider's path is not valid UTF-8")
}

// Claude Code: user-scope servers live under `mcpServers` in ~/.claude.json.

fn claude_add(existing: Option<&str>, exe: &Path) -> Result<Merge> {
    let entry = json!({ "type": "stdio", "command": exe_str(exe)?, "args": ["mcp"], "env": {} });
    json_set(existing, &["mcpServers"], entry, claude_is_ours)
}

fn claude_remove(existing: Option<&str>) -> Result<Merge> {
    json_remove(existing, &["mcpServers"], claude_is_ours)
}

fn claude_is_ours(entry: &Value) -> bool {
    is_ours(entry["command"].as_str(), &strings(&entry["args"]))
}

/// The strings in a JSON array; nothing if it is not one.
fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// A JSON config file's new text, formatted the way the Harnesses write it.
fn json_write(doc: &Value) -> Result<Merge> {
    Ok(Merge::Write(serde_json::to_string_pretty(doc)? + "\n"))
}

// OpenCode: servers live under `mcp`, with the command as one array.

fn opencode_add(existing: Option<&str>, exe: &Path) -> Result<Merge> {
    let entry = json!({ "type": "local", "command": [exe_str(exe)?, "mcp"], "enabled": true });
    let existing = existing
        .map(str::to_string)
        .or_else(|| Some(json!({ "$schema": "https://opencode.ai/config.json" }).to_string()));
    json_set(existing.as_deref(), &["mcp"], entry, opencode_is_ours)
}

fn opencode_remove(existing: Option<&str>) -> Result<Merge> {
    json_remove(existing, &["mcp"], opencode_is_ours)
}

fn opencode_is_ours(entry: &Value) -> bool {
    match strings(&entry["command"]).split_first() {
        Some((program, args)) => is_ours(Some(program), args),
        None => false,
    }
}

fn parse_json(existing: Option<&str>) -> Result<Value> {
    match existing {
        None => Ok(json!({})),
        Some(text) if text.trim().is_empty() => Ok(json!({})),
        Some(text) => serde_json::from_str(text).context(
            "not plain JSON (comments are not supported); add geleider's MCP server by hand",
        ),
    }
}

fn json_set(
    existing: Option<&str>,
    table: &[&str],
    entry: Value,
    ours: fn(&Value) -> bool,
) -> Result<Merge> {
    let mut doc = parse_json(existing)?;
    let mut node = &mut doc;
    for key in table {
        let object = node.as_object_mut().context("expected a JSON object")?;
        node = object.entry(*key).or_insert_with(|| json!({}));
    }
    let servers = node
        .as_object_mut()
        .with_context(|| format!("`{}` is not an object", table.join(".")))?;
    match servers.get(SERVER) {
        Some(current) if *current == entry => return Ok(Merge::Unchanged),
        Some(current) if !ours(current) => return Ok(Merge::Foreign),
        _ => {}
    }
    servers.insert(SERVER.to_string(), entry);
    json_write(&doc)
}

fn json_remove(existing: Option<&str>, table: &[&str], ours: fn(&Value) -> bool) -> Result<Merge> {
    let Some(existing) = existing else {
        return Ok(Merge::Unchanged);
    };
    let mut doc = parse_json(Some(existing))?;
    let mut node = &mut doc;
    for key in table {
        match node.get_mut(*key) {
            Some(next) => node = next,
            None => return Ok(Merge::Unchanged),
        }
    }
    let Some(servers) = node.as_object_mut() else {
        return Ok(Merge::Unchanged);
    };
    match servers.get(SERVER) {
        None => Ok(Merge::Unchanged),
        Some(current) if !ours(current) => Ok(Merge::Foreign),
        Some(_) => {
            servers.remove(SERVER);
            json_write(&doc)
        }
    }
}

// Codex: servers live under `[mcp_servers.<name>]` in config.toml. toml_edit keeps
// the user's comments and layout.

/// Codex asks before every MCP tool call unless told otherwise, and `codex exec`
/// cannot ask, so geleider's tools are pre-approved. They can only queue Jobs the
/// user declared in geleider's own config, and the Session's sandbox is unchanged.
const CODEX_APPROVAL: &str = "default_tools_approval_mode";

fn codex_add(existing: Option<&str>, exe: &Path) -> Result<Merge> {
    let mut doc: toml_edit::DocumentMut = existing.unwrap_or_default().parse()?;
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_like_mut()
        .context("`mcp_servers` is not a table")?;
    if let Some(current) = servers.get_mut(SERVER) {
        if !codex_is_ours(current) {
            return Ok(Merge::Foreign);
        }
        // Update the path in place, keeping anything the user added to the entry,
        // including their own approval mode.
        let mut changed = false;
        if current.get("command").and_then(|c| c.as_str()) != Some(exe_str(exe)?) {
            current["command"] = toml_edit::value(exe_str(exe)?);
            changed = true;
        }
        if current.get(CODEX_APPROVAL).is_none() {
            current[CODEX_APPROVAL] = toml_edit::value("approve");
            changed = true;
        }
        return Ok(if changed {
            Merge::Write(doc.to_string())
        } else {
            Merge::Unchanged
        });
    }
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    let mut table = toml_edit::Table::new();
    table.insert("command", toml_edit::value(exe_str(exe)?));
    table.insert("args", toml_edit::value(args));
    table.insert(CODEX_APPROVAL, toml_edit::value("approve"));
    servers.insert(SERVER, toml_edit::Item::Table(table));
    Ok(Merge::Write(doc.to_string()))
}

fn codex_remove(existing: Option<&str>) -> Result<Merge> {
    let Some(existing) = existing else {
        return Ok(Merge::Unchanged);
    };
    let mut doc: toml_edit::DocumentMut = existing.parse()?;
    let Some(servers) = doc
        .get_mut("mcp_servers")
        .and_then(|s| s.as_table_like_mut())
    else {
        return Ok(Merge::Unchanged);
    };
    match servers.get(SERVER) {
        None => Ok(Merge::Unchanged),
        Some(current) if !codex_is_ours(current) => Ok(Merge::Foreign),
        Some(_) => {
            servers.remove(SERVER);
            Ok(Merge::Write(doc.to_string()))
        }
    }
}

fn codex_is_ours(entry: &toml_edit::Item) -> bool {
    let command = entry.get("command").and_then(|c| c.as_str());
    let args: Vec<&str> = entry
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    is_ours(command, &args)
}

// Hooks and the status line share one settings file and install journal.
fn claude_settings_add(existing: Option<&str>, exe: &Path) -> Result<Merge> {
    let hooks = hooks_install::claude_add(existing, exe)?;
    let text = match &hooks {
        Merge::Write(text) => Some(text.as_str()),
        _ => existing,
    };
    match statusline_add(text, &status_line_command(exe)?)? {
        Merge::Unchanged => Ok(hooks),
        // `install` says so; see [`status_line_is_foreign`].
        Merge::Foreign => Ok(hooks),
        merged => Ok(merged),
    }
}

/// Whether the settings at `path` have a status line geleider cannot wrap, which
/// [`claude_settings_add`] leaves alone.
fn status_line_is_foreign(path: &Path, exe: &Path) -> bool {
    let Ok(Some(text)) = read_optional(path) else {
        return false;
    };
    status_line_command(exe)
        .and_then(|ours| statusline_add(Some(&text), &ours))
        .is_ok_and(|merged| merged == Merge::Foreign)
}

fn claude_settings_remove(existing: Option<&str>) -> Result<Merge> {
    let hooks = hooks_install::remove(existing)?;
    let text = match &hooks {
        Merge::Write(text) => Some(text.as_str()),
        _ => existing,
    };
    match statusline_remove(text)? {
        Merge::Unchanged => Ok(hooks),
        merged => Ok(merged),
    }
}

// Claude Code's status line: `statusLine` in ~/.claude/settings.json. It is the only
// place Claude Code reports a Subscription's usage limits. An existing status line
// is kept as `--wrap <its command>`, so it still prints exactly what it did and
// `uninstall` can put it back.

/// The command Claude Code runs. The env overrides travel with it, since Claude
/// Code starts it with its own environment.
fn status_line_command(exe: &Path) -> Result<String> {
    let env = ["GELEIDER_CONFIG", "GELEIDER_STATE_DIR"]
        .into_iter()
        .filter_map(|key| Some((key, std::env::var(key).ok()?)));
    Ok(command_with_env(env, exe_str(exe)?))
}

/// e.g. `GELEIDER_STATE_DIR='/a b' /usr/local/bin/geleider statusline`. Only the
/// value is quoted, so the shell still sees an assignment.
fn command_with_env<'a>(env: impl IntoIterator<Item = (&'a str, String)>, exe: &str) -> String {
    let mut words: Vec<String> = env
        .into_iter()
        .map(|(key, value)| format!("{key}={}", shell_words::quote(&value)))
        .collect();
    words.push(shell_words::quote(exe).into_owned());
    words.push("statusline".into());
    words.join(" ")
}

/// For a command geleider wrote, the status line it wraps, if any.
fn our_status_line(command: &str) -> Option<Option<String>> {
    let words = shell_words::split(command).ok()?;
    let mut words = words
        .into_iter()
        .skip_while(|w| w.split_once('=').is_some_and(|(k, _)| !k.contains('/')));
    let program = words.next()?;
    if Path::new(&program).file_name()? != SERVER || words.next()? != "statusline" {
        return None;
    }
    match (words.next(), words.next(), words.next()) {
        (None, _, _) => Some(None),
        (Some(flag), Some(wrapped), None) if flag == "--wrap" => Some(Some(wrapped)),
        _ => None,
    }
}

/// Whether the settings `text` geleider wrote keep a status line of the user's.
fn wraps_a_status_line(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|doc| our_status_line(doc["statusLine"]["command"].as_str()?))
        .is_some_and(|wrapped| wrapped.is_some())
}

fn statusline_add(existing: Option<&str>, ours: &str) -> Result<Merge> {
    let mut doc = parse_json(existing)?;
    let settings = doc
        .as_object_mut()
        .context("settings are not a JSON object")?;
    let current = settings.get("statusLine").cloned();
    let entry = match current {
        None | Some(Value::Null) => json!({ "type": "command", "command": ours }),
        Some(Value::Object(mut entry)) => {
            let Some(command) = entry.get("command").and_then(Value::as_str) else {
                return Ok(Merge::Foreign);
            };
            let wrapped = match our_status_line(command) {
                Some(wrapped) => wrapped,
                None => Some(command.to_string()),
            };
            let command = match wrapped {
                Some(wrapped) => format!("{ours} --wrap {}", shell_words::quote(&wrapped)),
                None => ours.to_string(),
            };
            entry.insert("command".into(), Value::String(command));
            Value::Object(entry)
        }
        Some(_) => return Ok(Merge::Foreign),
    };
    if settings.get("statusLine") == Some(&entry) {
        return Ok(Merge::Unchanged);
    }
    settings.insert("statusLine".into(), entry);
    json_write(&doc)
}

fn statusline_remove(existing: Option<&str>) -> Result<Merge> {
    let Some(existing) = existing else {
        return Ok(Merge::Unchanged);
    };
    let mut doc = parse_json(Some(existing))?;
    let Some(settings) = doc.as_object_mut() else {
        return Ok(Merge::Unchanged);
    };
    let Some(Value::Object(entry)) = settings.get_mut("statusLine") else {
        return Ok(Merge::Unchanged);
    };
    let wrapped = match entry
        .get("command")
        .and_then(Value::as_str)
        .and_then(our_status_line)
    {
        None => return Ok(Merge::Unchanged),
        Some(wrapped) => wrapped,
    };
    match wrapped {
        Some(command) => {
            entry.insert("command".into(), Value::String(command));
        }
        None => {
            settings.remove("statusLine");
        }
    }
    json_write(&doc)
}

/// Replaces `path` with `text` in one step, keeping a backup of the old file.
/// These files can hold credentials (`~/.claude.json` can hold an API key), so
/// the new file and the backup are created with the original's permissions, or
/// owner-only for a new file, never briefly readable by others. A leftover temp
/// or backup file, or a symlink planted at its name, is removed rather than
/// written through.
fn write_atomically(path: &Path, text: &str, keep_backup: bool) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A journal copies a config that can hold credentials; it is never shared.
    let mode = if path.as_os_str().to_string_lossy().ends_with(JOURNAL_SUFFIX) {
        0o600
    } else {
        std::fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o777)
    };
    if keep_backup {
        let original = std::fs::read(path)?;
        write_new(&sibling(path, ".geleider-backup"), &original, mode)?;
    }
    let temp = sibling(path, ".geleider-tmp");
    write_new(&temp, text.as_bytes(), mode)?;
    std::fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))
}

/// `path` with `suffix` added to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Writes a fresh file with `mode`, replacing whatever was at `path`.
fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    remove_if_present(path)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

// launchd

fn plist_path(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn domain() -> String {
    // The standard library has no way to ask for the user id, and `getuid` is the
    // smallest unsafe block that does.
    // SAFETY: getuid takes no arguments, has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .context("running launchctl")?;
    if !output.status.success() {
        bail!(
            "launchctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn install_service(term: &mut Term, home: &Path, exe: &Path) -> Result<()> {
    let logs = home.join("Library/Logs/geleider");
    std::fs::create_dir_all(&logs)?;
    // launchd starts services with a bare PATH; Jobs and `gh` need the user's.
    let mut env = vec![(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    )];
    for key in ["GELEIDER_CONFIG", "GELEIDER_STATE_DIR", "CODEX_HOME"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    let plist = plist_path(home);
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&plist, plist_xml(exe, &env, &logs.join("daemon.log")))?;

    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    launchctl(&[
        "bootstrap",
        &domain(),
        plist.to_str().context("non-UTF-8 path")?,
    ])?;
    term.say(&format!(
        "background service: started at login via {}",
        paths::display(&plist)
    ));
    Ok(())
}

fn plist_xml(exe: &Path, env: &[(String, String)], log: &Path) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let env: String = env
        .iter()
        .map(|(k, v)| {
            format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                esc(k),
                esc(v)
            )
        })
        .collect();
    let exe = esc(&exe.display().to_string());
    let log = esc(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>daemon</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>EnvironmentVariables</key>
	<dict>
{env}	</dict>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    )
}

const STARTER_CONFIG: &str = r#"# geleider config. Jobs are declared only here, never in a Project's repo.
#
# notifications = true      # macOS notifications when Runs start after waiting or finish
# pr_comments = true        # post each Run's result on the branch's open PR
#
# [subscriptions]
# notify_resets = 80        # notify when a Limit window resets after reaching this % used; false for never
# [subscriptions.claude]    # from Claude Code's status line, in CLI sessions only
# plan = "Claude Pro"
# [subscriptions.chatgpt]   # from Codex's session logs; shared with OpenCode's OpenAI login
# plan = "ChatGPT Plus"
# enabled = true
#
# [resources.unity]
# slots = 1                 # how many processes may use it at once
# process = "Unity"         # executable name; any running process with it holds a Lease
#
# [projects.lanternfall]
# repo = "owner/lanternfall"
# path = "~/code/lanternfall"  # the clone; its worktrees belong to the Project too
# vars = { unity = "/Applications/Unity/Hub/Editor/6000.0.23f1/Unity.app/Contents/MacOS/Unity" }
#
# [projects.lanternfall.jobs.playmode-test]
# resource = "unity"
# args = ["suite"]          # geleider run playmode-test <suite>
# # Split like a shell would, but never run through one. {worktree} is built in.
# command = "scripts/run-playmode.sh {suite} {unity}"
"#;

fn write_starter_config(term: &mut Term, path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().context("config path has no parent")?)?;
    std::fs::write(path, STARTER_CONFIG)?;
    term.say(&format!(
        "config: wrote a commented starter at {}",
        paths::display(path)
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/usr/local/bin/geleider";

    fn written(merge: Merge) -> String {
        match merge {
            Merge::Write(text) => text,
            other => panic!("expected a write, got {other:?}"),
        }
    }

    #[test]
    fn claude_keeps_other_servers_and_settings() {
        let existing = r#"{"numStartups": 3, "mcpServers": {"github": {"command": "gh-mcp"}}}"#;
        let text = written(claude_add(Some(existing), Path::new(EXE)).unwrap());
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["numStartups"], 3);
        assert_eq!(doc["mcpServers"]["github"]["command"], "gh-mcp");
        assert_eq!(doc["mcpServers"]["geleider"]["args"], json!(["mcp"]));
        // Keys keep their order, so the diff stays small.
        assert!(text.find("numStartups").unwrap() < text.find("mcpServers").unwrap());

        assert_eq!(
            claude_add(Some(&text), Path::new(EXE)).unwrap(),
            Merge::Unchanged
        );
        let moved = written(claude_add(Some(&text), Path::new("/opt/geleider")).unwrap());
        assert!(moved.contains("/opt/geleider"));

        let removed: Value =
            serde_json::from_str(&written(claude_remove(Some(&text)).unwrap())).unwrap();
        assert!(removed["mcpServers"].get("geleider").is_none());
        assert_eq!(removed["mcpServers"]["github"]["command"], "gh-mcp");
    }

    #[test]
    fn a_users_own_geleider_entry_is_never_touched() {
        let existing =
            r#"{"mcpServers": {"geleider": {"command": "node", "args": ["my-server.js"]}}}"#;
        assert_eq!(
            claude_add(Some(existing), Path::new(EXE)).unwrap(),
            Merge::Foreign
        );
        assert_eq!(claude_remove(Some(existing)).unwrap(), Merge::Foreign);
    }

    #[test]
    fn codex_keeps_comments_and_other_servers() {
        let existing =
            "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n";
        let text = written(codex_add(Some(existing), Path::new(EXE)).unwrap());
        assert!(text.starts_with("# my settings\nmodel = \"gpt-5\""));
        assert!(text.contains("[mcp_servers.docs]"));
        assert!(text.contains(&format!(
            "[mcp_servers.geleider]\ncommand = \"{EXE}\"\nargs = [\"mcp\"]\n\
             default_tools_approval_mode = \"approve\""
        )));
        assert_eq!(
            codex_add(Some(&text), Path::new(EXE)).unwrap(),
            Merge::Unchanged
        );

        let removed = written(codex_remove(Some(&text)).unwrap());
        assert!(!removed.contains("geleider"));
        assert!(removed.contains("[mcp_servers.docs]"));

        let fresh = written(codex_add(None, Path::new(EXE)).unwrap());
        assert!(fresh.contains("[mcp_servers.geleider]"));

        // An older entry gains the approval mode; one the user changed keeps theirs.
        let older = "[mcp_servers.geleider]\ncommand = \"/old/geleider\"\nargs = [\"mcp\"]\n";
        let updated = written(codex_add(Some(older), Path::new(EXE)).unwrap());
        assert!(
            updated.contains(&format!("command = \"{EXE}\"")),
            "{updated}"
        );
        assert!(
            updated.contains("default_tools_approval_mode = \"approve\""),
            "{updated}"
        );
        let prompting =
            format!("{older}default_tools_approval_mode = \"prompt\"\nstartup_timeout_sec = 30\n");
        let kept = written(codex_add(Some(&prompting), Path::new(EXE)).unwrap());
        assert!(
            kept.contains("default_tools_approval_mode = \"prompt\""),
            "{kept}"
        );
        assert!(kept.contains("startup_timeout_sec = 30"), "{kept}");

        let foreign = "[mcp_servers.geleider]\ncommand = \"python\"\nargs = [\"x.py\"]\n";
        assert_eq!(
            codex_add(Some(foreign), Path::new(EXE)).unwrap(),
            Merge::Foreign
        );
    }

    #[test]
    fn opencode_entry_uses_a_command_array() {
        let text = written(opencode_add(None, Path::new(EXE)).unwrap());
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["mcp"]["geleider"]["command"], json!([EXE, "mcp"]));
        assert_eq!(doc["$schema"], "https://opencode.ai/config.json");
        assert!(opencode_add(Some("{ // comment\n}"), Path::new(EXE)).is_err());
        assert!(written(opencode_remove(Some(&text)).unwrap()).contains("\"mcp\": {}"));
    }

    #[test]
    fn config_round_trips_exactly_and_keeps_later_user_edits() {
        let dir = tempfile::tempdir().unwrap();
        for (name, original, add, remove) in [
            (
                "claude.json",
                Some("{  \"theme\": \"dark\" }\n"),
                claude_add as fn(Option<&str>, &Path) -> Result<Merge>,
                claude_remove as fn(Option<&str>) -> Result<Merge>,
            ),
            (
                "config.toml",
                Some("# personal\nmodel = \"custom\"\n"),
                codex_add,
                codex_remove,
            ),
            (
                "settings.json",
                Some(
                    "{\n  \"hooks\": {}, \"theme\": \"dark\",\n  \"statusLine\": {\"type\": \"command\", \"command\": \"echo mine\", \"padding\": 2}\n}\n",
                ),
                claude_settings_add,
                claude_settings_remove,
            ),
            (
                "hooks.json",
                None,
                hooks_install::codex_add,
                hooks_install::remove,
            ),
            (
                "geleider.ts",
                None,
                hooks_install::opencode_add,
                hooks_install::opencode_remove,
            ),
        ] {
            let integration = Integration {
                name: "test",
                path: dir.path().join(name),
                present: true,
                add,
                remove,
            };
            if let Some(original) = original {
                std::fs::write(&integration.path, original).unwrap();
            }
            integration
                .apply(false, |s| add(s, Path::new(EXE)))
                .unwrap();
            if name == "settings.json" {
                let installed: Value =
                    serde_json::from_str(&std::fs::read_to_string(&integration.path).unwrap())
                        .unwrap();
                assert!(installed["hooks"]["SessionStart"].is_array());
                assert_eq!(
                    our_status_line(installed["statusLine"]["command"].as_str().unwrap()),
                    Some(Some("echo mine".into()))
                );
                assert_eq!(installed["statusLine"]["padding"], 2);
            }
            integration
                .apply(false, |s| add(s, Path::new("/new/geleider")))
                .unwrap();
            integration.apply(true, remove).unwrap();
            assert_eq!(
                std::fs::read_to_string(&integration.path).ok().as_deref(),
                original,
                "{name}"
            );
        }
        let integration = Integration {
            name: "test",
            path: dir.path().join("changed.json"),
            present: true,
            add: claude_settings_add,
            remove: claude_settings_remove,
        };
        std::fs::write(&integration.path, "{}").unwrap();
        integration
            .apply(false, |s| (integration.add)(s, Path::new(EXE)))
            .unwrap();
        let mut doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&integration.path).unwrap()).unwrap();
        doc["theme"] = json!("light");
        std::fs::write(&integration.path, doc.to_string()).unwrap();
        integration
            .apply(false, |s| {
                (integration.add)(s, Path::new("/updated/geleider"))
            })
            .unwrap();
        integration.apply(true, integration.remove).unwrap();
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&integration.path).unwrap()).unwrap();
        assert_eq!(doc["theme"], "light");
        assert!(doc.get("hooks").is_none());
        assert!(doc.get("statusLine").is_none());
    }

    #[test]
    fn status_line_is_set_and_removed() {
        let ours = format!("{EXE} statusline");
        let text = written(statusline_add(Some(r#"{"model": "opus"}"#), &ours).unwrap());
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["model"], "opus");
        assert_eq!(
            doc["statusLine"],
            json!({ "type": "command", "command": ours })
        );
        assert_eq!(
            statusline_add(Some(&text), &ours).unwrap(),
            Merge::Unchanged
        );

        let removed: Value =
            serde_json::from_str(&written(statusline_remove(Some(&text)).unwrap())).unwrap();
        assert_eq!(removed, json!({ "model": "opus" }));
        // No settings file yet.
        assert!(written(statusline_add(None, &ours).unwrap()).contains(&ours));
    }

    #[test]
    fn an_existing_status_line_is_wrapped_and_restored() {
        let ours = format!("{EXE} statusline");
        let mine = r#"~/bin/my line.sh --color "a b" | tr a-z A-Z"#;
        let existing =
            json!({ "statusLine": { "type": "command", "command": mine, "padding": 2 } });
        let text = written(statusline_add(Some(&existing.to_string()), &ours).unwrap());
        let doc: Value = serde_json::from_str(&text).unwrap();
        let command = doc["statusLine"]["command"].as_str().unwrap();
        assert_eq!(our_status_line(command), Some(Some(mine.to_string())));
        assert_eq!(doc["statusLine"]["padding"], 2);
        assert_eq!(
            statusline_add(Some(&text), &ours).unwrap(),
            Merge::Unchanged
        );

        // Reinstalling from a new path keeps the wrapped line.
        let moved = written(statusline_add(Some(&text), "/opt/geleider statusline").unwrap());
        let doc: Value = serde_json::from_str(&moved).unwrap();
        let command = doc["statusLine"]["command"].as_str().unwrap();
        assert!(
            command.starts_with("/opt/geleider statusline --wrap "),
            "{command}"
        );
        assert_eq!(our_status_line(command), Some(Some(mine.to_string())));

        let restored: Value =
            serde_json::from_str(&written(statusline_remove(Some(&moved)).unwrap())).unwrap();
        assert_eq!(restored, existing);
        // A status line geleider did not write is left alone on uninstall.
        assert_eq!(
            statusline_remove(Some(&existing.to_string())).unwrap(),
            Merge::Unchanged
        );
    }

    #[test]
    fn status_line_command_keeps_env_as_assignments() {
        let command = command_with_env(
            [("GELEIDER_STATE_DIR", "/a b".to_string())],
            "/x y/geleider",
        );
        assert_eq!(
            command,
            "GELEIDER_STATE_DIR='/a b' '/x y/geleider' statusline"
        );
        assert_eq!(our_status_line(&command), Some(None));
        let out = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &command.replace("'/x y/geleider' statusline", "printenv GELEIDER_STATE_DIR"),
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "/a b\n");
    }

    #[test]
    fn status_line_command_is_recognised() {
        assert_eq!(
            our_status_line("/usr/local/bin/geleider statusline"),
            Some(None)
        );
        assert_eq!(
            our_status_line("GELEIDER_STATE_DIR='/a b' /x/geleider statusline"),
            Some(None)
        );
        assert_eq!(our_status_line("/x/geleider status"), None);
        assert_eq!(our_status_line("ccstatusline"), None);
        assert_eq!(
            our_status_line("/x/geleider statusline --wrap 'echo hi'"),
            Some(Some("echo hi".into()))
        );
    }

    #[test]
    fn writes_keep_permissions_and_never_follow_planted_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, "{\"primaryApiKey\": \"secret\"}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Symlinks at the temp and backup names, aimed at a file elsewhere.
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, sibling(&path, ".geleider-tmp")).unwrap();
        std::os::unix::fs::symlink(&target, sibling(&path, ".geleider-backup")).unwrap();

        write_atomically(&path, "{}\n", true).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        for file in [path.clone(), sibling(&path, ".geleider-backup")] {
            let meta = std::fs::symlink_metadata(&file).unwrap();
            assert!(meta.is_file());
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "{}",
                file.display()
            );
        }
        assert!(
            std::fs::read_to_string(sibling(&path, ".geleider-backup"))
                .unwrap()
                .contains("secret")
        );

        let fresh = dir.path().join("new.json");
        write_atomically(&fresh, "{}", false).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn service_exe_is_read_back_from_the_plist() {
        let exe = Path::new("/Users/a&b/<bin>/geleider");
        let xml = plist_xml(exe, &[], Path::new("/tmp/log"));
        assert_eq!(
            service_exe(&xml).as_deref(),
            Some("/Users/a&b/<bin>/geleider")
        );
        assert_eq!(service_exe("<plist/>"), None);
    }

    #[test]
    fn plist_escapes_values() {
        let xml = plist_xml(
            Path::new("/Apps/A&B/geleider"),
            &[("PATH".into(), "/bin:/usr/bin".into())],
            Path::new("/logs/daemon.log"),
        );
        assert!(xml.contains("<string>/Apps/A&amp;B/geleider</string>"));
        assert!(xml.contains("<key>PATH</key>\n\t\t<string>/bin:/usr/bin</string>"));
    }

    #[test]
    fn starter_config_parses() {
        crate::config::Config::parse(STARTER_CONFIG).unwrap();
    }
}
