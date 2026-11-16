# Unattended work

`oxa work tasks.json` opts into sequential tasks in separate git worktrees.
Run it inside the project repository. The JSON file is an array:

```json
[{"id":"docs-fix","model":"your-model","branch":"agent/docs-fix","base":"main","prompt":"Fix documentation links."}]
```

`base` defaults to `main`. IDs contain letters, digits, hyphens or underscores.
IDs and branches must be unique; branches must be new. For a GitHub issue use
`oxa work --issue 123 --model your-model`. This explicitly invokes your installed,
authenticated `gh` CLI to fetch the issue title and body. Issue lookup is refused
when the environment or user settings enable offline mode.
Issue lookup also requires source-project trust. Git and GitHub CLI host calls use
validated executables outside the repository. Worker and check-supervisor PATHs
exclude project directories, relative entries and unsafe aliases; project tools
can still run by explicit path inside the sandbox.

Each task runs a child `oxa --mode=json` with its model and a saved session. The installed offline policy,
including a parent `--offline` flag, is passed to every child. Normal agent permission, trust and
sandbox rules apply. Configure project trust before unattended use. The runner
executes the base version of `.oxa/settings.json`'s `done` commands outside the
agent after it exits, even without edits. The source project must pass the existing
trust and executable-content approval before a task starts. Invoking `work` does
not approve an untrusted project. Acceptance runs in the normal bwrap sandbox;
missing or unusable bwrap blocks the task. Each check has the same ten-minute limit
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
JSON lines include the worker result and session ID and report `done`, `nothing_to_do` (no
commits or file changes), `red` (checks still fail), or `blocked` (setup or agent
execution failed). Exit codes are 0 for success, 1 if any task is red or blocked,
and 2 for invalid input or output errors. A task setup failure produces a blocked
JSON line and does not skip later tasks. If a task cannot be locked or already has
a receipt, its existing state is preserved; a pre-worktree failure has an empty
`worktree` in the result. Locks use kernel ownership, so an abandoned lock file
after process death does not permanently block a task.

Worktrees and uncommitted changes remain in `.oxa-worktrees/<repository>/<id>`
beside the repository (or the separate git directory). Private result receipts and
locks live under `~/.oxa/agent/work/<repository-hash>/` (or the configured agent
directory), outside the worker's writable Git bind. Worktrees stay outside that git
directory so the normal sandbox can bind git objects and refs for commits. No automatic
cleanup, commit, push, PR creation or continuation is implemented yet. Inspect
retained work before removing it with `git worktree remove`. Review task prompts
and bases before running them.
