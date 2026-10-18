# Configuration reference

geleider reads one file, `~/.config/geleider/config.toml`. Jobs, Profiles and Setup come only from this file, never from a Project's repo (see [ADR 0006](adr/0006-setup-imported-not-read-from-repo.md) and the [security model](security.md)). Unknown keys are rejected.

## Full example

```toml
notifications = true      # macOS notifications for Runs, CI failures and Dispatched Sessions
pr_comments = true        # post each Run's result on the branch's open PR

[resources.unity]
slots = 1
process = "Unity"         # every running process with this name holds a Lease

[projects.lanternfall]
repo = "Eloyye/lanternfall"
path = "~/code/lanternfall"  # the clone; its worktrees belong to the Project too
vars = { unity = "/Applications/Unity/Hub/Editor/6000.0.23f1/Unity.app/Contents/MacOS/Unity" }
default_profile = "claude-xhigh"
setup = "uv sync --frozen"  # run with /bin/sh in each new Dispatch worktree
disposable = ["Library/"]   # ignored output a Finished worktree may still contain

[projects.lanternfall.jobs.playmode-test]
resource = "unity"
args = ["suite"]
# Split like a shell would but never run through one, so args cannot inject
# commands. A relative program path is relative to the worktree. {worktree} is built in.
command = "scripts/run-playmode.sh {suite} {unity}"

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

[subscriptions]
notify_resets = 80        # a percentage, or false for never
[subscriptions.claude]
plan = "Claude Max"       # the name shown; defaults to Claude Pro
[subscriptions.chatgpt]
enabled = false           # stop showing a Subscription
```

## Top level

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `notifications` | bool | `true` | macOS notifications for Runs, CI failures and Dispatched Sessions. |
| `pr_comments` | bool | `true` | Post each Run's result on the branch's open PR. |
| `poll_interval_ms` | integer | `2000` | How often the background process looks for processes holding a Resource. |

## `[resources.<name>]`

A Resource is something only so many Runs can use at once.

| Key | Type | Meaning |
| --- | --- | --- |
| `slots` | integer | How many Leases the Resource can hand out at once. |
| `process` | string | Executable name, e.g. `Unity`. Every running process with this name holds a Lease, including one you opened by hand. |

## `[projects.<name>]`

| Key | Type | Meaning |
| --- | --- | --- |
| `repo` | string | GitHub repository as `owner/name`. |
| `path` | string | The local clone. Its worktrees belong to the Project too. |
| `vars` | table | Values usable as `{name}` in the Project's Job commands. |
| `default_profile` | string | The Profile `geleider dispatch` uses without `--profile`. |
| `setup` | string | Shell commands run with `/bin/sh` in each new Dispatch worktree before its Session starts, outside any sandbox. |
| `disposable` | array of strings | Gitignored paths, relative to the worktree root, that don't stop a worktree being [Finished](ci-and-worktrees.md#finished-worktrees), such as `"target/"`. Removal deletes them. Any other ignored file keeps the worktree. Default empty. |

## `[projects.<name>.jobs.<job>]`

| Key | Type | Meaning |
| --- | --- | --- |
| `resource` | string | The Resource each Run of this Job needs. |
| `args` | array of strings | Names of the positional args a Run passes, usable as `{name}` in `command`. |
| `vars` | table | Job-level values usable as `{name}` in `command`. |
| `command` | string | Program and arguments, split like a shell would but never run through one, so arg values cannot inject commands. `{name}` is replaced inside each word. `{worktree}` is built in. A relative program path is relative to the worktree. |

## `[profiles.<name>]`

A Profile is a named way of starting a [Dispatched](dispatch.md) Session.

| Key | Harness | Meaning |
| --- | --- | --- |
| `harness` | both | `"claude"` or `"codex"`. OpenCode cannot be Dispatched yet. |
| `model` | Claude Code | `--model`. |
| `effort` | Claude Code | `--effort`: `low`, `medium`, `high`, `xhigh`, `max` or `ultracode`. |
| `fast` | Claude Code | Fast mode, which bills usage credits rather than the Subscription. Default `false`. |
| `permission_mode` | Claude Code | `--permission-mode`: `default`, `manual`, `acceptEdits`, `plan`, `auto`, `dontAsk` or `bypassPermissions`. |
| `codex_profile` | Codex | The Codex profile to start with (`codex exec -p`), so Codex stays the source of its model, effort and sandbox settings. |
| `issue_prompt`, `pr_prompt` | both | Prompt templates. Placeholders: `{repo}`, `{number}` and `{kind}`. |

`geleider import` can create Profiles from Claude Desktop's session presets and Setup from each Project's Codex setup script; see [Dispatch](dispatch.md#importing-profiles-and-setup).

## `[subscriptions]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `notify_resets` | number or `false` | `80` | Notify when a Limit window resets that had reached this percentage used. Windows used less reset quietly. |
| `claude.enabled`, `chatgpt.enabled` | bool | `true` | Whether `geleider status` shows the Subscription. |
| `claude.plan`, `chatgpt.plan` | string | Claude Pro / ChatGPT Plus | The name shown, e.g. `Claude Max`. |

See [Subscription usage](subscription-usage.md).
