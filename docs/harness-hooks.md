# Harness hooks and Session identity

## Configuration and ownership

The installer keeps existing MCP registrations and adds these integrations:

| Harness | Hook source | Events |
| --- | --- | --- |
| Claude Code | `~/.claude/settings.json` | SessionStart/End, UserPromptSubmit, PreToolUse, PermissionRequest, PostToolUse/Failure, Stop/Failure, Notification |
| Codex | `$CODEX_HOME/hooks.json` | SessionStart/End, UserPromptSubmit, PreToolUse, PermissionRequest, PostToolUse, Stop, Interrupt |
| OpenCode 2.x | `~/.config/opencode/plugins/geleider.ts` | Session lifecycle/status, permission and form events, tool before/after hooks |

Codex must have hooks enabled and the installed definitions trusted in `/hooks`.
The installer preserves existing inline TOML hooks; Codex loads those alongside
`hooks.json`. OpenCode 1.x plugins use a different API and are not supported by
this plugin. It has no runtime package dependencies and uses the OpenCode 2
Promise plugin contract.

A hook command is owned only when it invokes `geleider hook <harness> <event>`.
Uninstall removes those handlers, preserving foreign handlers even when they
share a matcher group. A plugin file with a foreign owner is left alone.
Private `.geleider-install` sidecars retain the original and installed contents
across repeated installs. When a file still matches the installed version,
uninstall restores its exact original bytes or removes the newly created file.
When it has changed, uninstall merges removal into the current file. Never copy
an old whole-file backup over subsequent user edits. Sidecars may contain
original config credentials and are written with mode 0600.

## Reporting and state

`geleider hook <harness> <event>` reads JSON on stdin, bounds input to 1 MiB and
100 ms, emits no diagnostics, and always returns zero. It sends only identity,
cwd, state, prompt correlation ids, activity time and owning pid through a
nonblocking Unix datagram socket. Prompts, transcripts and tool arguments are
not stored. Daemon absence, a full socket and malformed payloads drop the
activity without blocking the Harness. Delivery is best effort, so an event
dropped during daemon downtime appears only after a later hook fires.

The daemon resolves the configured Project through the repository's common Git
directory, so linked worktrees retain their own branch. Unconfigured directories
still appear under an unconfigured Project. Records are keyed by both Harness
and its Session id. They persist in SQLite; ended Sessions remain in JSON status
and disappear from the live text listing. Installed shell hooks pass the
Harness pid through `$PPID`; the OpenCode plugin passes its process id. The
daemon pairs pids with process start times and ends Sessions whose owner exits.
An explicitly invoked hook without an owner pid relies on SessionEnd instead.

A permission request or a question tool creates a pending user prompt. Matching
completion/reply events resolve it; an unrelated parallel tool cannot clear it.
Stop and Interrupt mean idle. Claude's `idle_prompt` reminder is ignored.
Claude child tool events do not overwrite the parent state; OpenCode child ids
resolve to their parent Session, and child completion cannot idle the parent.
No Session event invokes the notification service.

## Run result routing

MCP initialization identifies a Harness, not a conversation. Its cwd or process
environment is also insufficient when a desktop app shares the MCP server.
Geleider therefore uses **per-call identity**:

- Claude Code and Codex PreToolUse hooks annotate only
  `mcp__geleider__submit_run`, using `updatedInput` to supply
  `session: { harness, id }` from that hook's own `session_id`, and the hook cwd
  when the call omitted cwd. Codex root Session ids are its thread ids.
- OpenCode's `tool.hook("execute.before", …)` annotates the corresponding MCP
  tool arguments using the originating Session id, resolving children to their
  parent. It supplies the Session's location, not the plugin instance location.
- For shell submissions, Codex supplies `CODEX_THREAD_ID` to execution processes.
  MCP servers deliberately do not infer their caller from an inherited value.

If hooks are disabled or skipped by an app, pass the exact Session explicitly
in `submit_run`, for example
`{"job":"ck3-test","args":["events"],"cwd":"/path/to/worktree","session":{"harness":"codex","id":"<exact-session-id>"}}`.
Without identity the Run still works through `get_run` or `geleider wait`; no
result is guessed into another Session. Claude Code should use `--wait`.

The Run retains its Session destination in SQLite. A separate worker invokes
`codex queue --thread <id> --message <result>` with a five-second timeout.
OpenCode polls `geleider session-results opencode <id>` and calls
`ctx.session.prompt({sessionID, text, delivery: "queue"})`, acknowledging the Run
only after admission succeeds. Results include outcome, exit code, duration,
log path and log tail. Failed delivery remains pending across restarts. A crash
between Harness admission and acknowledgment can cause a duplicate message;
Run ids let the recipient recognize it. Ended/archived recipients can leave a
result pending until that Harness accepts delivery again.

## Verification

Run the checks used by CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
node --test crates/geleider/plugins/opencode.test.mjs
```

The Rust suite drives a real daemon and disposable Git worktrees. It covers all
three Harness payload formats, branches, prompt correlation, waiting-first
ordering, idle/end transitions, owner exit, restarts, old database migration,
shared-MCP routing, daemon-down/full-socket/stalled-input behavior, exact config
round trips and retried Codex delivery through a recording executable. The Node
fixture exercises the actual OpenCode plugin against the v2 context contract,
including root/child identities, different worktree locations, queued result
admission, acknowledgment, retry and disposal.

Installed Harnesses on the development Mac: Claude Code 2.1.282, Codex CLI
0.162.1 (hooks enabled), OpenCode 2.0.25. A local smoke check on 2026-10-16
confirmed hook emission from all three installed Harnesses with the correct
Project and linked-worktree branch: Claude `--init-only`, Codex `exec` against a
local error fixture, and an OpenCode Session created through a temporary local
server. Each used disposable config/data directories; no model request or user
conversation was started. The Codex fixture bypassed trust only for the hooks
created in its temporary config directory; normal installation still requires
review. `codex queue --help` confirms the required delivery command.
These checks do not substitute for a manual desktop conversation or a live
model result-delivery check. Claude desktop and Codex desktop remain unverified;
no desktop hook failure is claimed from the CLI checks.

For the remaining desktop smoke checks, restart each Harness after installing:

1. Open a Queendom worktree in Claude Code CLI, Claude desktop, Codex CLI,
   Codex desktop and OpenCode. Confirm the exact Session id, worktree and branch
   in `geleider status --json`.
2. Trigger a permission prompt and a question tool. Confirm waiting while the
   prompt is pending, working after a reply and idle after the turn ends.
3. Stop the daemon. Confirm the same actions remain responsive; restart it and
   confirm subsequent activity is tracked.
4. In Codex, submit a configured Job through MCP. Confirm its completion arrives
   in that exact conversation, including with two Sessions sharing a worktree.
5. Uninstall and compare original Harness files; if edited after installation,
   verify those edits survive. Record actual app/version results in issue #4;
   mark an app that never emits hooks as unsupported rather than inventing an
   identity from its worktree.

Primary references used for the integration:

- [Claude Code hook reference](https://code.claude.com/docs/en/hooks)
- [Codex hooks, trust and input/output contracts](https://developers.openai.com/codex/hooks)
- [OpenCode v2 plugin migration and context](https://github.com/anomalyco/opencode/blob/dev/services/www/src/docs/content/build/plugins/migrate-v1.mdx)
- [OpenCode Session prompt endpoint](https://github.com/anomalyco/opencode/blob/dev/packages/protocol/src/groups/session.ts)
