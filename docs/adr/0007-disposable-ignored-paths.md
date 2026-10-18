# Only declared ignored paths may be left in a Finished worktree

A Finished worktree used to need a completely empty `git status --ignored`, so any gitignored file kept it. Every Rust worktree has a gitignored `target/`, so none was ever Finished. Ignored files are not all disposable, though: a `.env` copied in through `.worktreeinclude`, or notes and credentials a Session wrote and ignored, exist nowhere else, and `git worktree remove` deletes ignored files without `--force`.

So each Project lists its disposable paths in geleider's config, such as `disposable = ["target/"]`. An entry git reports as ignored is allowed when it is one of those paths or lies under one, relative to the worktree root. Anything else still keeps the worktree: tracked or untracked changes, an ignored file elsewhere (even an empty ignored directory), and a file under a disposable path that is not gitignored. The list defaults to empty, so nothing changes until you declare what is safe to lose. Like Jobs and Setup, it comes only from geleider's config, never from the repo (ADR 0006).

## Considered Options

- **Allowing every ignored file**: rejected. It would delete a `.worktreeinclude` copy or a Session's ignored work without asking.
- **Treating `.gitignore` as the list of disposable paths**: rejected. `.gitignore` says what should not be committed, not what can be lost, and a Session can edit it on its branch.
- **A built-in list of build directories** (`target/`, `node_modules/`, `dist/`): rejected. Whether a directory holds only rebuildable output depends on the Project, and a fixed list would silently cover one that does not.
- **Matching a disposable name at any depth**: rejected for now. Paths are rooted so that a nested `target/` is a choice you make by listing it.

## Consequences

Anything inside a disposable path is deleted with the worktree, including a file a Session put there on purpose. Removal still uses `git worktree remove` without `--force`.
