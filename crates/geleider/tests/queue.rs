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
