# MCP tools

`geleider install` registers `geleider mcp` as an MCP server in Claude Code, Codex and OpenCode. It offers these tools:

| Tool | Arguments | What it does |
| --- | --- | --- |
| `submit_run` | `job` (required), `args`, `cwd` | Queue a Run of a Job from geleider's config in the caller's worktree. Returns at once with the Run id, state and queue position. The reply's `delivery` says whether the result will be sent to the Session when the Run finishes; if not, poll `get_run`. |
| `get_run` | `id` (required) | A Run's state, queue position and, once started, its log tail. When finished: exit code, duration and full log path. |
| `list_runs` | `include_finished` | Running and queued Runs across all Projects, oldest first. `include_finished` adds the 20 most recently finished. |
| `claim` | `issue` (required), `cwd` | Claim an issue or PR for this Session before its first write. See [Issue Claims](claims.md). |
| `release` | `issue` (required), `cwd` | Release this Session's Claim. Only the holder can release through MCP. |
| `status` | — | The same overview as `geleider status`. |
| `usage` | — | How much of each Subscription is used in each Limit window: used percentage, reset time (Unix ms) and age. A stale window is last known, not current; `used_percent` is `null` once it has reset with no data since. |

`cwd` defaults to the server's working directory. `submit_run`, `claim` and `release` also accept a `session` argument, but the Harness hook replaces it with the attested calling Session, so a Session cannot speak for another (see [ADR 0003](adr/0003-attested-session-destinations.md)).

There is deliberately no MCP tool for `dispatch`, `review` or `clean`; those only run on your command.

## Approval in Codex

`codex exec` cannot ask for approval, so `geleider install` pre-approves geleider's tools in Codex (`default_tools_approval_mode = "approve"`). They can only queue Jobs from geleider's own config, and the Session itself stays in its sandbox, but read the [security model](security.md) first. Set it to `"prompt"` in `~/.codex/config.toml` to be asked each time; reinstalling keeps your choice. With pre-approval, `claim` and `release` comment on GitHub without asking.
