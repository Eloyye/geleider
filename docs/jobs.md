# Jobs, Resources and the Run queue

A Session in any Harness submits a Job that needs a Resource, such as Unity PlayMode tests that launch the Unity Editor. geleider queues each Run, starts it outside any sandbox once a slot is free, and reports the result. No two Runs share a one-slot Resource, and an Editor you open by hand holds a slot too.

Declare Resources, Projects and Jobs in `~/.config/geleider/config.toml` (see the [configuration reference](configuration.md)). Jobs come only from this file, never from a Project's repo:

```toml
[resources.unity]
slots = 1
process = "Unity"         # every running process with this name holds a Lease

[projects.lanternfall]
repo = "Eloyye/lanternfall"
path = "~/code/lanternfall"  # the clone; its worktrees belong to the Project too
vars = { unity = "/Applications/Unity/Hub/Editor/6000.0.23f1/Unity.app/Contents/MacOS/Unity" }

[projects.lanternfall.jobs.playmode-test]
resource = "unity"
args = ["suite"]
# Split like a shell would but never run through one, so args cannot inject
# commands. A relative program path is relative to the worktree. {worktree} is built in.
command = "scripts/run-playmode.sh {suite} {unity}"
```

## Submitting Runs

From any worktree of the Project:

```sh
geleider run playmode-test combat         # returns at once with the Run id and queue position
geleider run playmode-test combat --wait  # blocks, then exits with the Run's exit code
geleider show 12                          # state, exit code, duration, log path and tail
geleider wait 12
geleider queue [--all]
geleider status                           # Sessions, Claims, Resources and their holders, Runs
```

Sessions can do the same through the [MCP tools](mcp.md) `submit_run`, `get_run`, `list_runs` and `status`.

## Results

Codex receives completed Run results through `codex queue`; OpenCode receives queued messages through its plugin. Failed deliveries stay in SQLite for retry. Claude Code continues to use `geleider run … --wait`.

When a Run finishes, geleider sends a macOS notification and, if the worktree's branch has an open PR, posts the result there through `gh`. Each Job gets one comment per PR, edited in place on later Runs. It also notifies you when a queued Run starts because a slot freed up. Set `notifications = false` or `pr_comments = false` at the top of the config to turn these off.

If the background process restarts while a Run is running, the Run keeps its Lease until its process exits. It then ends as `lost`, since its exit code is unknown; its log is complete.

## Files

State lives in `~/Library/Application Support/geleider/`: the SQLite database, the socket and each Run's full log under `runs/`. The background process logs to `~/Library/Logs/geleider/daemon.log`.

A Run executes outside any sandbox. Read the [security model](security.md) before declaring Jobs.
