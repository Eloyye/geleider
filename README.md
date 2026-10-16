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

The MCP server (`geleider mcp`) offers the same as the tools `submit_run`, `get_run`, `list_runs` and `status`.

When a Run finishes, geleider sends a macOS notification and, if the worktree's branch has an open PR, posts the result there through `gh`. Each Job gets one comment per PR, edited in place on later Runs. It also notifies you when a queued Run starts because a slot freed up. Set `notifications = false` or `pr_comments = false` at the top of the config to turn these off.

State lives in `~/Library/Application Support/geleider/`: the SQLite database, the socket and each Run's full log under `runs/`. The background process logs to `~/Library/Logs/geleider/daemon.log`.

If the background process restarts while a Run is running, the Run keeps its Lease until its process exits. It then ends as `lost`, since its exit code is unknown; its log is complete.
