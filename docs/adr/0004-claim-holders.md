# A Claim's holder is attested, or the only Session in the worktree

A Claim names the Session holding an issue, in geleider and in its GitHub comment. Through MCP, the Harness hook attests the calling Session for `claim` and `release` exactly as for `submit_run` (ADR 0003). A shell has no attestation, but a Session works in one worktree, and the hooks already record which worktree each live Session is in. So when a call carries no attested Session, the daemon takes the only live Session in the caller's worktree, and refuses when there is none or more than one. Only the holder may release a Claim; the user can end any Claim with `geleider release --force`.

A Session starting on a branch that names `issue-<n>` is warned by its start hook when another Session holds #n. The hook must not wait on the daemon or run git, so the daemon writes every Claim to `claims.json` in its state directory, and the hook reads the branch and the repository's shared `.git` directory from git's own files.

A Claim ends when its issue closes, which the daemon learns by asking `gh` every 2 minutes, or when it is released. Never by timing out: a quiet holder is flagged in `geleider status` after 24 hours, and the user decides.

## Considered Options

- **Model-supplied Session id for shell claims**: rejected for the same reason as in ADR 0003; any id the model writes is believed.
- **Refusing every unattested claim**: rejected. Claude Code Sessions mostly use the command line, and a worktree with one Session names it unambiguously.
- **Asking the daemon from the start hook**: rejected. Hooks run on every event and must stay quick when the daemon is busy or down; a stale snapshot only risks a stale warning.
- **Ending Claims after a period of inactivity**: rejected. A Session paused overnight would lose its issue to another Session.

## Consequences

A model whose shell can reach another worktree, and so run `geleider claim` there, can claim as the Session working in it. That Session would then hold an issue it did not choose, which is visible on GitHub and easy to release. A Harness sandbox confined to its worktree prevents it.
