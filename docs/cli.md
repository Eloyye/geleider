# CLI reference

Run `geleider <command> --help` for the exact flags of your version. Commands that take `--json` print machine-readable output instead of text.

## Jobs and Runs

See [Jobs and Resources](jobs.md).

| Command | What it does |
| --- | --- |
| `geleider run <job> [args…] [--wait] [--json]` | Submit a Run of a Job in the current worktree. Returns at once with the Run id and queue position. `--wait` blocks until the Run ends and exits with its exit code. Put `--` before args that start with `-`. |
| `geleider show <id> [--json]` | A Run's state and, once it has started, its exit code, duration, log path and log tail. |
| `geleider wait <id> [--json]` | Block until a Run ends and exit with its exit code. |
| `geleider queue [--all] [--json]` | Running and queued Runs. `--all` also lists recently finished ones. |

## Overview

| Command | What it does |
| --- | --- |
| `geleider status [--json]` | Panels for Subscription usage, live Sessions (waiting first) with their Claims, Resources with their running and queued Runs and any process holding them, and Claims and Finished worktrees, with the command to release a quiet Claim or clean up. Two columns from 110 columns wide, stacked below that. Coloured on a terminal unless `NO_COLOR` is set. |
| `geleider usage [--json]` | How much of each Subscription is used in each Limit window, and when each resets. See [Subscription usage](subscription-usage.md). |

```text
╭─ Usage ──────────────────────────────────────────────────────────────────────╮
│ Claude Max    5h ██░░░░░░░░ 23%  ↺ 14:53                                     │
│               7d ████░░░░░░ 41%  ↺ Thu 12:43                                 │
│ ChatGPT Plus  5h █████████░ 87%  ↺ 13:24                                     │
│               7d ██████░░░░ 62%  ↺ Sat 12:43, 25m old                        │
╰──────────────────────────────────────────────────────────────────────────────╯
╭─ Sessions · 3 ───────────────────────────────────────────────────────────────╮
│ ◐ lanternfall   claude    #42  claude/issue-42                               │
│   waiting for 4m · 7f3a1c2e                                                  │
│ ● lanternfall   codex     #38  codex/issue-38                                │
│   working for 20s · 019a2b3c                                                 │
│ ○ geleider      opencode       feat/colors                                   │
│   idle for 42m · ses_01JB                                                    │
╰──────────────────────────────────────────────────────────────────────────────╯
╭─ Resources & Runs ───────────────────────────────────────────────────────────╮
│ unity  ■ 1/1                                                                 │
│   ▶ #57   playmode-test combat     3m 12s     lanternfall                    │
│   ⋯ #58   playmode-test save-load  next       lanternfall                    │
╰──────────────────────────────────────────────────────────────────────────────╯
╭─ Claims & cleanup ───────────────────────────────────────────────────────────╮
│   lanternfall#42          claude 7f3a1c2e   4m ago                           │
│   lanternfall#38          codex 019a2b3c    20s ago                          │
│ ! geleider#18             codex 019a0000    quiet 1d, Session ended          │
│   geleider release Eloyye/geleider#18 --force                                │
│ ✓ lanternfall #33 merged  codex/issue-31                                     │
│   geleider clean removes Finished worktrees after asking                     │
╰──────────────────────────────────────────────────────────────────────────────╯
```

## Issues, PRs and worktrees

| Command | What it does |
| --- | --- |
| `geleider claim <issue> [--json]` | Claim an issue or PR (`5`, `#5` or `owner/name#5`) for the Session in this worktree. See [Issue Claims](claims.md). |
| `geleider release <issue> [--force] [--json]` | End a Claim. `--force` releases a Claim another Session holds. |
| `geleider dispatch [numbers…] [--profile P] [--prompt T \| --prompt-file F] [--no-template] [--yes]` | Start a background Session on each issue or PR. Without numbers, pick from the open ones. See [Dispatch](dispatch.md). |
| `geleider review <pr> [--with claude\|codex] [--yes]` | Cross-review a PR with the other vendor. See [Cross-review](cross-review.md). |
| `geleider clean` | List Finished worktrees and remove them only after confirmation. See [CI failures and Finished worktrees](ci-and-worktrees.md). |
| `geleider import` | Turn Claude Desktop's session presets into Profiles and each Project's Codex setup script into its Setup, after confirming. |

## Setup and internals

| Command | What it does |
| --- | --- |
| `geleider install` | Start the background process at login and register the MCP server, Session hooks and status line in Claude Code, Codex and OpenCode. |
| `geleider uninstall` | Undo `install`. Config and Run history are kept. |
| `geleider daemon` | Run the background process in the foreground. |
| `geleider mcp` | Serve MCP over stdio for a Harness. See [MCP tools](mcp.md). |
| `geleider hook <harness> <event>` | Report a Harness lifecycle event from JSON on stdin. Always succeeds. Installed by `install`. |
| `geleider statusline [--wrap <cmd>]` | Claude Code's status line, set by `install`. |
