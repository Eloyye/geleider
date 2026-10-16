# geleider coordinates; it is not another agent front-end

geleider started as a native macOS GUI for running Claude Code, Codex and OpenCode by project. We dropped that: more than 40 Mac apps already do it (Conductor runs all three harnesses in worktrees), and each vendor's own app improves every month. geleider instead runs beside those apps, with no GUI and no interactive sessions of its own. It covers what none of them do: Leases on heavy Resources such as CK3, usage tracking per Subscription, and coordination of issues across Harnesses. Agents reach it through MCP, a command-line tool and Harness hooks. You see its state through `geleider status` and macOS notifications.

## Considered Options

- **Native GUI that hosts the sessions**: rejected. It means competing with the vendors' apps on chat, diff and worktree UI, which is crowded and changes monthly.
- **Menu-bar app**: deferred. If a quick glance is ever needed, a SwiftBar plugin over `geleider status` provides it without building an app.
