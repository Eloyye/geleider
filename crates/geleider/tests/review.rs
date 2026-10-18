//! Exercise the complete CLI against a real daemon, git worktrees and fake
//! Harness/GitHub programs. These tests never start a model or use a Subscription.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_geleider");
/// `protocol::PROTOCOL`, which every request to the socket carries.
const PROTOCOL: u32 = 1;

struct Fixture {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
    head: String,
}

fn checked(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(
        checked(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap(),
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .into()
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(root.join("bin")).unwrap();
        git(&repo, &["init", "-q", "-b", "feat/x"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        std::fs::write(repo.join("code.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("code.txt"), "after\n").unwrap();
        git(&repo, &["commit", "-qam", "change"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        // The caller's uncommitted changes must not enter the review.
        std::fs::write(repo.join("code.txt"), "private local changes\n").unwrap();
        std::fs::write(
            root.join("pr.json"),
            json!({"number":7,"headRefName":"feat/x",
            "headRefOid":head,"baseRefOid":base,"isCrossRepository":false})
            .to_string(),
        )
        .unwrap();
        std::fs::write(root.join("config.toml"), format!(
            "notifications = false\npr_comments = false\n[projects.demo]\nrepo = 'example/demo'\npath = '{}'\n", repo.display())).unwrap();
        let mut fixture = Self {
            dir,
            daemon: None,
            head,
        };
        fixture.script(
            "gh",
            r#"#!/bin/sh
case "$1 $2" in
  'pr view') cat "$REVIEW_FIXTURE/pr.json" ;;
  'pr review')
    if [ -f "$REVIEW_FIXTURE/post-fails" ]; then exit 1; fi
    cat > "$REVIEW_FIXTURE/posted"
    printf '%s\n' "$@" > "$REVIEW_FIXTURE/post-args" ;;
  'pr list') printf '[]' ;;
  *) exit 1 ;;
esac
"#,
        );
        for harness in ["claude", "codex"] {
            fixture.script(
                harness,
                r#"#!/bin/sh
printf '%s\n' "${0##*/}" > "$REVIEW_FIXTURE/reviewer"
printf '%s\n' "$@" > "$REVIEW_FIXTURE/reviewer-args"
cat > "$REVIEW_FIXTURE/prompt"
git rev-parse HEAD > "$REVIEW_FIXTURE/review-head"
cat code.txt > "$REVIEW_FIXTURE/review-code"
"$GELEIDER_BIN" status --json > "$REVIEW_FIXTURE/during.json"
if [ -f "$REVIEW_FIXTURE/change-pr" ]; then cp "$REVIEW_FIXTURE/changed-pr.json" "$REVIEW_FIXTURE/pr.json"; fi
if [ -f "$REVIEW_FIXTURE/review-fails" ]; then echo failure >&2; exit 3; fi
if [ -f "$REVIEW_FIXTURE/review-empty" ]; then exit 0; fi
printf 'No actionable findings.\n'
"#,
            );
        }
        let log = std::fs::File::create(fixture.root().join("daemon.log")).unwrap();
        fixture.daemon = Some(
            fixture
                .command()
                .arg("daemon")
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let start = Instant::now();
        while !fixture
            .command()
            .arg("queue")
            .output()
            .unwrap()
            .status
            .success()
        {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "daemon did not start"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
        // Use known M2 readings, independent of the developer's real usage.
        for subscription in ["claude", "chatgpt"] {
            fixture.request(json!({"op":"report_usage","subscription":subscription,
                "readings":[{"minutes":300,"used_percent":23,"resets_at":4_000_000_000_000_i64}]}));
        }
        fixture
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn script(&self, name: &str, text: &str) {
        let path = self.root().join("bin").join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.root().join("repo"))
            .env("GELEIDER_CONFIG", self.root().join("config.toml"))
            .env("GELEIDER_STATE_DIR", self.root().join("state"))
            .env("CODEX_HOME", self.root().join("codex-home"))
            .env("GELEIDER_BIN", BIN)
            .env("REVIEW_FIXTURE", self.root())
            .env_remove("GELEIDER_REVIEW_SESSION")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            );
        cmd
    }

    fn request(&self, mut request: Value) {
        request["protocol"] = PROTOCOL.into();
        let mut socket = UnixStream::connect(self.root().join("state/geleider.sock")).unwrap();
        writeln!(socket, "{request}").unwrap();
        let mut reply = String::new();
        BufReader::new(socket).read_line(&mut reply).unwrap();
        assert!(
            serde_json::from_str::<Value>(&reply)
                .unwrap()
                .get("ok")
                .is_some(),
            "{reply}"
        );
    }

    /// Records a writing Session the way an installed hook does, then waits for
    /// the daemon, which records hook activity in the background.
    fn writer(&self, harness: &str, provider: Option<&str>) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let datagram = json!({"datagram":"activity",
            "key":{"harness":harness,"id":"writer"},"cwd":self.root().join("repo"),
            "state":"working","at":at,"wait_for":null,"resolved":null,"reset":true,
            "owner_pid":std::process::id(),"provider_id":provider,"review":false});
        UnixDatagram::unbound()
            .unwrap()
            .send_to(
                datagram.to_string().as_bytes(),
                self.root().join("state/hooks.sock"),
            )
            .unwrap();
        let start = Instant::now();
        while !self.sessions().iter().any(|s| s["id"] == "writer") {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "writer not recorded"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn review(&self, args: &[&str]) -> Output {
        self.command()
            .args(["review", "7"])
            .args(args)
            .output()
            .unwrap()
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root().join(name)).unwrap()
    }

    fn sessions(&self) -> Vec<Value> {
        let out = checked(self.command().args(["status", "--json"]).output().unwrap());
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessions"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn logs(&self) -> PathBuf {
        std::fs::read_dir(self.root().join("state/reviews"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

#[test]
fn cross_review_routes_by_vendor_shows_usage_tracks_session_and_posts_the_pinned_diff() {
    for (writer, provider, reviewer, plan) in [
        ("claude", None, "codex", "ChatGPT Plus"),
        ("codex", None, "claude", "Claude Pro"),
        ("opencode", Some("openai"), "claude", "Claude Pro"),
    ] {
        let fixture = Fixture::new();
        assert!(
            !fixture.root().join("reviewer").exists(),
            "daemon must never start reviews"
        );
        fixture.writer(writer, provider);
        let out = checked(fixture.review(&["--yes"]));
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            stdout.contains(plan) && stdout.contains("23% used"),
            "{stdout}"
        );
        assert!(stdout.find("23% used").unwrap() < stdout.find("Posted Cross-review").unwrap());
        assert_eq!(fixture.read("reviewer").trim(), reviewer);
        assert_eq!(fixture.read("review-head").trim(), fixture.head);
        assert_eq!(fixture.read("review-code"), "after\n");
        assert_eq!(
            std::fs::read_to_string(fixture.root().join("repo/code.txt")).unwrap(),
            "private local changes\n"
        );
        if reviewer == "claude" {
            assert!(fixture.read("prompt").contains("+after"));
            assert!(!fixture.read("prompt").contains("private local changes"));
            assert!(fixture.read("reviewer-args").contains("dontAsk"));
        } else {
            assert!(fixture.read("reviewer-args").contains("--base"));
            assert!(fixture.read("reviewer-args").contains("read-only"));
        }
        let during: Value = serde_json::from_str(&fixture.read("during.json")).unwrap();
        let review = during["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["review"] == true)
            .unwrap();
        assert_eq!(review["state"], "working");
        assert_eq!(review["harness"], reviewer);
        assert_eq!(review["project"], "demo");
        assert!(
            fixture
                .sessions()
                .iter()
                .any(|s| s["review"] == true && s["state"] == "ended")
        );
        assert!(fixture.read("posted").contains("No actionable findings."));
        assert!(fixture.read("posted").contains(&fixture.head));
        assert!(fixture.read("post-args").contains("--comment"));
        assert!(!fixture.logs().join("worktree").exists());
    }
}

#[test]
fn unknown_writer_requires_with_and_cancellation_spends_nothing() {
    let fixture = Fixture::new();
    let out = fixture.review(&["--yes"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--with claude|codex"));
    let out = checked(fixture.review(&["--with", "claude"])); // EOF is cancellation.
    assert!(String::from_utf8_lossy(&out.stdout).contains("Cancelled"));
    assert!(!fixture.root().join("reviewer").exists());
    assert!(!fixture.root().join("state/reviews").exists());
    checked(fixture.review(&["--with", "claude", "--yes"]));
    assert_eq!(fixture.read("reviewer").trim(), "claude");
}

#[test]
fn failures_and_empty_results_end_the_session_without_posting_and_post_failures_keep_output() {
    for mode in ["review-fails", "review-empty", "post-fails"] {
        let fixture = Fixture::new();
        fixture.writer("codex", None);
        std::fs::write(fixture.root().join(mode), "").unwrap();
        assert!(!fixture.review(&["--yes"]).status.success());
        assert!(!fixture.root().join("posted").exists());
        assert!(
            fixture
                .sessions()
                .iter()
                .any(|s| s["review"] == true && s["state"] == "ended")
        );
        let logs = fixture.logs();
        assert!(logs.join("stdout.txt").exists() && logs.join("stderr.txt").exists());
        assert!(!logs.join("worktree").exists());
        if mode == "post-fails" {
            assert!(
                std::fs::read_to_string(logs.join("stdout.txt"))
                    .unwrap()
                    .contains("No actionable findings")
            );
        }
    }
}

#[test]
fn a_pr_changed_during_review_is_not_posted() {
    let fixture = Fixture::new();
    fixture.writer("codex", None);
    let mut changed: Value = serde_json::from_str(&fixture.read("pr.json")).unwrap();
    changed["headRefOid"] = json!("a".repeat(40));
    std::fs::write(fixture.root().join("changed-pr.json"), changed.to_string()).unwrap();
    std::fs::write(fixture.root().join("change-pr"), "").unwrap();
    let output = fixture.review(&["--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("changed during review"));
    assert!(!fixture.root().join("posted").exists());
    assert!(fixture.logs().join("stdout.txt").exists());
}

#[test]
fn review_activity_requests_never_record_a_writing_session() {
    let fixture = Fixture::new();
    fixture.request(json!({"op":"review_activity","activity":{
        "key":{"harness":"codex","id":"forged"},"cwd":fixture.root().join("repo"),
        "state":"working","at":1,"wait_for":null,"resolved":null,"reset":true,
        "owner_pid":std::process::id(),"provider_id":null,"review":false}}));
    let forged = fixture.sessions();
    assert!(
        forged
            .iter()
            .any(|s| s["id"] == "forged" && s["review"] == true)
    );
    let out = fixture.review(&["--yes"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--with claude|codex"));
}
