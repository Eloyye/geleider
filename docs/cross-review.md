# Cross-review on request

From any worktree of a configured Project:

```sh
geleider review 17
geleider review https://github.com/owner/project/pull/17
geleider review 17 --with claude    # when the writing Session is unknown
geleider review 17 --yes            # show usage, then start without a prompt
```

This is always an explicit request; geleider never starts a Cross-review while watching PRs.

## Choosing the reviewer

A PR whose recorded writing Sessions used Anthropic is reviewed through `codex review` on ChatGPT Plus. A PR written through Codex or OpenCode's `openai` provider is reviewed through `claude -p` on Claude Pro. OpenCode's provider comes from its root Session's model events; reinstall the plugin with `geleider install` to begin recording it. Old records remain readable.

When the writer is unknown, uses another OpenCode provider, has records from both vendors, or is on a fork, supply `--with claude|codex`. For a known writer, selecting the same vendor is refused. Ended Sessions remain usable until the normal seven-day history retention expires. Cross-review Sessions never count as writing Sessions.

## Confirmation

Before starting, the command shows the reviewing Subscription's Limit windows, including missing or stale readings, and asks for `y` or `yes`. EOF or any other answer cancels. `--yes` skips this prompt but still prints usage. The reviewing Harness must already be installed and signed in to its Subscription.

## Isolation

The review runs headlessly in a temporary detached worktree at the PR's exact head, with a read-only Codex sandbox or Claude's Read/Grep/Glob tools and no MCP servers.

- Claude loads only user settings so PR-supplied settings cannot install hooks, and its reads are confined to the review worktree. Claude receives the committed diff on stdin.
- Commands Codex runs inherit only core environment variables, so no tokens. Codex's read-only sandbox can still read files outside the worktree: since a PR can try to instruct its reviewer and the output is posted, **review untrusted PRs with Claude**.
- No current worktree edits enter the review.

The process is tracked as a working Session and ends when the review finishes or its owning process exits; installed hooks use the same review Session id. Reviews have a 30-minute deadline.

## Output

Successful output is posted as a comment review on the PR, with the base and head commits. If either commit changes during review, nothing is posted. Empty or failed reviews also post nothing. Output and diagnostics are saved privately under `~/Library/Application Support/geleider/reviews/<session-id>/`, including when posting fails. Temporary worktrees are removed without forcing; any that have changes are retained and reported. Reviews use the explicit request to post, independently of the `pr_comments` setting for Job results.

The integration tests use fake Harness and GitHub commands; they do not spend Subscription usage. Command interfaces were checked against the [Codex CLI reference](https://developers.openai.com/codex/cli/reference) and [Claude Code permission modes](https://code.claude.com/docs/en/permission-modes).
