# Dispatching Sessions on issues and PRs

`geleider dispatch` replaces opening Claude Code or Codex and typing "complete this issue and open a PR". From any worktree of a configured Project:

```sh
geleider dispatch 12 15 --profile claude-xhigh   # issue #12 and PR #15
geleider dispatch                                # pick from the open issues and PRs
geleider dispatch 12 --prompt "Keep the change small."
geleider dispatch 12 --prompt-file notes.md --no-template
geleider dispatch 12 --yes                       # show everything, then start without asking
```

Dispatch only runs on your command. Polling and labels never start a Dispatch, and there is no MCP tool for it. Without numbers, a picker lists the Project's open issues and PRs; Claimed and assigned ones are marked `-` and cannot be chosen. One Profile applies to everything in one command: `--profile`, else the Project's `default_profile`, else you are asked. Only Claude Code and Codex can be Dispatched; OpenCode cannot yet. See [ADR 0005](adr/0005-dispatch-through-harness-background-mode.md).

## Confirmation

Before anything starts, `dispatch` shows:
- each item with its Profile and Harness, marking authors who are not you;
- the Profile's Subscription's Limit windows, with missing or stale readings shown as such;
- how many Sessions are live.

It then asks `[y/N]`. Anything but `y` or `yes` cancels. `--yes` skips only the question.

## What happens for each item

In order:

1. **Claim.** It takes the [Claim](claims.md) for the Session about to start. An item already Claimed or assigned is refused, naming the holder. Claims cover PRs too.
2. **Worktree.** It creates `<clone>/.claude/worktrees/issue-<n>` or `pr-<n>`, adding `-2`, `-3` when the name is taken.
   - An issue gets a new branch `claude/issue-<n>` or `codex/issue-<n>`, started from the remote default branch fetched just now.
   - A PR is checked out on its own head branch, tracking `origin`.
   - PRs from forks are refused, and so is a branch checked out in another worktree; the refusal names that worktree.
3. **Copied files.** The gitignored files listed in the clone's `.worktreeinclude` are copied from the clone's checkout.
4. **Setup.** The Project's Setup runs in the worktree, outside any sandbox. Its output is kept in `~/Library/Application Support/geleider/dispatches/<id>/setup.log`. If Setup fails, geleider:
   - removes the worktree and the new branch (a PR's branch is never deleted);
   - keeps the log;
   - releases the Claim;
   - goes on with the other items.

   A Harness that fails to start is handled the same way.
5. **Start.** It starts the Harness's own background Session in the worktree, then lets go:
   - `claude --bg --session-id <id>`. Claude Code does not move a Session that is already in a linked worktree into one of its own; `worktree.bgIsolation: "none"` is passed too.
   - `codex exec --json -C <worktree> [-p <codex_profile>]`, detached from the terminal. Its events go to `dispatches/<id>/codex.jsonl`.

geleider never hosts, pushes for, or opens a PR for a Dispatched Session; the Session does that itself. Continue a Claude Session from `claude agents` (`claude attach <id>`), and a Codex one with `codex resume <thread id>`. Claude Desktop does not list `claude --bg` Sessions.

## Prompts

The prompt names only the repository, the number and whether it is an issue or a PR. Each Session reads the issue or PR itself with `gh`; no title, body or comment from GitHub is ever put in the prompt. `--prompt` or `--prompt-file` text goes after the Profile's template, and `--no-template` drops the template.

## Trust

A background Session cannot answer a trust prompt, and geleider never edits a Harness's trust settings. Claude Code and Codex both key a worktree's trust on its clone. If the Harness does not trust the clone, Dispatch refuses before claiming anything and tells you to run `claude` or `codex` there once to trust it.

## Notifications

Dispatched Sessions are tracked through the hooks `geleider install` set up, like any other [Session](sessions.md). A Dispatched Session, and only a Dispatched Session, sends a macOS notification:
- when it becomes waiting;
- when it first goes idle or ends, naming its branch's open PR or saying none was opened.

`notifications = false` turns these off.

## Environment variables

geleider stores none. Dispatched Sessions get the variables their Harness configures (`env` in Claude Code's `settings.json`, Codex's `shell_environment_policy`) and the `PATH` of the shell that ran `geleider dispatch`. Variables set only in Claude Desktop's environment editor do not reach them.

## Profiles and Setup

Profiles and Setup live only in geleider's config, never in a Project's repo:

```toml
[profiles.claude-xhigh]
harness = "claude"
model = "opus"             # --model
effort = "xhigh"           # low, medium, high, xhigh, max or ultracode
fast = false               # fast mode bills usage credits, not the Subscription
permission_mode = "auto"   # --permission-mode
# Optional; placeholders are {repo}, {number} and {kind} ("issue" or "PR").
issue_prompt = "Fix {kind} #{number} in {repo}; read it with `gh issue view {number} --repo {repo}`."

[profiles.codex-xhigh]
harness = "codex"
codex_profile = "xhigh"    # Codex stays the source of model, effort and sandbox

[projects.lanternfall]
repo = "Eloyye/lanternfall"
path = "~/code/lanternfall"
default_profile = "claude-xhigh"
setup = "uv sync --frozen"  # run with /bin/sh in each new Dispatch worktree
```

Every Profile key is listed in the [configuration reference](configuration.md#profilesname).

### Importing Profiles and Setup

`geleider import` copies settings into geleider's config once, and only when you ask:
- Claude Desktop's session presets (`~/Library/Application Support/Claude/ccd-session-presets.json`) become Profiles. That file is undocumented, so this is best effort.
- Each Project's Codex setup script (`.codex/environments/environment.toml`, `[setup.darwin]` else `[setup]`) becomes its Setup.

It shows everything it found and writes only after `y` or `yes`. geleider never reads a setup script from the repo at Dispatch time, since a Session could change it on a branch ([ADR 0006](adr/0006-setup-imported-not-read-from-repo.md)). The integration tests use fake `gh`, `claude` and `codex` commands and spend no Subscription usage.
