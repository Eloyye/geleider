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
    /// Extra environment for the background process.
    daemon_env: Vec<(&'static str, String)>,
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
disposable = ["target/"]

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
            daemon_env: Vec::new(),
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
            .envs(self.daemon_env.iter().map(|(k, v)| (k, v)))
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
    // Long enough that the first Run is still playing when its Lease is checked,
    // even on a slow machine.
    let first = env.submit("play", "5");
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
    // Wait for the game itself, so the check sees it rather than only the Run.
    wait_until(Duration::from_secs(10), || {
        Command::new("pgrep")
            .args(["-x", &env.game])
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success()
    });
    let mut holders = Value::Null;
    wait_until(Duration::from_secs(10), || {
        let status = env.geleider(&["status", "--json"]);
        let status: Value = serde_json::from_slice(&status.stdout).unwrap();
        holders = status["resources"][0]["holders"].clone();
        holders.as_array().is_some_and(|h| !h.is_empty())
    });
    assert_eq!(
        holders,
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

/// The submit_run arguments a Harness's hook sends on for `session_id`, with
/// the nonce it attested them with.
fn attested(env: &Env, harness: &str, session_id: &str, tool_input: Value) -> Value {
    attested_call(
        env,
        harness,
        session_id,
        "submit_run",
        &env.repo(),
        tool_input,
    )
}

/// The arguments a Harness's hook sends on for a call of geleider's `tool` by
/// `session_id` working in `cwd`.
fn attested_call(
    env: &Env,
    harness: &str,
    session_id: &str,
    tool: &str,
    cwd: &Path,
    tool_input: Value,
) -> Value {
    let (event, tool) = match harness {
        "opencode" => ("tool.before", format!("geleider_{tool}")),
        _ => ("PreToolUse", format!("mcp__geleider__{tool}")),
    };
    let out = hook(
        env,
        harness,
        event,
        serde_json::json!({"session_id":session_id,"cwd":cwd,"tool_name":tool,"tool_input":tool_input}),
    );
    let output: Value = serde_json::from_slice(&out.stdout).unwrap();
    output["hookSpecificOutput"]["updatedInput"].clone()
}

/// A `geleider mcp` server that has been initialized by `client`.
struct Mcp {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn start(env: &Env, client: &str) -> Self {
        let mut child = env
            .command()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        mcp.rpc(
            "initialize",
            serde_json::json!({"clientInfo":{"name":client}}),
        );
        mcp
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        use std::io::{BufRead, Write};
        self.next_id += 1;
        let message =
            serde_json::json!({"jsonrpc":"2.0","id":self.next_id,"method":method,"params":params});
        writeln!(self.stdin, "{message}").unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// The Run a submit_run call with `arguments` queued.
    fn submit(&mut self, arguments: Value) -> Value {
        let reply = self.rpc(
            "tools/call",
            serde_json::json!({"name":"submit_run","arguments":arguments}),
        );
        let run = reply["result"]["structuredContent"].clone();
        assert!(run["id"].is_i64(), "{reply}");
        run
    }

    /// The result of calling `tool`: its JSON, or `Err` with its error text.
    fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, String> {
        let reply = self.rpc(
            "tools/call",
            serde_json::json!({"name":tool,"arguments":arguments}),
        );
        let result = &reply["result"];
        if result["isError"] == true {
            return Err(result["content"][0]["text"].as_str().unwrap().to_string());
        }
        Ok(result["structuredContent"].clone())
    }

    fn stop(mut self) {
        drop(self.stdin);
        self.child.wait().unwrap();
    }
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
    let session = &output["hookSpecificOutput"]["updatedInput"]["session"];
    assert_eq!(session["harness"], "codex");
    assert_eq!(session["id"], "right-thread");
    assert_eq!(session["nonce"].as_str().unwrap().len(), 64);
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
    let args = attested(
        &env,
        "codex",
        "the-submitting-session",
        serde_json::json!({"job":"script","args":["delivery log"]}),
    );
    let mut mcp = Mcp::start(&env, "codex");
    let run = mcp.submit(args);
    mcp.stop();
    assert!(run["delivery"].as_str().unwrap().starts_with("on"), "{run}");
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
    let mut env = Env::new("fakegame-shared");
    let mut mcp = Mcp::start(&env, "opencode");
    let mut ids = Vec::new();
    for session in ["first", "second"] {
        let args = attested(
            &env,
            "opencode",
            session,
            json!({"job":"hold","args":["0"]}),
        );
        let run = mcp.submit(args);
        assert_eq!(run["session"]["id"], session);
        ids.push(id(&run));
    }
    mcp.stop();
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

    // Status shows each window, marking the last-known ones by their age.
    let text = String::from_utf8_lossy(&env.geleider(&["status"]).stdout).to_string();
    assert!(
        text.contains("Claude Pro") && text.contains(" 25% "),
        "{text}"
    );
    assert!(
        text.contains(" 31% ") && text.contains(", 2h old"),
        "{text}"
    );
    let text = String::from_utf8_lossy(&env.geleider(&["usage"]).stdout).to_string();
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

#[test]
fn only_hook_attested_destinations_are_delivered_and_each_attestation_works_once() {
    use serde_json::json;
    let env = Env::new("fakegame-attest");
    let job = json!({"job":"hold","args":["0"]});
    let mut mcp = Mcp::start(&env, "codex");
    let undelivered = |run: &Value| {
        assert!(run["session"].is_null(), "{run}");
        assert!(
            run["delivery"].as_str().unwrap().starts_with("off"),
            "{run}"
        );
    };

    let args = attested(&env, "codex", "caller", job.clone());
    let run = mcp.submit(args.clone());
    assert_eq!(run["session"], json!({"harness":"codex","id":"caller"}));
    assert!(run["delivery"].as_str().unwrap().starts_with("on"), "{run}");
    // Replayed: the nonce was used up by the first submission.
    undelivered(&mcp.submit(args));

    // Forged by the model, as when hooks do not run: no nonce, a nonce no hook
    // sent, and a real nonce moved to another Session, which also uses it up.
    let mut forged = job.clone();
    forged["session"] = json!({"harness":"codex","id":"victim"});
    undelivered(&mcp.submit(forged.clone()));
    forged["session"]["nonce"] = json!("ab".repeat(32));
    undelivered(&mcp.submit(forged.clone()));
    let args = attested(&env, "codex", "caller", job.clone());
    forged["session"]["nonce"] = args["session"]["nonce"].clone();
    undelivered(&mcp.submit(forged));
    undelivered(&mcp.submit(args));
    mcp.stop();

    // A shell's CODEX_THREAD_ID is the model's to set, so it names nothing.
    let out = env
        .command()
        .env("CODEX_THREAD_ID", "victim")
        .args(["run", "hold", "0", "--json"])
        .output()
        .unwrap();
    undelivered(&serde_json::from_slice(&out.stdout).unwrap());
}

#[test]
fn an_attestation_expires_if_its_submission_comes_too_late() {
    use serde_json::json;
    let mut env = Env::new("fakegame-attest-expiry");
    env.kill_daemon();
    env.daemon_env
        .push(("GELEIDER_ATTESTATION_TTL_MS", "1000".into()));
    env.start_daemon();
    let job = json!({"job":"hold","args":["0"]});
    let mut mcp = Mcp::start(&env, "codex");
    let late = attested(&env, "codex", "caller", job.clone());
    std::thread::sleep(Duration::from_millis(1500));
    let run = mcp.submit(late);
    assert!(run["session"].is_null(), "{run}");
    let prompt = attested(&env, "codex", "caller", job);
    assert_eq!(mcp.submit(prompt)["session"]["id"], "caller");
    mcp.stop();
}

fn fake_program(env: &Env, name: &str, source: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = env.root().join("bin").join(name);
    std::fs::write(&path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn fake_branch_pr(env: &Env, state: &str) -> Value {
    let out = Command::new("git")
        .arg("-C")
        .arg(env.repo())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let pr = serde_json::json!({"number":6,"state":state,"headRefOid":head,
        "isCrossRepository":false,"url":"https://github.com/example/demo/pull/6",
        "statusCheckRollup":[{"__typename":"CheckRun","name":"test","status":"COMPLETED",
            "conclusion":"FAILURE","detailsUrl":"job/1","startedAt":"t0","completedAt":"t1"}]});
    std::fs::write(
        env.root().join("branch-pr.json"),
        serde_json::to_string(&vec![&pr]).unwrap(),
    )
    .unwrap();
    fake_program(
        env,
        "gh",
        r#"#!/usr/bin/env python3
import sys,json,pathlib
root = pathlib.Path(__file__).parent.parent
assert sys.argv[1:3] == ['pr','list'], sys.argv
with (root / 'gh-calls').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
if (root / 'gh-offline').exists(): sys.exit(1)
print((root / 'branch-pr.json').read_text())
"#,
    );
    pr
}

fn clean_input(env: &Env, args: &[&str], input: &str) -> Output {
    use std::io::Write;
    let mut child = env
        .command()
        .arg("clean")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
fn finished_worktrees_are_listed_but_only_removed_after_explicit_confirmation() {
    let env = Env::new("fakegame-clean");
    fake_branch_pr(&env, "MERGED");
    let wt = env.root().join("finished worktree");
    git(
        &env.repo(),
        &["worktree", "add", "-qb", "done", wt.to_str().unwrap()],
    );
    let status: Value =
        serde_json::from_slice(&env.geleider(&["status", "--json"]).stdout).unwrap();
    assert_eq!(
        status["finished_worktrees"].as_array().unwrap().len(),
        1,
        "{status}"
    );
    let text = String::from_utf8(env.geleider(&["status"]).stdout).unwrap();
    assert!(
        text.contains("Finished worktrees") && text.contains("done"),
        "{text}"
    );
    // Status uses the same cached lookup, regardless of how often it is called.
    assert_eq!(
        std::fs::read_to_string(env.root().join("gh-calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    for input in ["", "\n", "no\n"] {
        let out = clean_input(&env, &[], input);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(wt.exists());
    }
    let out = clean_input(&env, &[], "yes\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!wt.exists());
    assert!(env.repo().exists());
}

#[test]
fn clean_removes_only_the_named_finished_worktrees() {
    let env = Env::new("fakegame-clean-targets");
    fake_branch_pr(&env, "MERGED");
    let [one, two, three] = ["one", "two", "three"].map(|b| {
        let wt = env.root().join(b);
        git(
            &env.repo(),
            &["worktree", "add", "-qb", b, wt.to_str().unwrap()],
        );
        wt
    });
    // Every fake worktree has PR #6, so the bare number is ambiguous.
    for bad in [&["6"][..], &[one.to_str().unwrap(), "nowhere"][..]] {
        let out = clean_input(&env, bad, "yes\n");
        assert!(!out.status.success());
        assert!(one.exists() && two.exists() && three.exists());
    }
    let out = clean_input(&env, &[one.to_str().unwrap()], "no\n");
    assert!(out.status.success());
    assert!(one.exists());
    let out = clean_input(&env, &[one.to_str().unwrap()], "yes\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    // The question goes to stderr, so stdout keeps only what was done.
    assert!(stderr.contains("Remove this worktree? [y/N]"), "{stderr}");
    assert!(
        !stdout.contains("[y/N]") && stdout.contains("Removed "),
        "{stdout}"
    );
    assert!(!one.exists() && two.exists() && three.exists());
    let out = clean_input(&env, &[two.to_str().unwrap(), "--yes"], "");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!two.exists() && three.exists());
}

#[test]
fn without_a_terminal_clean_offers_every_finished_worktree_and_a_piped_yes_removes_them() {
    let env = Env::new("fakegame-clean-several");
    fake_branch_pr(&env, "MERGED");
    let [one, two] = ["one", "two"].map(|b| {
        let wt = env.root().join(b);
        git(
            &env.repo(),
            &["worktree", "add", "-qb", b, wt.to_str().unwrap()],
        );
        wt
    });
    let out = clean_input(&env, &[], "no\n");
    assert!(out.status.success());
    assert!(one.exists() && two.exists());
    let out = clean_input(&env, &[], "y\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    // The checkboxes all start checked, so nothing is asked before the confirmation.
    assert!(
        stderr.contains("#6 merged  one") && stderr.contains("#6 merged  two"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Remove these 2 worktrees? [y/N]"),
        "{stderr}"
    );
    assert!(!one.exists() && !two.exists());
}

#[test]
fn build_output_does_not_keep_a_finished_worktree_but_other_ignored_files_do() {
    let env = Env::new("fakegame-clean-ignored");
    std::fs::write(env.repo().join(".gitignore"), "target/\n.env\n").unwrap();
    git(&env.repo(), &["add", ".gitignore"]);
    git(
        &env.repo(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "ignore",
        ],
    );
    fake_branch_pr(&env, "MERGED");
    let built = env.root().join("built");
    let secret = env.root().join("secret");
    for (branch, wt) in [("built", &built), ("secret", &secret)] {
        git(
            &env.repo(),
            &["worktree", "add", "-qb", branch, wt.to_str().unwrap()],
        );
    }
    std::fs::create_dir_all(built.join("target/debug")).unwrap();
    std::fs::write(built.join("target/debug/demo"), "binary").unwrap();
    std::fs::write(secret.join(".env"), "SECRET=1").unwrap();
    let status: Value =
        serde_json::from_slice(&env.geleider(&["status", "--json"]).stdout).unwrap();
    let listed = status["finished_worktrees"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{status}");
    assert_eq!(listed[0]["branch"], "built", "{status}");
    let out = clean_input(&env, &[], "yes\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!built.exists());
    assert!(secret.join(".env").exists());
}

#[test]
fn status_answers_promptly_while_github_hangs_and_lists_the_worktree_once_it_answers() {
    let env = Env::new("fakegame-clean-slow");
    fake_branch_pr(&env, "MERGED");
    // A `gh` that hangs, as with an unreachable GitHub, until the test lets it answer.
    fake_program(
        &env,
        "gh",
        r#"#!/usr/bin/env python3
import pathlib,time
root = pathlib.Path(__file__).parent.parent
while not (root / 'gh-answer').exists(): time.sleep(0.05)
print((root / 'branch-pr.json').read_text())
"#,
    );
    let wt = env.root().join("finished");
    git(
        &env.repo(),
        &["worktree", "add", "-qb", "done", wt.to_str().unwrap()],
    );
    let status = |args: &[&str]| {
        let start = Instant::now();
        let out = env.geleider(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(start.elapsed() < Duration::from_secs(10), "{args:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let json: Value = serde_json::from_str(&status(&["status", "--json"])).unwrap();
    assert_eq!(json["cleanup_pending"], true, "{json}");
    assert_eq!(json["finished_worktrees"], serde_json::json!([]), "{json}");
    assert!(status(&["status"]).contains("still being checked"));

    std::fs::write(env.root().join("gh-answer"), "").unwrap();
    wait_until(Duration::from_secs(10), || {
        let json: Value = serde_json::from_str(&status(&["status", "--json"])).unwrap();
        json["finished_worktrees"].as_array().unwrap().len() == 1
            && json.get("cleanup_pending").is_none()
    });
}

#[test]
fn cleanup_rechecks_pr_and_session_state_after_the_confirmation_prompt() {
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    let env = Env::new("fakegame-clean-race");
    fake_branch_pr(&env, "MERGED");
    let wt = env.root().join("finished");
    git(
        &env.repo(),
        &["worktree", "add", "-qb", "done", wt.to_str().unwrap()],
    );
    for change in ["reopen", "session", "offline", "dirty"] {
        fake_branch_pr(&env, "MERGED");
        let mut child = env
            .command()
            .arg("clean")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        // Seeing the displayed candidate means the initial scan has completed.
        loop {
            line.clear();
            assert!(stdout.read_line(&mut line).unwrap() > 0);
            if line.contains("#6 merged") {
                break;
            }
        }
        match change {
            "reopen" => {
                fake_branch_pr(&env, "OPEN");
            }
            "session" => {
                hook(
                    &env,
                    "codex",
                    "SessionStart",
                    json!({"session_id":"new-session","cwd":wt}),
                );
                wait_until(Duration::from_secs(5), || {
                    sessions(&env)
                        .iter()
                        .any(|s| s["id"] == "new-session" && s["state"] == "idle")
                });
            }
            "offline" => {
                std::fs::write(env.root().join("gh-offline"), "").unwrap();
            }
            "dirty" => {
                std::fs::write(wt.join("new-file"), "new work").unwrap();
            }
            _ => unreachable!(),
        }
        child.stdin.take().unwrap().write_all(b"y\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(!out.status.success(), "{change}");
        assert!(wt.exists(), "{change}");
        if change == "session" {
            hook(
                &env,
                "codex",
                "SessionEnd",
                json!({"session_id":"new-session","cwd":wt}),
            );
            wait_until(Duration::from_secs(5), || {
                sessions(&env).iter().all(|s| s["state"] == "ended")
            });
        }
        if change == "offline" {
            std::fs::remove_file(env.root().join("gh-offline")).unwrap();
        }
    }
}

#[test]
fn ci_notifications_go_only_to_the_user_and_do_not_repeat_after_restart() {
    use serde_json::json;
    let mut env = Env::new("fakegame-ci");
    let mut pr = fake_branch_pr(&env, "OPEN");
    fake_program(
        &env,
        "osascript",
        r#"#!/usr/bin/env python3
import sys,json,pathlib
root = pathlib.Path(__file__).parent.parent
with (root / 'notifications').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
"#,
    );
    // A notification must never queue a message back into a Session.
    fake_program(
        &env,
        "codex",
        r#"#!/bin/sh
touch "$(dirname "$0")/../unexpected-session-message"
exit 1
"#,
    );
    let config_path = env.root().join("config.toml");
    let config = std::fs::read_to_string(&config_path)
        .unwrap()
        .replace("notifications = false", "notifications = true");
    std::fs::write(config_path, config).unwrap();
    hook(
        &env,
        "codex",
        "SessionStart",
        json!({"session_id":"ci-session","cwd":env.repo()}),
    );
    wait_until(Duration::from_secs(5), || !sessions(&env).is_empty());
    env.kill_daemon();
    env.start_daemon();
    let notification_path = env.root().join("notifications");
    let count = || {
        std::fs::read_to_string(&notification_path)
            .unwrap_or_default()
            .lines()
            .count()
    };
    wait_until(Duration::from_secs(5), || count() == 1);
    let calls_path = env.root().join("gh-calls");
    let calls = || {
        std::fs::read_to_string(&calls_path)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let before = calls();
    env.kill_daemon();
    env.start_daemon();
    wait_until(Duration::from_secs(5), || calls() > before);
    assert_eq!(count(), 1);
    pr["headRefOid"] = json!("another-pushed-commit");
    std::fs::write(
        env.root().join("branch-pr.json"),
        serde_json::to_string(&vec![pr]).unwrap(),
    )
    .unwrap();
    env.kill_daemon();
    env.start_daemon();
    wait_until(Duration::from_secs(5), || count() == 2);
    let notices = std::fs::read_to_string(env.root().join("notifications")).unwrap();
    assert!(
        notices.contains("CI failed") && notices.contains("#6"),
        "{notices}"
    );
    assert!(!env.root().join("unexpected-session-message").exists());
}

/// A stand-in for `gh`, signed in as `tester`, that keeps issues of
/// `example/demo` in `gh-issues.json`.
const FAKE_GH: &str = r#"#!/usr/bin/env python3
import sys, json, pathlib
root = pathlib.Path(__file__).parent.parent
path = root / 'gh-issues.json'
args = sys.argv[1:]
method = args[args.index('-X') + 1] if '-X' in args else 'GET'
target = next(a for a in args[1:] if a == 'user' or a.startswith('repos/'))
if target == 'user':
    print(json.dumps({'login': 'tester'}))
    sys.exit(0)
state = json.loads(path.read_text())
parts = target.split('/')
issue = state[parts[4]]
body = json.load(sys.stdin) if '--input' in args else None
if parts[5:] == ['assignees']:
    issue['assignees'] = [a for a in issue['assignees'] if a not in body['assignees']]
    if method == 'POST':
        issue['assignees'] += body['assignees']
elif parts[5:] == ['comments']:
    issue['comments'].append(body['body'])
path.write_text(json.dumps(state))
print(json.dumps({'state': issue['state'], 'assignees': [{'login': a} for a in issue['assignees']]}))
"#;

fn issues(env: &Env) -> Value {
    serde_json::from_str(&std::fs::read_to_string(env.root().join("gh-issues.json")).unwrap())
        .unwrap()
}

fn claims(env: &Env) -> Vec<Value> {
    let out = env.geleider(&["status", "--json"]);
    serde_json::from_slice::<Value>(&out.stdout).unwrap()["claims"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn one_session_claims_an_issue_across_harnesses_until_it_closes_or_is_released() {
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new("fakegame-claims");
    let gh = env.root().join("bin/gh");
    std::fs::write(&gh, FAKE_GH).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let open = json!({"state":"open","assignees":[],"comments":[]});
    std::fs::write(
        env.root().join("gh-issues.json"),
        json!({"5": open, "6": open}).to_string(),
    )
    .unwrap();
    env.daemon_env
        .push(("GELEIDER_CLAIM_CHECK_MS", "200".into()));
    env.daemon_env
        .push(("GELEIDER_CLAIM_QUIET_MS", "1500".into()));
    env.kill_daemon();
    env.start_daemon();

    let worktree = |name: &str, branch: &str| {
        let wt = env.root().join(name);
        git(
            &env.repo(),
            &["worktree", "add", "-q", "-b", branch, wt.to_str().unwrap()],
        );
        wt
    };
    let claude_wt = worktree("wt-claude", "claude/issue-5-a");
    let codex_wt = worktree("wt-codex", "codex/issue-5-b");
    let in_dir = |dir: &Path, args: &[&str]| {
        let mut cmd = env.command();
        cmd.current_dir(dir).args(args);
        cmd.output().unwrap()
    };

    // A Claude Code Session claims #5 from the command line in its worktree.
    hook(
        &env,
        "claude",
        "SessionStart",
        json!({"session_id":"claude-1","cwd":claude_wt}),
    );
    wait_until(Duration::from_secs(5), || sessions(&env).len() == 1);
    let out = in_dir(&claude_wt, &["claim", "5"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Claimed example/demo#5"));
    // Claiming again is a no-op, not a second comment.
    assert!(in_dir(&claude_wt, &["claim", "#5"]).status.success());
    let issue = &issues(&env)["5"];
    assert_eq!(issue["assignees"], json!(["tester"]));
    assert_eq!(
        issue["comments"],
        json!(["Claimed by Claude Code session claude-1 on `claude/issue-5-a`."])
    );

    // A Codex Session starting on a branch for #5 is warned, but claims nothing.
    let out = hook(
        &env,
        "codex",
        "SessionStart",
        json!({"session_id":"codex-1","cwd":codex_wt}),
    );
    let output: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    let warning = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        warning.contains("example/demo#5 is claimed by Claude Code session claude-1"),
        "{warning}"
    );
    assert_eq!(claims(&env).len(), 1);

    // Its claim through MCP fails, naming the holder.
    let mut mcp = Mcp::start(&env, "codex");
    let args = attested_call(
        &env,
        "codex",
        "codex-1",
        "claim",
        &codex_wt,
        json!({"issue":5}),
    );
    let err = mcp.call("claim", args).unwrap_err();
    assert!(
        err.contains("already claimed by Claude Code session claude-1"),
        "{err}"
    );
    assert_eq!(issues(&env)["5"]["comments"].as_array().unwrap().len(), 1);

    // A quiet holder is flagged in status, and keeps its Claim.
    std::thread::sleep(Duration::from_millis(1700));
    let held = claims(&env);
    assert_eq!(held[0]["session"]["id"], "claude-1");
    assert_eq!(held[0]["quiet"], true);
    let text = String::from_utf8(env.geleider(&["status"]).stdout).unwrap();
    assert!(
        text.lines().any(|l| l.contains("! demo#5 ")
            && l.contains("claude claude-1")
            && l.contains("quiet ")),
        "{text}"
    );
    assert!(
        text.contains("geleider release example/demo#5 --force"),
        "{text}"
    );

    // Closing the issue ends the Claim.
    let mut state = issues(&env);
    state["5"]["state"] = json!("closed");
    std::fs::write(env.root().join("gh-issues.json"), state.to_string()).unwrap();
    wait_until(Duration::from_secs(5), || claims(&env).is_empty());
    let out = hook(
        &env,
        "codex",
        "SessionStart",
        json!({"session_id":"codex-1","cwd":codex_wt}),
    );
    assert!(out.stdout.is_empty());

    // Codex claims #6. Only it can release it, unless forced.
    let args = attested_call(
        &env,
        "codex",
        "codex-1",
        "claim",
        &codex_wt,
        json!({"issue":"#6"}),
    );
    let claimed = mcp.call("claim", args).unwrap();
    assert_eq!(claimed["claim"]["session"]["id"], "codex-1");
    let out = in_dir(&claude_wt, &["release", "6"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("only that Session can release it"));
    let args = attested_call(
        &env,
        "codex",
        "codex-1",
        "release",
        &codex_wt,
        json!({"issue":6}),
    );
    let released = mcp.call("release", args).unwrap();
    assert!(
        released["message"]
            .as_str()
            .unwrap()
            .starts_with("Released example/demo#6")
    );
    assert!(claims(&env).is_empty());
    let issue = &issues(&env)["6"];
    assert_eq!(issue["assignees"], json!([]));
    assert_eq!(
        issue["comments"][1],
        "Claim released (was held by Codex session codex-1 on `codex/issue-5-b`)."
    );

    // From a terminal, a Claim another Session holds is ended with --force.
    let args = attested_call(
        &env,
        "codex",
        "codex-1",
        "claim",
        &codex_wt,
        json!({"issue":6}),
    );
    mcp.call("claim", args).unwrap();
    mcp.stop();
    let out = env.geleider(&["release", "example/demo#6", "--force"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(claims(&env).is_empty());
    assert_eq!(issues(&env)["6"]["assignees"], json!([]));
}
