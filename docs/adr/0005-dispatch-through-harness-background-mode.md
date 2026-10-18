# Dispatched Sessions run in each Harness's own background mode

Most Sessions start as "complete issue #n and open a PR". geleider now starts these on request (a Dispatch), but it still does not host them. It creates the worktree, runs the Project's Setup in it, and then asks the Harness to start one of its own background Sessions there (`claude --bg`, `codex exec`), with the Harness's own worktree creation turned off, and lets go. The Harness owns the process and the conversation, so you attach to or continue it in the vendor's own tools, and geleider tracks it through the same hooks as any other Session. This keeps ADR 0001: vendors host Sessions, geleider coordinates them. Starting a Session is not hosting it.

## Considered Options

- **geleider runs the Harness headless and owns the process**, as Cross-review does: rejected for Dispatch. You could not take the Session over in the vendor's tools. Cross-review keeps it, because it is one-off, read-only and must be confined.
- **Letting each Harness create the worktree** (`claude --bg -w`, `codex exec --worktree`): rejected. Neither can check out a PR's existing branch, and `codex exec --worktree` makes a detached checkout of the caller's HEAD with no setup and no copied files, since only the Codex app runs a local environment's setup script.
- **Opening the vendor's desktop app with the prompt filled in**: rejected as the only way. You would press send in each app for each Session, so Dispatching several issues at once does not work.

## Consequences

Background Sessions are CLI Sessions. Claude Desktop does not list `claude --bg` Sessions, so you continue them with `claude agents` and `claude attach`. Environment variables set only in Claude Desktop's own editor do not reach them; geleider stores none of its own, so they belong in each Harness's own config (`settings.json` `env`, Codex's `shell_environment_policy`).

Dispatch worktrees for every Harness, Codex included, go in `<clone>/.claude/worktrees/`, where Claude Desktop already puts its own and which the clone already ignores. The Codex app manages and cleans up `~/.codex/worktrees` itself, so geleider stays out of it. A background Session cannot answer a trust prompt, and geleider never edits a Harness's trust settings, so a Dispatch into a worktree the Harness does not trust is refused with instructions.

A Dispatch takes the Claim for the Session it starts, so a second Dispatch of the same issue or PR is refused. That Session is the only one in a worktree nothing else uses yet, so this is ADR 0004's rule rather than a new way to name a holder.
