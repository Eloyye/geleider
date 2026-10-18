# Subscription usage

`geleider status` (or `geleider usage`) shows how much of each Subscription is used in each Limit window and when each window resets, so you can see what is left before starting work. The [MCP tool](mcp.md) `usage` returns the same data. geleider only shows these numbers; it never routes work by them.

```text
Subscriptions
  Claude Pro  (from Claude Code's status line; CLI sessions only)
    5h   23% used, resets 14:05 (in 2h 10m)  updated 3m ago
    7d   41% used, resets Fri 09:00 (in 3d 04h)  updated 3m ago
  ChatGPT Plus  (from Codex's session logs)
    5h   12% used, resets 13:00 (in 1h 05m)  last known at 09:12 (2h 50m ago)
    7d  reset at 11:00, no data since  last known at 09:12 (2h 50m ago)
```

## Sources

The numbers come only from what each Harness reports itself. geleider calls no usage endpoints, never reads or stores login tokens, and never estimates from token counts:

- **Claude Pro**: `geleider install` sets Claude Code's `statusLine` to `geleider statusline`, which passes `rate_limits.five_hour` and `rate_limits.seven_day` to geleider and prints a short status line. If you already had a status line, it is wrapped (`geleider statusline --wrap '<your command>'`) and its output is shown unchanged; `geleider uninstall` puts it back. Claude Code runs status lines only in interactive CLI sessions, not in the desktop app or `claude -p`, so these numbers update only while a CLI session runs.
- **ChatGPT Plus** (shared by Codex and OpenCode's OpenAI login): the newest `rate_limits` entry in Codex's session logs under `~/.codex/sessions/` (or `$CODEX_HOME/sessions/`): a 300-minute and a 10,080-minute window.
- **OpenCode Go** is not tracked. Its usage is shown only in the OpenCode console; the one usage endpoint, `/zen/go/v1/usage`, is undocumented and needs the Go API key.

Numbers more than ten minutes old are shown as "last known at", with their age. A window whose reset time has passed shows no percentage until the Harness reports again.

## Reset notifications

When a window resets after reaching 80% used, geleider sends a macOS notification. Windows used less reset quietly, so a lightly used 5-hour window does not notify every 5 hours. Configure it in `~/.config/geleider/config.toml` (see the [configuration reference](configuration.md#subscriptions)):

```toml
[subscriptions]
notify_resets = 80        # a percentage, or false for never
[subscriptions.claude]
plan = "Claude Max"       # the name shown; defaults to Claude Pro
[subscriptions.chatgpt]
enabled = false           # stop showing a Subscription
```
