# Issue Claims

Only one Session works on an issue or PR at a time, across all Harnesses. GitHub treats a PR as an issue for assignees and comments, so everything below applies to PRs too. Every Harness acts on GitHub as you, so an assignee alone can't show *which* Session has an issue; a Claim records that. A Session claims an issue as its first write, with the [MCP tool](mcp.md) `claim` or from a shell in its worktree:

```sh
geleider claim 5                      # or #5
geleider release 5                    # by the Session holding it
geleider release owner/name#5 --force # you, ending a Claim another Session holds
```

`claim` fails, naming the holder, if another Session holds the issue or it is already assigned on GitHub. On success it assigns the issue to you, so issue workflows that skip assigned issues keep working, and comments ``Claimed by <Harness> session <id> on `<branch>`.`` `release` (also an MCP tool) unassigns you and comments that the Claim was released. Claiming an issue you already hold changes nothing. The issue must be in a Project from geleider's config, and claiming goes through `gh`, signed in as you.

## Lifetime

A Claim ends when its issue closes (geleider checks every 2 minutes) or when it is released, never by timing out. `geleider status` shows each Claim beside the Session holding it, and flags any whose Session has been quiet for more than 24 hours with the command to release it, so you can check on it; it is not released.

## Branch warnings

When a Session starts on a branch containing `issue-<n>`, such as `codex/issue-5-fix`, while another Session holds #n, the start hook adds a warning to that Session's context. It only warns; it never claims. Claude Code and Codex get it; OpenCode's plugin has no way to add start context, so its Sessions are not warned.

## Which Session claims

Through MCP, the Harness hook names the calling Session and attests it, as for `submit_run`. From a shell, or when no hook ran, the Claim goes to the only live Session in the caller's worktree; with none or several there, `claim` fails and asks for the MCP tool instead. In Codex, `geleider install` pre-approves `claim` and `release` along with geleider's other tools, so they comment on GitHub without asking. See [ADR 0004](adr/0004-claim-holders.md).
