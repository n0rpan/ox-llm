# Unattended work

`oxa work tasks.json` opts into sequential tasks in separate git worktrees.
Run it inside the project repository. The JSON file is an array:

```json
[{"id":"docs-fix","model":"your-model","branch":"agent/docs-fix","base":"main","prompt":"Fix documentation links."}]
```

`base` defaults to `main`. IDs contain letters, digits, hyphens or underscores.
IDs and branches must be unique within one file. Existing task branches can be
continued. For a GitHub issue use `oxa work --issue 123 --model your-model`.
This explicitly invokes your installed,
authenticated `gh` CLI to fetch the issue title and body. Issue lookup is refused
when the environment or user settings enable offline mode.
Issue lookup also requires source-project trust. Git and GitHub CLI host calls use
validated executables outside the repository. Worker and check-supervisor PATHs
exclude project directories, relative entries and unsafe aliases; project tools
can still run by explicit path inside the sandbox.
Branches must be literal names; checkout-history aliases such as `@{-1}` are refused.
Every base and acceptance command list is resolved before any task in the batch
starts. All bases must therefore exist at batch startup. Host metadata commands
ignore replacement objects and inherited `GIT_*` overrides, disable filesystem
monitors, and never lazily fetch missing objects. Fetch the needed objects before
starting the batch. Checkout and checks also ignore replacement objects.

Each task runs a child `oxa --mode=json` with its model and a saved session. The installed offline policy,
including a parent `--offline` flag, is passed to every child. Normal agent permission, trust and
sandbox rules apply. Configure project trust before unattended use. The runner
passes `-na` on every child start, including repair, and removes `OXA_TRUST_PROJECT`.
Worker project settings, MCP servers and extensions are therefore ignored even
when a parent directory or the user's default would trust the new worktree.
User-level resources keep their normal behavior; external acceptance uses the
approved source project and the batch's frozen command list. The runner
creates detached Git metadata with callbacks suppressed on the host, then creates
the task branch and populates the fresh worktree inside the normal sandbox.
Reference hooks, checkout hooks and smudge filters stay inside it; a reference hook
that rejects the task branch still blocks the task.
missing bwrap blocks before checkout or a worker starts. Configured hook directories
keep their normal read-only protection: a checkout that needs to populate a tracked
protected hook directory blocks with the worktree retained.
Acceptance commands are frozen from the captured base commit before checkout
callbacks can change settings. The runner executes those `done` commands outside the
agent after it exits, even without edits. The source project must pass the existing
trust and executable-content approval before a task starts. Invoking `work` does
not approve an untrusted project. Acceptance runs in the normal bwrap sandbox;
missing or unusable bwrap blocks the task. Checkout and each check have the same ten-minute limit
as agent-internal done checks. `in subdirectory: command` selects a folder inside
that sandbox; escaping the worktree is rejected.
The final Git inspection is also sandboxed, with a read-only workspace, no network
and fsmonitor disabled. Clean filters stay inside that sandbox; an inspection
error blocks the task rather than producing a clean verdict.
Agent-internal done checks may also run when the project is trusted.

The runner reads the child's exit code, terminal `oxa_result` and session header,
without retaining its event stream. A blocked, failed or interrupted worker remains
blocked and does not consume a repair attempt. Failed acceptance after a successful
worker continues that same saved session once, then acceptance runs once more.
JSON lines include the worker result, session ID and captured `base_sha` and report `done`, `nothing_to_do` (no
commits or file changes), `red` (checks still fail), or `blocked` (setup or agent
execution failed). Exit codes are 0 for success, 1 if any task is red or blocked,
and 2 for invalid input or output errors. A task setup failure produces a blocked
JSON line and does not skip later tasks. If a task cannot be locked or has an
incompatible receipt, its existing state is preserved; a pre-worktree failure has
an empty `worktree` in the result. Locks use kernel ownership, so an abandoned lock file
after process death does not permanently block a task.

Worktrees and uncommitted changes remain in `.oxa-worktrees/<repository>/<id>`
beside the repository (or the separate git directory). Private result receipts and
locks live under `~/.oxa/agent/work/<repository-hash>/` (or the configured agent
directory), outside the worker's writable Git bind. New receipts are published
atomically without overwriting an existing receipt; a validated matching receipt
is replaced atomically under the task lock when continuing. Worktrees stay outside that git
directory so the normal sandbox can bind git objects and refs for commits. No automatic
cleanup, commit, push or PR creation is implemented yet. Inspect
retained work before removing it with `git worktree remove`. Review task prompts
and bases before running them.

`done` here describes accepted local work, which may be uncommitted. This slice
does not publish it. The saving/publication slices must bind acceptance to a clean
committed tree and verify its recorded commit before pushing.

To continue a task, run the same task file again. A matching retained receipt and
linked worktree are reused. The source checkout is never used as the task worktree.
An existing branch checked out elsewhere is blocked. Dirty retained work is kept
and blocked until committed. A clean retained branch gets an ordinary sandboxed
merge of the captured base; no reset, stash, force checkout or automatic fetch runs.

An unfinished merge is passed to the worker with its old pending commit, the
captured current base and at most twenty conflicted paths. The worker must finish
that merge and merge the captured base too. Aborted or unresolved merges and branch
switches remain blocked even if acceptance passes. Worker, merge and acceptance
failures are retained together. Receipt replacement keeps private modes.
