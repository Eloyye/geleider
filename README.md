# geleider

[![CI](https://github.com/Eloyye/geleider/actions/workflows/ci.yml/badge.svg)](https://github.com/Eloyye/geleider/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Coordinates coding-agent work on one Mac across Claude Code, Codex and OpenCode.

When several agent Sessions run at once, they compete for things only one of them should touch: a Unity Editor, an issue, your Subscription limits. geleider is a small background service plus an MCP server that keeps them out of each other's way and tells you what they are doing. It coordinates; it is not a front end for the agents ([ADR 0001](docs/adr/0001-coordinator-not-front-end.md)).

## Features

- **Job queue with Resource Leases.** Sessions submit Jobs, such as Unity PlayMode tests, that need a scarce Resource. geleider runs them outside the sandbox one slot at a time and reports results to the Session, a macOS notification and the PR.
- **Session tracking.** Lifecycle hooks in every Harness show which Sessions are working, waiting or idle, in which worktree and branch.
- **Issue Claims.** Only one Session works on an issue or PR at a time, across all Harnesses.
- **Dispatch.** Start background Claude Code or Codex Sessions on issues and PRs, each in its own worktree with your Project's Setup.
- **Cross-review.** Review a PR written with one vendor's model using the other's.
- **CI failures and cleanup.** Notifications for failed checks on Session PRs, and confirmed removal of Finished worktrees.
- **Subscription usage.** See how much of Claude Pro and ChatGPT Plus each Limit window has left.

## Requirements

- macOS (geleider uses launchd and macOS notifications)
- Rust 1.99 or newer to build
- [`gh`](https://cli.github.com/), signed in, for Claims, PR comments, CI, Dispatch and Cross-review
- At least one Harness: [Claude Code](https://code.claude.com/), [Codex](https://developers.openai.com/codex/cli/) or [OpenCode](https://opencode.ai/) 2.x

## Installation

```sh
git clone https://github.com/Eloyye/geleider.git
cd geleider
cargo install --path crates/geleider
geleider install
```

`geleider install` starts the background process at login and registers the MCP server, Session hooks and status line in Claude Code, Codex and OpenCode. Restart running Sessions to load them; in Codex, review and trust the hooks with `/hooks`. `geleider uninstall` reverses it and keeps your config and history.

To move to the latest `main`, run `geleider update`: it rebuilds with `cargo install`, re-runs `install` (which restarts the background process) and holds off while a Run is running. After updating with `cargo install` yourself, run `geleider restart` so the background process runs the new binary. Either way, restart Sessions so their MCP servers run it too.

## Quick start

Declare a Resource, a Project and a Job in `~/.config/geleider/config.toml`:

```toml
[resources.unity]
slots = 1
process = "Unity"

[projects.lanternfall]
repo = "Eloyye/lanternfall"
path = "~/code/lanternfall"

[projects.lanternfall.jobs.playmode-test]
resource = "unity"
args = ["suite"]
command = "scripts/run-playmode.sh {suite}"
```

Then, from any worktree of the Project, or through the `submit_run` MCP tool from a Session:

```sh
geleider run playmode-test combat --wait  # queue, wait for a free slot, exit with the Run's code
geleider status                           # Sessions, Claims, Resources, Runs and usage
```

## Documentation

| Guide | |
| --- | --- |
| [Jobs and Resources](docs/jobs.md) | Queueing Runs, results, notifications and state files |
| [Sessions](docs/sessions.md) | How hooks track Sessions in each Harness ([integration details](docs/harness-hooks.md)) |
| [Issue Claims](docs/claims.md) | One Session per issue or PR |
| [Dispatch](docs/dispatch.md) | Starting background Sessions on issues and PRs; Profiles and Setup |
| [Cross-review](docs/cross-review.md) | Reviewing a PR with the other vendor |
| [CI failures and Finished worktrees](docs/ci-and-worktrees.md) | CI notifications and `geleider clean` |
| [Subscription usage](docs/subscription-usage.md) | Limit windows and reset notifications |
| [Security model](docs/security.md) | **Read before declaring Jobs.** What runs outside the sandbox and the safeguards |

| Reference | |
| --- | --- |
| [CLI](docs/cli.md) | Every `geleider` command |
| [MCP tools](docs/mcp.md) | `submit_run`, `get_run`, `list_runs`, `claim`, `release`, `status`, `usage` |
| [Configuration](docs/configuration.md) | Every key in `config.toml` |
| [Glossary](CONTEXT.md) | The domain vocabulary: Session, Harness, Job, Run, Lease, Claim… |
| [Architecture decisions](docs/adr) | Why geleider works the way it does |

## Security

A Run executes outside any sandbox, using files from the submitting Session's worktree, so a Session that can submit a Job can run code with your access. Only declare Jobs for Projects whose Sessions you would trust with that. See the [security model](docs/security.md).

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
node --test crates/geleider/plugins/opencode.test.mjs
```

The integration tests use fake `gh`, `claude` and `codex` commands and spend no Subscription usage.

## Contributing

Issues and pull requests are welcome on [GitHub](https://github.com/Eloyye/geleider/issues). Please use the vocabulary in [CONTEXT.md](CONTEXT.md), and record significant design decisions as an ADR in [docs/adr](docs/adr). Make sure the checks under [Development](#development) pass before opening a PR.

## License

[MIT](LICENSE) © 2026 Eloy Gustavo Ye Luo
