# CI failures and Finished worktrees

## CI failures

The background process watches open PRs on branches recorded by Sessions in configured Projects, including recently ended Sessions. It queries one branch every 15 seconds, and each branch at most once every five minutes. More than 20 branches take longer to revisit. Polls run separately from Job scheduling.

A failed check sends a macOS notification only to you; it never messages a Session or starts a fix. Failure identities are kept in SQLite, so repeated polls and daemon restarts stay quiet. A new failing commit or failed rerun notifies again. `notifications = false` also disables CI notifications.

## Finished worktrees

`geleider status` lists Finished worktrees across every configured Project's `git worktree list`, whichever Harness created them. PR lookups are cached for five minutes; unavailable GitHub data is reported and never permits cleanup. The check runs in the background so `status` stays fast: it waits up to two seconds for it, then shows the previous result, or "still being checked" before the first one finishes. `geleider clean` always checks GitHub afresh.

```sh
geleider clean  # show the list, then ask "Remove these worktrees? [y/N]"
```

Removal requires typing `y` or `yes`; an empty answer, EOF or anything else cancels. The command refreshes PR state after confirmation and rechecks the worktrees before removing them with `git worktree remove`, without forcing it. It never runs automatically and has no MCP tool.

A Finished worktree has a closed or merged PR, no live Session or queued/running Run using it, and no staged, unstaged, untracked or ignored files, except ignored files under the Project's `disposable` paths, such as `target/`, which removal deletes (see [ADR 0007](adr/0007-disposable-ignored-paths.md)). Its HEAD must equal GitHub's retained PR head, proving its commits were pushed even after a squash merge or deletion of the remote branch. A newer open PR on a reused branch prevents cleanup.

These are always kept:

- the primary clone and the configured Project directory;
- locked and detached worktrees, and the directory invoking `clean`;
- worktrees with index flags that hide changes (including sparse checkouts);
- worktrees containing submodules;
- worktrees whose `.git` file no longer points back to the clone.

geleider reads each worktree through the clone's own record of it, so config a Session wrote never runs. Local branches are retained after worktree removal.
