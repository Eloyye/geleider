# geleider

Coordinates coding-agent work on one Mac across Claude Code, Codex and OpenCode. See [CONTEXT.md](CONTEXT.md) for the vocabulary and [docs/adr](docs/adr) for the decisions behind it.

## Resource Leases and the Job queue

A Session in any Harness submits a Job that needs a Resource, such as Unity PlayMode tests that launch the Unity Editor. geleider queues each Run, starts it outside any sandbox once a slot is free, and reports the result. No two Runs share a one-slot Resource, and an Editor you open by hand holds a slot too.

```sh
cargo install --path crates/geleider
geleider install      # launchd login service + MCP server in Claude Code, Codex, OpenCode
```

Then declare Projects, Resources and Jobs in `~/.config/geleider/config.toml`. Jobs come only from this file, never from a Project's repo:

```toml
[resources.unity]
slots = 1
process = "Unity"         # every running process with this name holds a Lease

[projects.lanternfall]
repo = "Eloyye/lanternfall"
path = "~/code/lanternfall"  # the clone; its worktrees belong to the Project too
vars = { unity = "/Applications/Unity/Hub/Editor/6000.0.23f1/Unity.app/Contents/MacOS/Unity" }

[projects.lanternfall.jobs.playmode-test]
resource = "unity"
args = ["suite"]
# Split like a shell would but never run through one, so args cannot inject
# commands. A relative program path is relative to the worktree. {worktree} is built in.
command = "scripts/run-playmode.sh {suite} {unity}"
```

From any worktree of the Project:

```sh
geleider run playmode-test combat         # returns at once with the Run id and queue position
geleider run playmode-test combat --wait  # blocks, then exits with the Run's exit code
geleider show 12                          # state, exit code, duration, log path and tail
geleider wait 12
geleider queue [--all]
geleider status                           # Sessions, Claims, Resources and their holders, Runs
```

The MCP server (`geleider mcp`) offers the same as the tools `submit_run`, `get_run`, `list_runs` and `status`, plus `claim`, `release` and `usage` (see below). In Codex, `geleider install` pre-approves these tools (`default_tools_approval_mode = "approve"`), since `codex exec` cannot ask. They can only queue Jobs from geleider's own config, and the Session itself stays in its sandbox, but see [Trust boundary](#trust-boundary). Set it to `"prompt"` in `~/.codex/config.toml` to be asked each time; reinstalling keeps your choice.

When a Run finishes, geleider sends a macOS notification and, if the worktree's branch has an open PR, posts the result there through `gh`. Each Job gets one comment per PR, edited in place on later Runs. It also notifies you when a queued Run starts because a slot freed up. Set `notifications = false` or `pr_comments = false` at the top of the config to turn these off.

State lives in `~/Library/Application Support/geleider/`: the SQLite database, the socket and each Run's full log under `runs/`. The background process logs to `~/Library/Logs/geleider/daemon.log`.

### Trust boundary

A Run executes outside any sandbox, in the submitting Session's worktree. geleider fixes *which* command runs, but the files it runs, such as `scripts/run-playmode.sh` and the tests it calls, come from that worktree, and a Session can edit them. So a Session that can submit a Job can run code outside its sandbox, with your network access and files. Only declare Jobs for Projects whose Sessions you would trust with that, and leave Codex's approval mode at `"prompt"` if you want to see each submission first. In addition:

- A Run only starts in the Project's clone or in a worktree that clone has registered (`git worktree list`). A directory merely pointed at the clone's repository through a hand-written `.git` file is refused.
- geleider runs git in the caller's directory with `core.fsmonitor` and hooks switched off, so repository settings an agent wrote cannot run commands.
- geleider's state directory, socket and Run logs are readable only by you. `install` writes Harness configs, which can hold credentials, without ever making them readable by others, and never writes through a symlink planted at its temp or backup file.
- When `pr_comments` is on, the last 40 lines of each Run's log are posted to the PR. Don't print secrets in Jobs on repositories others can read. Job args, errors and the log appear there only as code, so a Session cannot make them links, mentions or HTML posted as you.
- Job args with control or invisible characters are refused, and `geleider status`, `queue`, `show` and error messages drop terminal escapes from paths, branches and logs, so a Session cannot write escape sequences into your terminal.
- Every `git`, `gh` and `codex` call has a deadline (10 s, 30 s and 5 s), so one that hangs cannot stall Session tracking, Claims or delivery.

If the background process restarts while a Run is running, the Run keeps its Lease until its process exits. It then ends as `lost`, since its exit code is unknown; its log is complete.

## Sessions through Harness hooks

`geleider install` also merges lifecycle hooks into Claude Code's
`~/.claude/settings.json` and Codex's `$CODEX_HOME/hooks.json` (normally
`~/.codex/hooks.json`), and installs a thin OpenCode **2.x** plugin at
`~/.config/opencode/plugins/geleider.ts`. Restart existing Sessions to load the
new hooks. In Codex, review and trust them with `/hooks`; installation does not
bypass the Harness's trust checks.

`geleider status` groups live Sessions by Project, with waiting ones first. Each
entry includes the Harness's Session id, worktree, branch, state and last
activity. Permission requests and question tools mean **waiting**; finishing a
turn means **idle**. Session changes never send geleider notifications.

Hooks report through a nonblocking local socket. They always exit successfully,
even when the daemon is stopped or its socket is full. Run and Session history
survive daemon restarts; a Session ends when its Harness reports it or its
owning process exits.

For `submit_run`, the Harness hook attaches the exact calling Session id and its
working directory to that call. This supports MCP server processes shared by
multiple Sessions. Codex receives completed Run results through `codex queue`;
OpenCode receives queued messages through the plugin. Failed deliveries stay in
SQLite for retry. Claude Code continues to use `geleider run … --wait`.

`geleider uninstall` restores unchanged Harness files byte for byte (and removes
files created during installation). If you edited a file after installation, it
removes geleider's entries while preserving your edits. See
[hook integration details and verification](docs/harness-hooks.md), including
manual desktop checks and the fallback when a Harness doesn't run hooks.

## Issue Claims

Only one Session works on an issue at a time, across all Harnesses. Every Harness acts on GitHub as you, so an assignee alone can't show *which* Session has an issue; a Claim records that. A Session claims an issue as its first write, with the MCP tool `claim` or from a shell in its worktree:

```sh
geleider claim 5                      # or #5
geleider release 5                    # by the Session holding it
geleider release owner/name#5 --force # you, ending a Claim another Session holds
```

`claim` fails, naming the holder, if another Session holds the issue or it is already assigned on GitHub. On success it assigns the issue to you, so issue workflows that skip assigned issues keep working, and comments ``Claimed by <Harness> session <id> on `<branch>`.`` `release` (also an MCP tool) unassigns you and comments that the Claim was released. Claiming an issue you already hold changes nothing. The issue must be in a Project from geleider's config, and claiming goes through `gh`, signed in as you.

A Claim ends when its issue closes (geleider checks every 2 minutes) or when it is released, never by timing out. `geleider status` lists Claims and flags any whose Session has been quiet for more than 24 hours, so you can check on it; it is not released.

When a Session starts on a branch containing `issue-<n>`, such as `codex/issue-5-fix`, while another Session holds #n, the start hook adds a warning to that Session's context. It only warns; it never claims. Claude Code and Codex get it; OpenCode's plugin has no way to add start context, so its Sessions are not warned.

**Which Session claims.** Through MCP, the Harness hook names the calling Session and attests it, as for `submit_run`. From a shell, or when no hook ran, the Claim goes to the only live Session in the caller's worktree; with none or several there, `claim` fails and asks for the MCP tool instead. In Codex, `geleider install` pre-approves `claim` and `release` along with geleider's other tools, so they comment on GitHub without asking. See [ADR 0004](docs/adr/0004-claim-holders.md).

## CI failures and Finished worktrees

The background process watches open PRs on branches recorded by Sessions in
configured Projects, including recently ended Sessions. It queries one branch
every 15 seconds, and each branch at most once every five minutes. More than
20 branches take longer to revisit. Polls run separately from Job scheduling.
A failed check sends a macOS notification only to you; it never messages a
Session or starts a fix. Failure identities are kept in SQLite, so repeated
polls and daemon restarts stay quiet. A new failing commit or failed rerun
notifies again. `notifications = false` also disables CI notifications.

`geleider status` lists Finished worktrees across every configured Project's
`git worktree list`, whichever Harness created them. PR lookups are cached for
five minutes; unavailable GitHub data is reported and never permits cleanup.
The check runs in the background so `status` stays fast: it waits up to two
seconds for it, then shows the previous result, or "still being checked" before
the first one finishes. `geleider clean` always checks GitHub afresh.

```sh
geleider clean  # show the list, then ask "Remove these worktrees? [y/N]"
```

Removal requires typing `y` or `yes`; an empty answer, EOF or anything else
cancels. The command refreshes PR state after confirmation and rechecks the
worktrees before removing them with `git worktree remove`, without forcing it.
It never runs automatically and has no MCP tool.

A Finished worktree has a closed or merged PR, no live Session or queued/running
Run using it, and no staged, unstaged, untracked or ignored files. Its HEAD must
equal GitHub's retained PR head, proving its commits were pushed even after a
squash merge or deletion of the remote branch. A newer open PR on a reused
branch prevents cleanup. The primary clone, configured Project directory,
locked worktrees, detached worktrees, and the directory invoking `clean` are
kept. Worktrees with index flags that hide changes (including sparse checkouts)
are kept too, as are worktrees containing submodules and worktrees whose `.git`
file no longer points back to the clone. geleider reads each worktree through
the clone's own record of it, so config a Session wrote never runs. Local
branches are retained after worktree removal.

## Cross-review on request

From any worktree of a configured Project:

```sh
geleider review 17
geleider review https://github.com/owner/project/pull/17
geleider review 17 --with claude    # when the writing Session is unknown
geleider review 17 --yes            # show usage, then start without a prompt
```

This is always an explicit request; geleider never starts a Cross-review while
watching PRs. A PR whose recorded writing Sessions used Anthropic is reviewed
through `codex review` on ChatGPT Plus. A PR written through Codex or OpenCode's
`openai` provider is reviewed through `claude -p` on Claude Pro. OpenCode's
provider comes from its root Session's model events; reinstall the plugin with
`geleider install` to begin recording it. Old records remain readable.

When the writer is unknown, uses another OpenCode provider, has records from
both vendors, or is on a fork, supply `--with claude|codex`. For a known writer,
selecting the same vendor is refused. Ended Sessions remain usable until the
normal seven-day history retention expires. Cross-review Sessions never count
as writing Sessions.

Before starting, the command shows the reviewing Subscription's Limit windows,
including missing or stale readings, and asks for `y` or `yes`. EOF or any other
answer cancels. `--yes` skips this prompt but still prints usage. The reviewing
Harness must already be installed and signed in to its Subscription.

The review runs headlessly in a temporary detached worktree at the PR's exact
head, with a read-only Codex sandbox or Claude's Read/Grep/Glob tools and no MCP
servers. Claude loads only user settings so PR-supplied settings cannot install
hooks. Claude receives the committed diff on stdin. No current worktree edits
enter the review. The process is tracked as a working Session and ends when the
review finishes or its owning process exits; installed hooks use the same
review Session id. Reviews have a 30-minute deadline.

Successful output is posted as a comment review on the PR, with the base and
head commits. If either commit changes during review, nothing is posted. Empty
or failed reviews also post nothing. Output and diagnostics are saved privately
under `~/Library/Application Support/geleider/reviews/<session-id>/`, including
when posting fails. Temporary worktrees are removed without forcing; any that
have changes are retained and reported. Reviews use the explicit request to
post, independently of the `pr_comments` setting for Job results.

The integration tests use fake Harness and GitHub commands; they do not spend
Subscription usage. Command interfaces were checked against the
[Codex CLI reference](https://developers.openai.com/codex/cli/reference) and
[Claude Code permission modes](https://code.claude.com/docs/en/permission-modes).

## Subscription usage

`geleider status` (or `geleider usage`) shows how much of each Subscription is used in each Limit window and when each window resets, so you can see what is left before starting work. The MCP tool `usage` returns the same data. geleider only shows these numbers; it never routes work by them.

```text
Subscriptions
  Claude Pro  (from Claude Code's status line; CLI sessions only)
    5h   23% used, resets 14:05 (in 2h 10m)  updated 3m ago
    7d   41% used, resets Fri 09:00 (in 3d 04h)  updated 3m ago
  ChatGPT Plus  (from Codex's session logs)
    5h   12% used, resets 13:00 (in 1h 05m)  last known at 09:12 (2h 50m ago)
    7d  reset at 11:00, no data since  last known at 09:12 (2h 50m ago)
```

The numbers come only from what each Harness reports itself. geleider calls no usage endpoints, never reads or stores login tokens, and never estimates from token counts:

- **Claude Pro**: `geleider install` sets Claude Code's `statusLine` to `geleider statusline`, which passes `rate_limits.five_hour` and `rate_limits.seven_day` to geleider and prints a short status line. If you already had a status line, it is wrapped (`geleider statusline --wrap '<your command>'`) and its output is shown unchanged; `geleider uninstall` puts it back. Claude Code runs status lines only in interactive CLI sessions, not in the desktop app or `claude -p`, so these numbers update only while a CLI session runs.
- **ChatGPT Plus** (shared by Codex and OpenCode's OpenAI login): the newest `rate_limits` entry in Codex's session logs under `~/.codex/sessions/` (or `$CODEX_HOME/sessions/`): a 300-minute and a 10,080-minute window.
- **OpenCode Go** is not tracked. Its usage is shown only in the OpenCode console; the one usage endpoint, `/zen/go/v1/usage`, is undocumented and needs the Go API key.

Numbers more than ten minutes old are shown as "last known at", with their age. A window whose reset time has passed shows no percentage until the Harness reports again.

When a window resets after reaching 80% used, geleider sends a macOS notification. Windows used less reset quietly, so a lightly used 5-hour window does not notify every 5 hours. Configure it in `~/.config/geleider/config.toml`:

```toml
[subscriptions]
notify_resets = 80        # a percentage, or false for never
[subscriptions.claude]
plan = "Claude Max"       # the name shown; defaults to Claude Pro
[subscriptions.chatgpt]
enabled = false           # stop showing a Subscription
```
