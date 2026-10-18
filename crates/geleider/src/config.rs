//! `~/.config/geleider/config.toml`: Projects, Resources and the Jobs each Project
//! may submit. Jobs come only from this file, never from a Project's repo.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::paths;
use crate::usage::Subscription;

/// Placeholders every Job command may use besides its own args and vars.
const BUILTINS: &[&str] = &["worktree"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// macOS notifications for Runs and CI failures on Session PRs.
    #[serde(default = "yes")]
    pub notifications: bool,
    /// Post each Run's result on the branch's open PR.
    #[serde(default = "yes")]
    pub pr_comments: bool,
    /// How often the background process looks for processes holding a Resource.
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default)]
    pub subscriptions: Subscriptions,
    #[serde(default)]
    pub resources: BTreeMap<String, Resource>,
    #[serde(default)]
    pub projects: BTreeMap<String, Project>,
    /// Named ways of starting Dispatched Sessions. Never read from a Project.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// Which Harnesses can be Dispatched. OpenCode cannot yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchHarness {
    Claude,
    Codex,
}

impl DispatchHarness {
    pub fn harness(self) -> crate::session::Harness {
        match self {
            Self::Claude => crate::session::Harness::Claude,
            Self::Codex => crate::session::Harness::Codex,
        }
    }

    pub fn subscription(self) -> Subscription {
        match self {
            Self::Claude => Subscription::Claude,
            Self::Codex => Subscription::Chatgpt,
        }
    }
}

/// How a Dispatched Session starts: its Harness, settings and prompt.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub harness: DispatchHarness,
    /// Claude Code: `--model`.
    pub model: Option<String>,
    /// Claude Code: `--effort`.
    pub effort: Option<String>,
    /// Claude Code: fast mode, which bills usage credits.
    #[serde(default)]
    pub fast: bool,
    /// Claude Code: `--permission-mode`.
    pub permission_mode: Option<String>,
    /// Codex: the Codex profile to start with (`codex exec -p`), so Codex stays
    /// the source of its model, effort and sandbox settings.
    pub codex_profile: Option<String>,
    /// Prompt templates. Placeholders: `{repo}`, `{number}` and `{kind}`.
    pub issue_prompt: Option<String>,
    pub pr_prompt: Option<String>,
}

/// Placeholders a Profile's prompt templates may use.
pub const PROMPT_PLACEHOLDERS: &[&str] = &["repo", "number", "kind"];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultracode"];
const PERMISSION_MODES: &[&str] = &[
    "default",
    "manual",
    "acceptEdits",
    "plan",
    "auto",
    "dontAsk",
    "bypassPermissions",
];

impl Profile {
    fn validate(&self, name: &str) -> Result<()> {
        let at = || format!("profile `{name}`");
        let word = |value: &str| {
            !value.starts_with('-')
                && value.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'[' | b']')
                })
        };
        match self.harness {
            DispatchHarness::Claude => {
                if self.codex_profile.is_some() {
                    bail!("{}: codex_profile is only for Codex profiles", at());
                }
                if let Some(model) = nonempty(&self.model)
                    && !word(model)
                {
                    bail!("{}: model `{model}` is not a model name", at());
                }
                if let Some(effort) = nonempty(&self.effort)
                    && !EFFORTS.contains(&effort)
                {
                    bail!("{}: effort must be one of {}", at(), EFFORTS.join(", "));
                }
                if let Some(mode) = nonempty(&self.permission_mode)
                    && !PERMISSION_MODES.contains(&mode)
                {
                    bail!(
                        "{}: permission_mode must be one of {}",
                        at(),
                        PERMISSION_MODES.join(", ")
                    );
                }
            }
            DispatchHarness::Codex => {
                if self.model.is_some()
                    || self.effort.is_some()
                    || self.fast
                    || self.permission_mode.is_some()
                {
                    bail!(
                        "{}: Codex takes its settings from a Codex profile; set codex_profile \
                         instead of model, effort, fast or permission_mode",
                        at()
                    );
                }
                if let Some(profile) = nonempty(&self.codex_profile)
                    && !word(profile)
                {
                    bail!("{}: codex_profile `{profile}` is not a profile name", at());
                }
            }
        }
        for template in [&self.issue_prompt, &self.pr_prompt].into_iter().flatten() {
            for name in placeholders(template).with_context(at)? {
                if !PROMPT_PLACEHOLDERS.contains(&name.as_str()) {
                    bail!(
                        "{}: unknown placeholder `{{{name}}}` (prompts may use {{repo}}, \
                         {{number}} and {{kind}})",
                        at()
                    );
                }
            }
        }
        Ok(())
    }
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.is_empty())
}

/// Fills a prompt template. `values` must cover [`PROMPT_PLACEHOLDERS`].
pub fn fill_prompt(template: &str, values: &BTreeMap<&str, String>) -> Result<String> {
    substitute(template, values)
}

/// Which Subscriptions `geleider status` shows, and when their resets notify.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Subscriptions {
    /// Notify when a Limit window resets that had reached this percentage used,
    /// or `false` for never. Windows used less reset quietly.
    #[serde(deserialize_with = "percent_or_false")]
    pub notify_resets: Option<f64>,
    pub claude: SubscriptionConfig,
    pub chatgpt: SubscriptionConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubscriptionConfig {
    pub enabled: bool,
    /// Shown in `geleider status`, e.g. `Claude Max`.
    pub plan: Option<String>,
}

/// Reaching 80% is when a reset starts to matter: below it you were not about to
/// run out.
const DEFAULT_NOTIFY_RESETS: f64 = 80.0;

impl Default for Subscriptions {
    fn default() -> Self {
        Self {
            notify_resets: Some(DEFAULT_NOTIFY_RESETS),
            claude: SubscriptionConfig::default(),
            chatgpt: SubscriptionConfig::default(),
        }
    }
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            plan: None,
        }
    }
}

impl Subscriptions {
    pub fn get(&self, subscription: Subscription) -> &SubscriptionConfig {
        match subscription {
            Subscription::Claude => &self.claude,
            Subscription::Chatgpt => &self.chatgpt,
        }
    }

    pub fn plan(&self, subscription: Subscription) -> String {
        self.get(subscription)
            .plan
            .clone()
            .unwrap_or_else(|| subscription.default_plan().to_string())
    }
}

fn percent_or_false<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Threshold {
        Switch(bool),
        Percent(f64),
    }
    Ok(match Threshold::deserialize(d)? {
        Threshold::Switch(false) => None,
        Threshold::Switch(true) => Some(DEFAULT_NOTIFY_RESETS),
        Threshold::Percent(p) => Some(p),
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub slots: u32,
    /// Name of the executable that uses this Resource, e.g. `Unity`. Every running
    /// process with this name holds a Lease.
    pub process: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// GitHub repository as `owner/name`.
    pub repo: String,
    /// The local clone. Its worktrees belong to the Project too.
    pub path: String,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub jobs: BTreeMap<String, Job>,
    /// The Profile `geleider dispatch` uses without `--profile`.
    pub default_profile: Option<String>,
    /// Shell commands run in each new Dispatch worktree before its Session
    /// starts, outside any sandbox. Only ever read from this file.
    pub setup: Option<String>,
    /// Ignored paths, relative to the worktree root, that a Finished worktree
    /// may still contain and that removal deletes, such as `target/`.
    #[serde(default)]
    pub disposable: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub resource: String,
    /// Names of the positional args a Run passes, usable as `{name}` in `command`.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// Program and arguments, split like a shell would but never run through one,
    /// so arg values cannot inject commands. `{name}` is replaced inside each word.
    pub command: String,
}

fn yes() -> bool {
    true
}

fn default_poll_interval_ms() -> u64 {
    2000
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.max(100))
    }

    fn validate(&self) -> Result<()> {
        if let Some(p) = self.subscriptions.notify_resets
            && !(0.0..=100.0).contains(&p)
        {
            bail!("subscriptions.notify_resets must be a percentage from 0 to 100, or false");
        }
        for (name, resource) in &self.resources {
            if resource.slots == 0 {
                bail!("resource `{name}` needs at least one slot");
            }
            if resource.process.trim().is_empty() {
                bail!("resource `{name}` needs a process name to watch");
            }
        }
        for (name, profile) in &self.profiles {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                bail!("profile `{name}`: names use letters, digits, `-`, `_` and `.`");
            }
            profile.validate(name)?;
        }
        for (project_name, project) in &self.projects {
            crate::github::check_repo(&project.repo)
                .with_context(|| format!("project `{project_name}`"))?;
            if let Some(profile) = &project.default_profile
                && !self.profiles.contains_key(profile)
            {
                bail!("project `{project_name}`: unknown default_profile `{profile}`");
            }
            for path in &project.disposable {
                let trimmed = path.trim_end_matches('/');
                if trimmed.is_empty()
                    || trimmed.starts_with('/')
                    || trimmed
                        .split('/')
                        .any(|c| matches!(c, "" | "." | ".." | ".git"))
                {
                    bail!(
                        "project `{project_name}`: disposable path `{path}` must be relative \
                         to the worktree root, without `.`, `..` or `.git`"
                    );
                }
            }
            for (job_name, job) in &project.jobs {
                let at = || format!("job `{job_name}` in project `{project_name}`");
                if !self.resources.contains_key(&job.resource) {
                    bail!("{}: unknown resource `{}`", at(), job.resource);
                }
                let mut known: BTreeSet<&str> = BUILTINS.iter().copied().collect();
                for arg in &job.args {
                    if !known.insert(arg) {
                        bail!(
                            "{}: arg `{arg}` is declared twice or shadows a builtin",
                            at()
                        );
                    }
                }
                known.extend(project.vars.keys().map(String::as_str));
                known.extend(job.vars.keys().map(String::as_str));
                let words = shell_words::split(&job.command)
                    .with_context(|| format!("{}: cannot split command", at()))?;
                if words.is_empty() {
                    bail!("{}: command is empty", at());
                }
                for word in &words {
                    for name in placeholders(word).with_context(at)? {
                        if !known.contains(name.as_str()) {
                            bail!("{}: unknown placeholder `{{{name}}}`", at());
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Re-reads the config file when it changes, keeping the last good version if an
/// edit breaks it.
pub struct ConfigCache {
    path: PathBuf,
    modified: Option<SystemTime>,
    current: Option<Config>,
}

impl ConfigCache {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            modified: None,
            current: None,
        }
    }

    pub fn get(&mut self) -> Result<&Config> {
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if self.current.is_none() || modified != self.modified {
            match Config::load(&self.path) {
                Ok(config) => self.current = Some(config),
                Err(err) if self.current.is_some() => {
                    eprintln!("geleider: keeping previous config: {err:#}");
                }
                Err(err) => return Err(err),
            }
            self.modified = modified;
        }
        self.current.as_ref().context("no config loaded")
    }
}

/// A Job resolved for one Run: which Resource it needs and the exact argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub resource: String,
    pub argv: Vec<String>,
}

impl Project {
    pub fn clone_path(&self) -> Result<PathBuf> {
        paths::expand_tilde(&self.path)
    }

    /// Fills in a Job's command for a Run in `worktree` with the given args.
    pub fn render(&self, job_name: &str, args: &[String], worktree: &Path) -> Result<Rendered> {
        let Some(job) = self.jobs.get(job_name) else {
            let known: Vec<&str> = self.jobs.keys().map(String::as_str).collect();
            if known.is_empty() {
                bail!("no job `{job_name}`: this project declares no jobs");
            }
            bail!(
                "no job `{job_name}` in this project (known: {})",
                known.join(", ")
            );
        };
        // Args are shown in `geleider status`, notifications and PR comments.
        if let Some(arg) = args.iter().find(|a| crate::text::has_hidden(a)) {
            bail!("job args cannot contain control or invisible characters: {arg:?}");
        }
        if args.len() != job.args.len() {
            let usage: Vec<String> = job.args.iter().map(|a| format!("<{a}>")).collect();
            bail!(
                "job `{job_name}` takes {} arg(s): {job_name} {}",
                job.args.len(),
                usage.join(" ")
            );
        }
        let mut values: BTreeMap<&str, String> = BTreeMap::new();
        values.insert("worktree", worktree.display().to_string());
        for (key, value) in self.vars.iter().chain(&job.vars) {
            values.insert(key, value.clone());
        }
        for (name, value) in job.args.iter().zip(args) {
            values.insert(name, value.clone());
        }

        let mut argv = Vec::new();
        for word in shell_words::split(&job.command)? {
            argv.push(substitute(&word, &values)?);
        }
        // A relative program path such as `scripts/test.sh` means one in the worktree.
        if let Some(program) = argv.first_mut()
            && program.contains('/')
            && Path::new(program.as_str()).is_relative()
        {
            *program = worktree.join(&*program).display().to_string();
        }
        Ok(Rendered {
            resource: job.resource.clone(),
            argv,
        })
    }
}

/// Names of the `{placeholders}` in one word. `{{` and `}}` are literal braces.
fn placeholders(word: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    walk(word, |piece| {
        if let Piece::Name(name) = piece {
            names.push(name.to_string());
        }
        Ok(())
    })?;
    Ok(names)
}

fn substitute(word: &str, values: &BTreeMap<&str, String>) -> Result<String> {
    let mut out = String::new();
    walk(word, |piece| {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Name(name) => match values.get(name) {
                Some(value) => out.push_str(value),
                None => bail!("unknown placeholder `{{{name}}}`"),
            },
        }
        Ok(())
    })?;
    Ok(out)
}

enum Piece<'a> {
    Text(&'a str),
    Name(&'a str),
}

fn walk<'a>(word: &'a str, mut f: impl FnMut(Piece<'a>) -> Result<()>) -> Result<()> {
    let mut rest = word;
    while let Some(i) = rest.find(['{', '}']) {
        f(Piece::Text(&rest[..i]))?;
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            f(Piece::Text(&tail[..1]))?;
            rest = &tail[2..];
        } else if tail.starts_with('}') {
            bail!("unmatched `}}` in `{word}` (write `}}}}` for a literal brace)");
        } else {
            let end = tail
                .find('}')
                .with_context(|| format!("unclosed `{{` in `{word}`"))?;
            let name = &tail[1..end];
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !valid {
                bail!("bad placeholder `{{{name}}}` in `{word}`");
            }
            f(Piece::Name(name))?;
            rest = &tail[end + 1..];
        }
    }
    f(Piece::Text(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANTERNFALL: &str = r#"
        [resources.unity]
        slots = 1
        process = "Unity"

        [projects.lanternfall]
        repo = "Eloyye/lanternfall"
        path = "/code/lanternfall"
        vars = { unity = "/Unity/Unity.app/Contents/MacOS/Unity" }

        [projects.lanternfall.jobs.playmode-test]
        resource = "unity"
        args = ["suite"]
        command = "scripts/run-playmode.sh {suite} {unity}"
    "#;

    #[test]
    fn renders_args_vars_and_relative_program() {
        let config = Config::parse(LANTERNFALL).unwrap();
        let project = &config.projects["lanternfall"];
        let rendered = project
            .render(
                "playmode-test",
                &["combat; rm -rf /".into()],
                Path::new("/wt/feat"),
            )
            .unwrap();
        assert_eq!(rendered.resource, "unity");
        assert_eq!(
            rendered.argv,
            vec![
                "/wt/feat/scripts/run-playmode.sh",
                // An arg stays one word, whatever it contains: no shell runs it.
                "combat; rm -rf /",
                "/Unity/Unity.app/Contents/MacOS/Unity",
            ]
        );
    }

    #[test]
    fn args_cannot_hide_terminal_escapes_and_repos_cannot_leave_their_path() {
        let config = Config::parse(LANTERNFALL).unwrap();
        let project = &config.projects["lanternfall"];
        for arg in ["\u{1b}]52;c;aGk=\u{7}", "a\u{202e}b"] {
            let err = project
                .render("playmode-test", &[arg.into()], Path::new("/wt"))
                .unwrap_err();
            assert!(err.to_string().contains("control or invisible"), "{err}");
        }
        for repo in ["../x", "o/r/x", "o/..", "o/r?x"] {
            let text = LANTERNFALL.replace("Eloyye/lanternfall", repo);
            assert!(Config::parse(&text).is_err(), "{repo}");
        }
    }

    #[test]
    fn disposable_paths_stay_inside_the_worktree() {
        let with = |paths: &str| {
            Config::parse(&format!(
                "[projects.demo]\nrepo='o/r'\npath='/c'\ndisposable=[{paths}]"
            ))
        };
        assert_eq!(
            with("'target/', 'web/dist'").unwrap().projects["demo"].disposable,
            ["target/", "web/dist"]
        );
        for path in ["", "/", "/tmp", "..", "a/../b", "./target", ".git", "a//b"] {
            assert!(with(&format!("'{path}'")).is_err(), "{path}");
        }
    }

    #[test]
    fn rejects_unknown_job_and_wrong_arg_count() {
        let config = Config::parse(LANTERNFALL).unwrap();
        let project = &config.projects["lanternfall"];
        let err = project.render("deploy", &[], Path::new("/wt")).unwrap_err();
        assert!(err.to_string().contains("no job `deploy`"), "{err}");
        let err = project
            .render("playmode-test", &[], Path::new("/wt"))
            .unwrap_err();
        assert!(err.to_string().contains("takes 1 arg"), "{err}");
    }

    #[test]
    fn validates_placeholders_and_resources() {
        let unknown_placeholder = LANTERNFALL.replace("{unity}", "{unity_path}");
        let err = Config::parse(&unknown_placeholder).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown placeholder `{unity_path}`"),
            "{err:#}"
        );

        let unknown_resource = LANTERNFALL.replace("resource = \"unity\"", "resource = \"gpu\"");
        let err = Config::parse(&unknown_resource).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown resource `gpu`"),
            "{err:#}"
        );

        let zero_slots = LANTERNFALL.replace("slots = 1", "slots = 0");
        assert!(Config::parse(&zero_slots).is_err());
    }

    #[test]
    fn subscriptions_default_to_both_with_quiet_resets() {
        let config = Config::parse("").unwrap();
        assert!(config.subscriptions.claude.enabled && config.subscriptions.chatgpt.enabled);
        assert_eq!(config.subscriptions.notify_resets, Some(80.0));
        assert_eq!(
            config.subscriptions.plan(Subscription::Claude),
            "Claude Pro"
        );

        let config = Config::parse(
            "[subscriptions]\nnotify_resets = false\n[subscriptions.claude]\nplan = \"Claude Max\"\n\
             [subscriptions.chatgpt]\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(config.subscriptions.notify_resets, None);
        assert_eq!(
            config.subscriptions.plan(Subscription::Claude),
            "Claude Max"
        );
        assert!(!config.subscriptions.chatgpt.enabled);

        let config = Config::parse("[subscriptions]\nnotify_resets = 50\n").unwrap();
        assert_eq!(config.subscriptions.notify_resets, Some(50.0));
        assert!(Config::parse("[subscriptions]\nnotify_resets = 150\n").is_err());
        // OpenCode Go has no documented usage source.
        assert!(Config::parse("[subscriptions.opencode-go]\n").is_err());
    }

    #[test]
    fn literal_braces_and_builtins() {
        let values = BTreeMap::from([("worktree", "/wt".to_string())]);
        assert_eq!(substitute("{{x}} {worktree}", &values).unwrap(), "{x} /wt");
        assert!(placeholders("{oops").is_err());
        assert!(placeholders("a}b").is_err());
    }
}
