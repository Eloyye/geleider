# Security model

## Trust boundary

A Run executes outside any sandbox, in the submitting Session's worktree. geleider fixes *which* command runs, but the files it runs, such as `scripts/run-playmode.sh` and the tests it calls, come from that worktree, and a Session can edit them. So a Session that can submit a Job can run code outside its sandbox, with your network access and files. Only declare Jobs for Projects whose Sessions you would trust with that, and leave Codex's approval mode at `"prompt"` if you want to see each submission first (see [MCP tools](mcp.md#approval-in-codex)).

The same applies to a Project's Setup, which runs outside any sandbox in each new [Dispatch](dispatch.md) worktree. geleider never reads Setup from the repo, since a Session could change it on a branch ([ADR 0006](adr/0006-setup-imported-not-read-from-repo.md)).

## Safeguards

- A Run only starts in the Project's clone or in a worktree that clone has registered (`git worktree list`). A directory merely pointed at the clone's repository through a hand-written `.git` file is refused.
- geleider runs git in the caller's directory with `core.fsmonitor` and hooks switched off, so repository settings an agent wrote cannot run commands.
- geleider's state directory, socket and Run logs are readable only by you. `install` writes Harness configs, which can hold credentials, without ever making them readable by others, and never writes through a symlink planted at its temp or backup file.
- When `pr_comments` is on, the last 40 lines of each Run's log are posted to the PR. Don't print secrets in Jobs on repositories others can read. Job args, errors and the log appear there only as code, so a Session cannot make them links, mentions or HTML posted as you.
- Job args with control or invisible characters are refused, and `geleider status`, `queue`, `show` and error messages drop terminal escapes from paths, branches and logs, so a Session cannot write escape sequences into your terminal. `status` colours its own words only, and keeps no escapes but those colours.
- Every `git`, `gh` and `codex` call has a deadline (10 s, 30 s and 5 s), so one that hangs cannot stall Session tracking, Claims or delivery.
- The Harness hook attaches the exact calling Session id to `submit_run`, `claim` and `release`, so a Session cannot act as another ([ADR 0003](adr/0003-attested-session-destinations.md)).

See also the safeguards specific to [Cross-review](cross-review.md#isolation) and [Finished worktree cleanup](ci-and-worktrees.md#finished-worktrees).
