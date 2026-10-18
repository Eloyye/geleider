# Sessions through Harness hooks

`geleider install` merges lifecycle hooks into Claude Code's `~/.claude/settings.json` and Codex's `$CODEX_HOME/hooks.json` (normally `~/.codex/hooks.json`), and installs a thin OpenCode **2.x** plugin at `~/.config/opencode/plugins/geleider.ts`. Restart existing Sessions to load the new hooks. In Codex, review and trust them with `/hooks`; installation does not bypass the Harness's trust checks.

`geleider status` lists live Sessions, waiting ones first, each with its Project (or worktree outside a Project), Harness, Claims, branch, state, last activity and the first 8 characters of its Session id. `geleider status --json` has every field, including full ids and worktrees. Permission requests and question tools mean **waiting**; finishing a turn means **idle**. Session changes never send geleider notifications, except for [Dispatched](dispatch.md#notifications) Sessions.

Hooks report through a nonblocking local socket. They always exit successfully, even when the daemon is stopped or its socket is full. Run and Session history survive daemon restarts; a Session ends when its Harness reports it or its owning process exits.

For `submit_run`, the Harness hook attaches the exact calling Session id and its working directory to that call. This supports MCP server processes shared by multiple Sessions. See [Jobs](jobs.md#results) for how each Harness receives Run results.

`geleider uninstall` restores unchanged Harness files byte for byte (and removes files created during installation). If you edited a file after installation, it removes geleider's entries while preserving your edits.

See [hook integration details and verification](harness-hooks.md), including manual desktop checks and the fallback when a Harness doesn't run hooks.
