# geleider

Coordinates coding-agent work on one Mac across Claude Code, Codex and OpenCode. See [CONTEXT.md](CONTEXT.md) for the vocabulary and [docs/adr](docs/adr) for the decisions behind it.

## Resource Leases and the Job queue

A Session in any Harness submits a Job that needs a Resource, such as integration tests that launch CK3. geleider queues each Run, starts it outside any sandbox once a slot is free, and reports the result. No two Runs share a one-slot Resource, and a game you start by hand holds a slot too.

```sh
cargo install --path crates/geleider
geleider install      # launchd login service + MCP server in Claude Code, Codex, OpenCode
```

Then declare Projects, Resources and Jobs in `~/.config/geleider/config.toml`. Jobs come only from this file, never from a Project's repo:

```toml
[resources.ck3]
slots = 1
process = "ck3"           # every running process with this name holds a Lease

[projects.queendom]
repo = "Eloyye/queendom"
path = "~/code/queendom"  # the clone; its worktrees belong to the Project too
vars = { ck3_dir = "/path/to/Crusader Kings III" }

[projects.queendom.jobs.ck3-test]
resource = "ck3"
args = ["suite"]
# Split like a shell would but never run through one, so args cannot inject
# commands. A relative program path is relative to the worktree. {worktree} is built in.
command = "scripts/test-engine.sh {suite} {ck3_dir}"
```

From any worktree of the Project:

```sh
geleider run ck3-test events          # returns at once with the Run id and queue position
geleider run ck3-test events --wait   # blocks, then exits with the Run's exit code
geleider show 12                      # state, exit code, duration, log path and tail
geleider wait 12
geleider queue [--all]
geleider status                       # Resources, their holders, running and queued Runs
```

The MCP server (`geleider mcp`) offers the same as the tools `submit_run`, `get_run`, `list_runs` and `status`. In Codex, `geleider install` pre-approves these tools (`default_tools_approval_mode = "approve"`), since `codex exec` cannot ask. They can only queue Jobs from geleider's own config, and the Session itself stays in its sandbox, but see [Trust boundary](#trust-boundary). Set it to `"prompt"` in `~/.codex/config.toml` to be asked each time; reinstalling keeps your choice.

When a Run finishes, geleider sends a macOS notification and, if the worktree's branch has an open PR, posts the result there through `gh`. Each Job gets one comment per PR, edited in place on later Runs. It also notifies you when a queued Run starts because a slot freed up. Set `notifications = false` or `pr_comments = false` at the top of the config to turn these off.

State lives in `~/Library/Application Support/geleider/`: the SQLite database, the socket and each Run's full log under `runs/`. The background process logs to `~/Library/Logs/geleider/daemon.log`.

### Trust boundary

A Run executes outside any sandbox, in the submitting Session's worktree. geleider fixes *which* command runs, but the files it runs, such as `scripts/test-engine.sh` and the tests it calls, come from that worktree, and a Session can edit them. So a Session that can submit a Job can run code outside its sandbox, with your network access and files. Only declare Jobs for Projects whose Sessions you would trust with that, and leave Codex's approval mode at `"prompt"` if you want to see each submission first. In addition:

- A Run only starts in the Project's clone or in a worktree that clone has registered (`git worktree list`). A directory merely pointed at the clone's repository through a hand-written `.git` file is refused.
- geleider runs git in the caller's directory with `core.fsmonitor` and hooks switched off, so repository settings an agent wrote cannot run commands.
- geleider's state directory, socket and Run logs are readable only by you. `install` writes Harness configs, which can hold credentials, without ever making them readable by others, and never writes through a symlink planted at its temp or backup file.
- When `pr_comments` is on, the last 40 lines of each Run's log are posted to the PR. Don't print secrets in Jobs on repositories others can read.

If the background process restarts while a Run is running, the Run keeps its Lease until its process exits. It then ends as `lost`, since its exit code is unknown; its log is complete.

## Subscription usage

`geleider status` (or `geleider usage`) shows how much of each Subscription is used in each Limit window and when each window resets, so you can see what is left before starting work. The MCP tool `usage` returns the same data. geleider only shows these numbers; it never routes work by them.

```text
Subscriptions
  Claude Pro  (from Claude Code's status line; CLI sessions only)
    5h   23% used, resets 14:05 (in 2h 10m)  updated 3m ago
    7d   41% used, resets Fri 09:00 (in 3d 04h)  updated 3m ago
  ChatGPT Plus  (from Codex's session logs)
    5h   12% used, resets 13:00 (in 1h 05m)  last known at 09:12 (2h 50m ago)
    7d  reset at 11:00, no data since  last known at 09:12 (2h 50m ago)
```

The numbers come only from what each Harness reports itself. geleider calls no usage endpoints, never reads or stores login tokens, and never estimates from token counts:

- **Claude Pro**: `geleider install` sets Claude Code's `statusLine` to `geleider statusline`, which passes `rate_limits.five_hour` and `rate_limits.seven_day` to geleider and prints a short status line. If you already had a status line, it is wrapped (`geleider statusline --wrap '<your command>'`) and its output is shown unchanged; `geleider uninstall` puts it back. Claude Code runs status lines only in interactive CLI sessions, not in the desktop app or `claude -p`, so these numbers update only while a CLI session runs.
- **ChatGPT Plus** (shared by Codex and OpenCode's OpenAI login): the newest `rate_limits` entry in Codex's session logs under `~/.codex/sessions/` (or `$CODEX_HOME/sessions/`): a 300-minute and a 10,080-minute window.
- **OpenCode Go** is not tracked. Its usage is shown only in the OpenCode console; the one usage endpoint, `/zen/go/v1/usage`, is undocumented and needs the Go API key.

Numbers more than ten minutes old are shown as "last known at", with their age. A window whose reset time has passed shows no percentage until the Harness reports again.

When a window resets after reaching 80% used, geleider sends a macOS notification. Windows used less reset quietly, so a lightly used 5-hour window does not notify every 5 hours. Configure it in `~/.config/geleider/config.toml`:

```toml
[subscriptions]
notify_resets = 80        # a percentage, or false for never
[subscriptions.claude]
plan = "Claude Max"       # the name shown; defaults to Claude Pro
[subscriptions.chatgpt]
enabled = false           # stop showing a Subscription
```
