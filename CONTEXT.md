# Geleider

Coordinates coding-agent work on one Mac across Claude Code, Codex and OpenCode: who may use a heavy resource right now, how much of each subscription is left, and which session is on which issue. It sits beside the vendors' own apps and does not replace them.

## Language

### Work

**Project**:
One GitHub repository and the agent work done in it.
_Avoid_: Repo (in prose), workspace

**Harness**:
A coding-agent tool, including its desktop app: Claude Code, Codex or OpenCode.
_Avoid_: Agent, tool, CLI

**Session**:
One conversation in one Harness, in one worktree of one Project, working on at most one issue or PR. Subagents belong to the Session that started them.
_Avoid_: Agent, thread, chat

**Waiting session**:
A Session stuck on a permission prompt or a direct question to you. A Session that has only finished its turn is idle, not waiting.
_Avoid_: Blocked, needs attention

**Claim**:
A Session's exclusive hold on one issue or PR, shown on GitHub as the issue or PR assigned to you plus a comment naming the Session. It ends when the issue or PR closes or you release it, never by timing out.
_Avoid_: Lock, assignment

**Cross-review**:
A one-off review of a PR by a Harness from a different vendor than the one whose Session wrote it.
_Avoid_: Second opinion, audit

**Profile**:
A named, reusable way of starting Sessions in one Harness, such as `codex-xhigh`: the model, effort and settings each new Session gets, and the prompt it starts with. A Profile is never a Session itself.
_Avoid_: Provider instance, preset, agent

**Dispatch**:
Starting a new Session from a Profile on one issue or PR of a Project, at your explicit request. geleider prepares a new worktree and starts the Session in it; the Harness hosts the Session from then on. Dispatching a PR continues it on its own branch.
_Avoid_: Assign, launch, spawn

**Setup**:
The commands a Project runs in each new Dispatch worktree before its Session starts, such as `uv sync --frozen`. Like Jobs, it is declared in geleider's own config, never in the Project.
_Avoid_: Environment, bootstrap, setup script (in prose)

**Finished worktree**:
A worktree whose PR is merged or closed, with no Session using it and no uncommitted changes, untracked files or ignored files other than its Project's disposable paths (such as `target/`), so it is safe to remove.
_Avoid_: Stale worktree, dead worktree

### Scheduling

**Resource**:
A named thing on this Mac, recognised by the process that uses it, that only a fixed number of processes may use at once, such as `unity` with one slot, since only one Unity Editor may have a project open at a time. It is shared by every Project.
_Avoid_: GPU (the limit is often memory, not the GPU)

**Lease**:
One slot of a Resource, taken by a running Job or by any other process using the Resource, such as you working in the Unity Editor. It ends when that process exits, even if it crashes.
_Avoid_: Lock, mutex

**Job**:
A named command a Project declares, such as `playmode-test <suite>`, that needs a Resource. A Session submits it and geleider runs it, outside any sandbox, once a slot is free. Jobs are declared in geleider's own config, never in the Project, so agents cannot add them.
_Avoid_: Task, script

**Run**:
One submission of a Job by a Session. It runs in that Session's worktree, waits in the queue, holds a Lease while running, and ends with a result that goes back to the Session and onto the branch's PR.
_Avoid_: Execution, build, test run

### Usage

**Subscription**:
A paid plan with its own usage limits, shared by every Harness signed in to it, such as Claude Pro, ChatGPT Plus or OpenCode Go.
_Avoid_: Account, quota

**Limit window**:
A rolling period, such as 5 hours or 7 days, over which a Subscription's usage is capped. It has a used percentage and a reset time.
_Avoid_: Rate limit, budget
