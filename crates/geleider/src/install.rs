//! `geleider install` / `uninstall`: the launchd login service for the background
//! process, and the MCP server entry in each Harness's config. Only geleider's
//! own entries are ever added, changed or removed.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::paths::{self, Paths};

const LABEL: &str = "com.github.eloyye.geleider";
const SERVER: &str = "geleider";

/// The result of merging geleider's entry into a config file.
#[derive(Debug, PartialEq, Eq)]
pub enum Merge {
    Write(String),
    Unchanged,
    /// A `geleider` entry exists that geleider did not write; it is left alone.
    Foreign,
}

pub fn install(paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "note: registering {} from a build directory; `cargo install --path crates/geleider` \
             gives a stable path.",
            exe.display()
        );
    }
    let home = paths::home_dir()?;

    write_starter_config(&paths.config)?;
    install_service(&home, &exe)?;
    for harness in harnesses(&home) {
        let outcome = harness.apply(|text| (harness.add)(text, &exe));
        report(harness.name, &harness.path, outcome, "registered");
    }
    println!(
        "\nDeclare Projects, Resources and Jobs in {} — Jobs come only from that file.",
        paths::display(&paths.config)
    );
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let home = paths::home_dir()?;
    for harness in harnesses(&home) {
        if harness.path.exists() {
            let outcome = harness.apply(|text| (harness.remove)(text));
            report(harness.name, &harness.path, outcome, "removed");
        }
    }
    let plist = plist_path(&home);
    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    if plist.exists() {
        std::fs::remove_file(&plist)?;
    }
    println!("background service: removed {}", paths::display(&plist));
    println!("Config and Run history are kept; delete them by hand if you want them gone.");
    Ok(())
}

struct Harness {
    name: &'static str,
    path: PathBuf,
    /// Whether the Harness looks installed, so its config should be touched.
    present: bool,
    add: fn(Option<&str>, &Path) -> Result<Merge>,
    remove: fn(Option<&str>) -> Result<Merge>,
}

impl Harness {
    fn apply(&self, merge: impl FnOnce(Option<&str>) -> Result<Merge>) -> Result<Option<Merge>> {
        if !self.present {
            return Ok(None);
        }
        let existing = match std::fs::read_to_string(&self.path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.path.display()));
            }
        };
        let merged = merge(existing.as_deref())
            .with_context(|| format!("updating {}", self.path.display()))?;
        if let Merge::Write(text) = &merged {
            write_atomically(&self.path, text, existing.is_some())?;
        }
        Ok(Some(merged))
    }
}

fn harnesses(home: &Path) -> Vec<Harness> {
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
        Harness {
            name: "Claude Code",
            path: home.join(".claude.json"),
            present: home.join(".claude.json").exists() || home.join(".claude").is_dir(),
            add: claude_add,
            remove: claude_remove,
        },
        Harness {
            name: "Codex",
            path: codex_home.join("config.toml"),
            present: codex_home.is_dir(),
            add: codex_add,
            remove: codex_remove,
        },
        Harness {
            name: "OpenCode",
            path: opencode,
            present: opencode_dir.is_dir(),
            add: opencode_add,
            remove: opencode_remove,
        },
    ]
}

fn report(name: &str, path: &Path, outcome: Result<Option<Merge>>, verb: &str) {
    let path = paths::display(path);
    match outcome {
        Ok(None) => println!("{name}: not installed, skipped"),
        Ok(Some(Merge::Write(_))) => println!("{name}: MCP server {verb} in {path}"),
        Ok(Some(Merge::Unchanged)) => println!("{name}: {path} already up to date"),
        Ok(Some(Merge::Foreign)) => println!(
            "{name}: {path} has a `{SERVER}` MCP server geleider did not write; left it alone"
        ),
        Err(err) => println!("{name}: {err:#}"),
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
    let args: Vec<&str> = entry["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    is_ours(entry["command"].as_str(), &args)
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
    let command: Vec<&str> = entry["command"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    match command.split_first() {
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
    Ok(Merge::Write(serde_json::to_string_pretty(&doc)? + "\n"))
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
            Ok(Merge::Write(serde_json::to_string_pretty(&doc)? + "\n"))
        }
    }
}

// Codex: servers live under `[mcp_servers.<name>]` in config.toml. toml_edit keeps
// the user's comments and layout.

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
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    if let Some(current) = servers.get(SERVER) {
        if !codex_is_ours(current) {
            return Ok(Merge::Foreign);
        }
        let same_command = current.get("command").and_then(|c| c.as_str()) == Some(exe_str(exe)?);
        if same_command {
            return Ok(Merge::Unchanged);
        }
    }
    let mut table = toml_edit::Table::new();
    table.insert("command", toml_edit::value(exe_str(exe)?));
    table.insert("args", toml_edit::value(args));
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

fn write_atomically(path: &Path, text: &str, keep_backup: bool) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if keep_backup {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".geleider-backup");
        std::fs::copy(path, &backup)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".geleider-tmp");
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, text)?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&temp, meta.permissions())?;
    }
    std::fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))
}

// launchd

fn plist_path(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
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

fn install_service(home: &Path, exe: &Path) -> Result<()> {
    let logs = home.join("Library/Logs/geleider");
    std::fs::create_dir_all(&logs)?;
    // launchd starts services with a bare PATH; Jobs and `gh` need the user's.
    let mut env = vec![(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    )];
    for key in ["GELEIDER_CONFIG", "GELEIDER_STATE_DIR"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    let plist = plist_path(home);
    std::fs::create_dir_all(plist.parent().expect("has parent"))?;
    std::fs::write(&plist, plist_xml(exe, &env, &logs.join("daemon.log")))?;

    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    launchctl(&[
        "bootstrap",
        &domain(),
        plist.to_str().context("non-UTF-8 path")?,
    ])?;
    println!(
        "background service: started at login via {}",
        paths::display(&plist)
    );
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
# [resources.ck3]
# slots = 1                 # how many processes may use it at once
# process = "ck3"           # executable name; any running process with it holds a Lease
#
# [projects.queendom]
# repo = "owner/queendom"
# path = "~/code/queendom"  # the clone; its worktrees belong to the Project too
# vars = { ck3_dir = "~/Library/Application Support/Steam/steamapps/common/Crusader Kings III" }
#
# [projects.queendom.jobs.ck3-test]
# resource = "ck3"
# args = ["suite"]          # geleider run ck3-test <suite>
# # Split like a shell would, but never run through one. {worktree} is built in.
# command = "scripts/test-engine.sh {suite} {ck3_dir}"
"#;

fn write_starter_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().context("config path has no parent")?)?;
    std::fs::write(path, STARTER_CONFIG)?;
    println!(
        "config: wrote a commented starter at {}",
        paths::display(path)
    );
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
            "[mcp_servers.geleider]\ncommand = \"{EXE}\"\nargs = [\"mcp\"]"
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
