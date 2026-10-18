# Setup is imported into geleider's config, never read from the repo

The Codex app runs the setup script it finds in the repo's `.codex/environments/environment.toml` whenever it creates a worktree. geleider does not. A Project's Setup is declared in geleider's own config, and `geleider import` copies a Codex setup script there once, after you have seen and approved it. geleider runs Setup outside any sandbox, so it follows the rule for Jobs: the commands come only from geleider's config. A Dispatch on a PR checks out a branch a Session may have written, and reading Setup from that branch would let a Session pick commands that run outside its sandbox. The files Setup runs, such as a lockfile's install scripts, still come from the worktree, as for Jobs.

## Considered Options

- **Reading `environment.toml` at each Dispatch**, as the Codex app does: rejected for the reason above, and because a change to the script on a branch would take effect without you seeing it.
- **Having the Session run Setup as its first step**, inside its sandbox: rejected. Setup usually needs the network, which Codex's sandbox blocks, and a failure would surface as a confused Session instead of a failed Dispatch.
