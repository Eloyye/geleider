//! Drives a real `geleider daemon` with a fake Resource: a copy of `sleep` under a
//! made-up name plays the part of the game.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_geleider");

struct Env {
    dir: tempfile::TempDir,
    game: String,
    daemon: Option<Child>,
}

impl Env {
    /// A config with one one-slot Resource watched under the name `game`, and a
    /// Project whose Jobs use it.
    fn new(game: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        // macOS kills a copied system binary on launch unless it is re-signed.
        let game_bin = root.join("bin").join(game);
        std::fs::copy("/bin/sleep", &game_bin).unwrap();
        let signed = Command::new("codesign")
            .args(["-f", "-s", "-"])
            .arg(&game_bin)
            .output()
            .unwrap();
        assert!(
            signed.status.success(),
            "{}",
            String::from_utf8_lossy(&signed.stderr)
        );

        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "feat/x"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );

        let game_path = root.join("bin").join(game);
        std::fs::write(
            root.join("config.toml"),
            format!(
                r#"
notifications = false
pr_comments = false
poll_interval_ms = 100

[resources.game]
slots = 1
process = "{game}"

[projects.demo]
repo = "example/demo"
path = "{repo}"
vars = {{ game = "{game_path}" }}

# The game runs as a child of a shell, like a test script launching it.
[projects.demo.jobs.play]
resource = "game"
args = ["secs"]
command = "sh -c '\"$0\" \"$1\"; true' {{game}} {{secs}}"

# The game is the Run's own process.
[projects.demo.jobs.hold]
resource = "game"
args = ["secs"]
command = "{{game}} {{secs}}"

[projects.demo.jobs.script]
resource = "game"
args = ["name"]
command = "sh -c 'echo hello \"$0\" from \"$PWD\"; exit 3' {{name}}"
"#,
                repo = repo.display(),
                game_path = game_path.display(),
            ),
        )
        .unwrap();

        let mut env = Self {
            dir,
            game: game.to_string(),
            daemon: None,
        };
        env.start_daemon();
        env
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn repo(&self) -> PathBuf {
        self.root().join("repo")
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env("GELEIDER_CONFIG", self.root().join("config.toml"))
            .env("GELEIDER_STATE_DIR", self.root().join("state"))
            .env_remove("CODEX_THREAD_ID")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root().join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            // Never the real Codex's session logs.
            .env("CODEX_HOME", self.root().join("codex"))
            .current_dir(self.repo());
        cmd
    }

    fn start_daemon(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root().join("daemon.log"))
            .unwrap();
        let child = self
            .command()
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        // Generous: macOS scans a freshly built binary on its first launch.
        wait_until(Duration::from_secs(60), || {
            self.command()
                .arg("queue")
                .output()
                .unwrap()
                .status
                .success()
        });
    }

    fn kill_daemon(&mut self) {
        let mut daemon = self.daemon.take().unwrap();
        daemon.kill().unwrap();
        daemon.wait().unwrap();
    }

    fn geleider(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn submit(&self, job: &str, arg: &str) -> Value {
        let out = self.geleider(&["run", job, arg, "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn show(&self, id: i64) -> Value {
        let out = self.geleider(&["show", &id.to_string(), "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn wait(&self, id: i64) -> Value {
        let out = self.geleider(&["wait", &id.to_string(), "--json"]);
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)))
    }

    /// Starts the game by hand, as the user play-testing would.
    fn start_game_by_hand(&self, secs: &str) -> Child {
        Command::new(self.root().join("bin").join(&self.game))
            .arg(secs)
            .spawn()
            .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        // Leave no stray games behind for other tests.
        let _ = Command::new("pkill").args(["-x", &self.game]).status();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < timeout, "timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn id(run: &Value) -> i64 {
    run["id"].as_i64().unwrap()
}

fn ms(run: &Value, key: &str) -> i64 {
    run[key]
        .as_i64()
        .unwrap_or_else(|| panic!("no {key} in {run}"))
}

#[test]
fn two_submissions_run_one_after_the_other() {
    let env = Env::new("fakegame-order");
    let first = env.submit("play", "1");
    let second = env.submit("play", "1");
    assert_eq!(first["state"], "running");
    assert_eq!(second["state"], "queued");
    assert_eq!(second["queue_position"], 1);
    assert_eq!(
        first["worktree"],
        std::fs::canonicalize(env.repo())
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(first["branch"], "feat/x");

    // The game the first Run launched counts as that Run's Lease, not a second one.
    std::thread::sleep(Duration::from_millis(500));
    let status = env.geleider(&["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        status["resources"][0]["holders"],
        serde_json::json!([{"kind": "run", "id": id(&first)}])
    );

    let first = env.wait(id(&first));
    let second = env.wait(id(&second));
    assert_eq!(
        (first["state"].as_str(), first["exit_code"].as_i64()),
        (Some("succeeded"), Some(0))
    );
    assert_eq!(
        (second["state"].as_str(), second["exit_code"].as_i64()),
        (Some("succeeded"), Some(0))
    );
    assert!(
        ms(&second, "started_at") >= ms(&first, "ended_at"),
        "runs overlapped"
    );
}

#[test]
fn a_game_started_by_hand_holds_the_queue_until_it_quits() {
    let env = Env::new("fakegame-hand");
    let mut game = env.start_game_by_hand("30");
    std::thread::sleep(Duration::from_millis(400));

    let run = env.submit("hold", "0");
    assert_eq!(run["state"], "queued");
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(env.show(id(&run))["state"], "queued");

    let status: Value =
        serde_json::from_slice(&env.geleider(&["status", "--json"]).stdout).unwrap();
    assert_eq!(status["resources"][0]["holders"][0]["pid"], game.id());

    game.kill().unwrap();
    game.wait().unwrap();
    let run = env.wait(id(&run));
    assert_eq!(run["state"], "succeeded");
}

#[test]
fn killing_a_runs_process_frees_the_slot() {
    let env = Env::new("fakegame-kill");
    let first = env.submit("hold", "30");
    let second = env.submit("hold", "0");
    assert_eq!(second["state"], "queued");

    let pid = env.show(id(&first))["pid"].as_i64().unwrap();
    Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();

    let first = env.wait(id(&first));
    assert_eq!(
        (first["state"].as_str(), first["exit_code"].as_i64()),
        (Some("failed"), Some(137))
    );
    assert_eq!(env.wait(id(&second))["state"], "succeeded");
}

#[test]
fn killing_the_background_process_frees_the_slot_once_the_run_ends() {
    let mut env = Env::new("fakegame-restart");
    let first = env.submit("hold", "1");
    let second = env.submit("hold", "0");
    assert_eq!(second["state"], "queued");

    env.kill_daemon();
    env.start_daemon();

    // The restarted process finds the first Run's process, waits for it to go,
    // then lets the queue carry on.
    let first = env.wait(id(&first));
    assert_eq!(first["state"], "lost");
    let second = env.wait(id(&second));
    assert_eq!(second["state"], "succeeded");
    assert!(ms(&second, "started_at") >= ms(&first, "ended_at"));
}

#[test]
fn wait_returns_the_runs_exit_code_and_log_tail() {
    let env = Env::new("fakegame-exit");
    let out = env.geleider(&["run", "script", "it's me; rm -rf /", "--wait"]);
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let repo = std::fs::canonicalize(env.repo()).unwrap();
    assert!(
        stdout.contains(&format!("hello it's me; rm -rf / from {}", repo.display())),
        "{stdout}"
    );
    assert!(stdout.contains("exit code  3"), "{stdout}");
}

#[test]
fn only_jobs_in_geleiders_config_can_run() {
    let env = Env::new("fakegame-unknown");
    // A job declared inside the repo means nothing to geleider.
    std::fs::write(
        env.repo().join("geleider.toml"),
        "[projects.demo.jobs.deploy]\nresource = \"game\"\ncommand = \"touch pwned\"\n",
    )
    .unwrap();
    let out = env.geleider(&["run", "deploy"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no job `deploy`"), "{stderr}");
    assert!(!env.repo().join("pwned").exists());

    let out = env
        .command()
        .current_dir(env.root())
        .args(["run", "play", "1"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not inside a git repository"));
}

#[test]
fn mcp_server_submits_and_reports_runs() {
    use std::io::{BufRead, BufReader, Write};

    let env = Env::new("fakegame-mcp");
    let mut mcp = env
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());
    let mut rpc = |message: Value| -> Value {
        writeln!(stdin, "{message}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    rpc(
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "codex"}, "capabilities": {}}}),
    );
    let submitted = rpc(
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "submit_run", "arguments": {"job": "hold", "args": ["0"]}}}),
    );
    let run = &submitted["result"]["structuredContent"];
    assert_eq!(run["submitter"], "mcp:codex", "{submitted}");

    let finished = env.wait(id(run));
    assert_eq!(finished["state"], "succeeded");
    let got = rpc(
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "get_run", "arguments": {"id": id(run)}}}),
    );
    assert_eq!(got["result"]["structuredContent"]["exit_code"], 0);

    drop(stdin);
    mcp.wait().unwrap();
}

fn hook(env: &Env, harness: &str, event: &str, payload: Value) -> Output {
    use std::io::Write;
    let mut child = env
        .command()
        .args(["hook", harness, event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn sessions(env: &Env) -> Vec<Value> {
    let out = env.geleider(&["status", "--json"]);
    serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessions"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn harness_sessions_track_worktrees_waiting_idle_and_end_across_restart() {
    use serde_json::json;
    let mut env = Env::new("fakegame-sessions");
    let wt = env.root().join("worktree");
    git(
        &env.repo(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/other",
            wt.to_str().unwrap(),
        ],
    );
    for harness in ["claude", "codex", "opencode"] {
        let out = hook(
            &env,
            harness,
            "SessionStart",
            json!({"session_id":harness, "cwd":wt}),
        );
        assert!(out.status.success());
    }
    wait_until(Duration::from_secs(5), || sessions(&env).len() == 3);
    for session in sessions(&env) {
        assert_eq!(session["project"], "demo");
        assert_eq!(session["branch"], "feat/other");
        assert_eq!(session["state"], "idle");
    }
    let input = json!({"session_id":"codex", "cwd":wt, "tool_use_id":"needs-approval"});
    hook(&env, "codex", "PermissionRequest", input.clone());
    wait_until(Duration::from_secs(5), || {
        sessions(&env)[0]["state"] == "waiting"
    });
    assert_eq!(sessions(&env)[0]["id"], "codex");
    // Completing a parallel tool must not hide an outstanding user prompt.
    hook(
        &env,
        "codex",
        "PostToolUse",
        json!({"session_id":"codex","cwd":wt,"tool_use_id":"another-tool"}),
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(sessions(&env)[0]["state"], "waiting");
    hook(&env, "codex", "PostToolUse", input.clone());
    wait_until(Duration::from_secs(5), || {
        sessions(&env)
            .iter()
            .any(|s| s["id"] == "codex" && s["state"] == "working")
    });
    hook(&env, "codex", "Stop", input.clone());
    wait_until(Duration::from_secs(5), || {
        sessions(&env).iter().all(|s| s["state"] == "idle")
    });
    env.kill_daemon();
    env.start_daemon();
    assert_eq!(sessions(&env).len(), 3);
    hook(&env, "codex", "SessionEnd", input);
    wait_until(Duration::from_secs(5), || {
        sessions(&env).iter().any(|s| s["state"] == "ended")
    });
    let text = String::from_utf8(env.geleider(&["status"]).stdout).unwrap();
    assert!(!text.contains(" codex "));
}

#[test]
fn hooks_succeed_quickly_with_daemon_down_stalled_input_or_full_socket() {
    use serde_json::json;
    use std::os::unix::net::UnixDatagram;
    let mut env = Env::new("fakegame-hook-down");
    env.kill_daemon();
    let start = Instant::now();
    let out = hook(
        &env,
        "codex",
        "PermissionRequest",
        json!({"session_id":"s", "cwd":env.repo()}),
    );
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    assert!(start.elapsed() < Duration::from_secs(1));
    // A caller that leaves stdin open cannot hold the hook indefinitely.
    let mut child = env
        .command()
        .args(["hook", "claude", "Stop"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    wait_until(Duration::from_secs(1), || {
        child.try_wait().unwrap().is_some()
    });
    assert!(start.elapsed() < Duration::from_millis(800));
    assert!(child.wait().unwrap().success());
    let path = env.root().join("state/hooks.sock");
    std::fs::remove_file(&path).unwrap();
    let _unread = UnixDatagram::bind(&path).unwrap();
    for _ in 0..40 {
        let out = hook(
            &env,
            "codex",
            "Stop",
            json!({"session_id":"s", "cwd":env.repo()}),
        );
        assert!(out.status.success());
    }
    // The annotation still works while reporting is unavailable, including for
    // a child tool call whose session_id belongs to the parent Session.
    let out = hook(
        &env,
        "codex",
        "PreToolUse",
        json!({"session_id":"right-thread","cwd":env.repo(),"agent_id":"child",
        "tool_name":"mcp__geleider__submit_run","tool_input":{"job":"hold","args":["0"]}}),
    );
    let output: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["updatedInput"]["session"],
        json!({"harness":"codex","id":"right-thread"})
    );
    assert_eq!(
        output["hookSpecificOutput"]["updatedInput"]["cwd"],
        json!(env.repo())
    );
    let malformed = hook(&env, "claude", "Stop", json!("bad payload"));
    assert!(malformed.status.success());
    assert!(malformed.stdout.is_empty());
}

#[test]
fn codex_result_delivery_retries_and_targets_the_submitting_session() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("fakegame-delivery");
    let program = env.root().join("bin/codex");
    std::fs::write(
        &program,
        r#"#!/usr/bin/env python3
import sys,json,pathlib
root = pathlib.Path(__file__).parent.parent
if not (root / 'allow-delivery').exists(): sys.exit(1)
with (root / 'queued-messages').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = env
        .command()
        .env("CODEX_THREAD_ID", "the-submitting-session")
        .args(["run", "script", "delivery log", "--json"])
        .output()
        .unwrap();
    let run: Value = serde_json::from_slice(&out.stdout).unwrap();
    let finished = env.wait(id(&run));
    assert_eq!(finished["session"]["id"], "the-submitting-session");
    // Keep failing long enough to exercise a retry without acknowledging.
    std::thread::sleep(Duration::from_millis(2300));
    assert!(!env.root().join("queued-messages").exists());
    std::fs::write(env.root().join("allow-delivery"), "").unwrap();
    wait_until(Duration::from_secs(8), || {
        env.root().join("queued-messages").exists()
    });
    let text = std::fs::read_to_string(env.root().join("queued-messages")).unwrap();
    let args: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(args[0], "queue");
    assert_eq!(args[1], "--thread");
    assert_eq!(args[2], "the-submitting-session");
    assert_eq!(args[3], "--message");
    let message = args[4].as_str().unwrap();
    assert!(message.contains("failed (exit 3)"));
    assert!(message.contains("hello delivery log"));
    std::thread::sleep(Duration::from_millis(2300));
    assert_eq!(
        std::fs::read_to_string(env.root().join("queued-messages"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn crashed_harness_sessions_end_when_their_process_exits() {
    use serde_json::json;
    let env = Env::new("fakegame-owner");
    let mut owner = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    hook(
        &env,
        "codex",
        "SessionStart",
        json!({"session_id":"owned","cwd":env.repo(),"owner_pid":owner.id()}),
    );
    wait_until(Duration::from_secs(5), || !sessions(&env).is_empty());
    assert_eq!(sessions(&env)[0]["state"], "idle");
    owner.kill().unwrap();
    owner.wait().unwrap();
    wait_until(Duration::from_secs(5), || {
        sessions(&env)[0]["state"] == "ended"
    });
}

#[test]
fn a_shared_mcp_process_keeps_session_destinations_separate_and_results_survive_restart() {
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    let mut env = Env::new("fakegame-shared");
    let mut mcp = env
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());
    let mut rpc = |message: Value| -> Value {
        writeln!(stdin, "{message}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    rpc(json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"opencode"}}}));
    let mut ids = Vec::new();
    for (index, session) in ["first", "second"].into_iter().enumerate() {
        let submitted = rpc(
            json!({"id":index+2,"method":"tools/call","params":{"name":"submit_run","arguments":{
                "job":"hold","args":["0"],"cwd":env.repo(),"session":{"harness":"opencode","id":session}
            }}}),
        );
        let run = &submitted["result"]["structuredContent"];
        assert_eq!(run["session"]["id"], session);
        ids.push(id(run));
    }
    drop(rpc);
    drop(stdin);
    mcp.wait().unwrap();
    for id in &ids {
        env.wait(*id);
    }
    env.kill_daemon();
    env.start_daemon();
    let out = env.geleider(&["session-results", "opencode", "second"]);
    let results: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results.as_array().unwrap().len(), 1);
    assert_eq!(results[0]["run_id"], ids[1]);
    let out = env.geleider(&[
        "session-results",
        "opencode",
        "second",
        "--ack",
        &ids[1].to_string(),
    ]);
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!([])
    );
    let out = env.geleider(&["session-results", "opencode", "first"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()[0]["run_id"],
        ids[0]
    );
}

/// What Claude Code passes its status line, with the given 5-hour and 7-day usage.
fn claude_status_input(five_hour: f64, seven_day: f64, now: i64) -> String {
    serde_json::json!({
        "model": { "display_name": "Opus" },
        "rate_limits": {
            "five_hour": { "used_percentage": five_hour, "resets_at": now + 3 * 3600 },
            "seven_day": { "used_percentage": seven_day, "resets_at": now + 4 * 86_400 }
        }
    })
    .to_string()
}

fn statusline(env: &Env, args: &[&str], input: &str) -> Output {
    use std::io::Write;

    let mut child = env
        .command()
        .arg("statusline")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn status_shows_both_subscriptions_with_their_age() {
    let env = Env::new("fakegame-usage");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // A Codex session two hours ago left its limits in its session log.
    let day = env.root().join("codex/sessions/2026/10/16");
    std::fs::create_dir_all(&day).unwrap();
    let written_at = Command::new("date")
        .args([
            "-u",
            "-r",
            &(now - 7200).to_string(),
            "+%Y-%m-%dT%H:%M:%S.000Z",
        ])
        .output()
        .unwrap();
    let entry = serde_json::json!({
        "timestamp": String::from_utf8_lossy(&written_at.stdout).trim(),
        "type": "event_msg",
        "payload": { "type": "token_count", "info": null, "rate_limits": {
            "limit_id": "codex",
            "primary": { "used_percent": 31.0, "window_minutes": 300, "resets_at": now + 3600 },
            "secondary": { "used_percent": 9.0, "window_minutes": 10080, "resets_at": now + 86_400 }
        }}
    });
    std::fs::write(
        day.join("rollout-2026-10-16T07-00-00-x.jsonl"),
        format!("{entry}\n"),
    )
    .unwrap();

    // A Claude Code CLI session renders its status line.
    let out = statusline(&env, &[], &claude_status_input(23.5, 41.0, now));
    assert!(out.status.success());
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(line.starts_with("Opus · 5h 24% until "), "{line}");

    // A status line the user already had keeps its output.
    let out = statusline(
        &env,
        &["--wrap", "read -r input; echo \"mine: ${#input}\"; exit 3"],
        &claude_status_input(25.0, 41.0, now),
    );
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("mine: "));

    let status = env.geleider(&["status", "--json"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    let subscriptions = status["subscriptions"].as_array().unwrap();
    let windows = |i: usize| -> Vec<(String, f64, bool)> {
        subscriptions[i]["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["window"].as_str().unwrap().to_string(),
                    w["used_percent"].as_f64().unwrap(),
                    w["stale"].as_bool().unwrap(),
                )
            })
            .collect()
    };
    assert_eq!(subscriptions[0]["plan"], "Claude Pro");
    assert_eq!(
        windows(0),
        [("5h".into(), 25.0, false), ("7d".into(), 41.0, false)]
    );
    assert_eq!(subscriptions[1]["plan"], "ChatGPT Plus");
    // Two hours old, so last known rather than current.
    assert_eq!(
        windows(1),
        [("5h".into(), 31.0, true), ("7d".into(), 9.0, true)]
    );
    assert_eq!(
        subscriptions[1]["windows"][0]["resets_at"].as_i64(),
        Some((now + 3600) * 1000)
    );

    let text = String::from_utf8_lossy(&env.geleider(&["status"]).stdout).to_string();
    assert!(text.contains("Claude Pro"), "{text}");
    assert!(text.contains(" 25% used, resets "), "{text}");
    assert!(text.contains("updated "), "{text}");
    assert!(text.contains("last known at "), "{text}");
    assert!(text.contains("(2h 00m ago)"), "{text}");

    // The MCP tool returns the same numbers.
    let mut mcp = env
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "t"}, "capabilities": {}}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "usage", "arguments": {}}}),
    ];
    let mut stdin = mcp.stdin.take().unwrap();
    for request in requests {
        use std::io::Write;
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let out = mcp.wait_with_output().unwrap();
    let reply: Value = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["id"] == 2)
        .unwrap();
    let usage = &reply["result"]["structuredContent"]["subscriptions"];
    assert_eq!(usage[0]["windows"][0]["used_percent"], 25.0, "{reply}");
    assert_eq!(usage[1]["windows"][1]["used_percent"], 9.0, "{reply}");
}
