# Unattended work

`oxa work board --shadow --repo OWNER/REPO --once` captures a read-only GitHub sample.
For continuous five-minute sampling use `--token-stdin`: the caller supplies a fresh
newline-terminated App token on a private pipe before each sample. No token is
retained across samples or read from a credential file. The five-minute user timer
may instead remint externally and repeat `--once` with `GH_TOKEN`. It requires a
trusted source project and an explicit `GH_TOKEN` supplied by a GitHub App caller;
offline mode refuses before GitHub or log creation. No worker or mutation runs.
The private, locked `work/<project-hash>/board/OWNER/REPO/board-shadow.jsonl` under the agent
directory appends `{schema_version:1,observed_at,board,would_take}`. Eligible shared
board entries form `would_take`; shadow reserves no leases or attempts. Use one
GitHub repository per capture. Read errors and incomplete connections stop sampling.
Limit: 100 PRs/items per connection. Reported checks are matched against the base's
classic protection and active repository/organization rulesets; missing required
contexts and unknown/no required checks stay pending. Review
requires GitHub's approval decision and all latest approving writer reviews at the
current head; older approvals wait, including pure base merges without fresh evidence.
Snapshot reads reject changing open-PR sets and responses above 4 MiB. Detailed
merge state is checked; behind heads can be queue-ready when the main merge queue
composes and validates them, without requesting pre-emptive main merges.

The opt-in `oxa_work::board` library derives one next action per open PR from a
single normalized GitHub snapshot. `Policy.enabled` defaults to false and makes
no GitHub reads while disabled. Findings take precedence over failed required CI,
conflicts, missing nonempty `Removes:` lines for source removal, then green draft
promotion and reviewed readiness. Pending CI or unknown mergeability waits; other
PRs wait for review. Release-pinned PRs sort first, then action priority and number.
Active leases and a per-PR UTC daily attempt cap
make entries ineligible without hiding them. Snapshot adapters must supply the
complete open set, current-head required checks, unresolved findings, source-removal
facts and review-policy approval for the PR's own diff. Callers
select private repository-scoped claim storage outside worker-writable binds.
`Claims::open(root).snapshot()` supplies durable state for `Board::read`.
Before dispatch, `claim(entry, policy, holder, now, ttl)` atomically reserves a
lease and an attempt under a kernel file lock, then syncs state before returning.
The default daily cap is three; a crash after reservation consumes an attempt.
`release(number, lease)` fences stale workers and never refunds attempts. Active
leases survive UTC midnight; corrupt state, or state that vanished after a PR's
first reservation, fails closed. Execution, fresh-head validation and CLI polling
remain in the adapter; core claim storage never spawns workers or writes to GitHub.

`oxa work tasks.json` opts into sequential tasks in separate git worktrees.
Run it inside the project repository. The JSON file is an array:

```json
[{"id":"docs-fix","model":"your-model","branch":"agent/docs-fix","base":"main","prompt":"Fix documentation links."}]
```

`base` defaults to `main`. IDs contain 1–128 letters, digits, hyphens or underscores.
Optional `max_time` is a positive integer number of seconds, defaulting to 2700
(45 minutes). The task's saved deadline is shared by its worker, repair and crash
recovery; a repair does not get a fresh budget. Each child receives its remaining
allowance as `--max-time` for one graceful wrap-up turn. If it remains alive 60
seconds after that allowance, the runner sends SIGTERM, then SIGKILL after another
10 seconds. The result is interrupted and its worktree is retained. `kill` must be
available as a trusted executable outside the project; status reads do not need it.
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
Missing bwrap blocks before checkout or a worker starts. Configured hook directories
keep their normal read-only protection: a checkout that needs to populate a tracked
protected hook directory blocks with the worktree retained.
Acceptance commands are frozen from the captured base commit before checkout
callbacks can change settings. The runner executes those `done` commands outside the
agent after it exits, even without edits. The source project must pass the existing
trust and executable-content approval before a task starts. Invoking `work` does
not approve an untrusted project. Acceptance runs in the normal bwrap sandbox;
missing or unusable bwrap blocks the task. Checkout and each check have the same ten-minute limit
as agent-internal done checks. `in subdirectory: command` selects a folder inside
that sandbox; escaping the worktree is rejected. A command whose program is not installed here
(the shell exits 127 with its `command not found` line) did not run, so it is reported and the
task is blocked instead of `red`: no worker turn is spent on a program it cannot install.
The final Git inspection is also sandboxed, with a read-only workspace, no network
and fsmonitor disabled. Clean filters stay inside that sandbox; an inspection
error blocks the task rather than producing a clean verdict.
Agent-internal done checks may also run when the project is trusted.

The runner reads the child's exit code, terminal `oxa_result` and session header,
without retaining its event stream. It checkpoints the session ID as soon as the
header arrives. A blocked or failed worker remains blocked; an interrupted worker
remains interrupted. Neither consumes a repair attempt. Failed acceptance after a successful
worker continues that same saved session once, then acceptance runs once more.
JSON lines include the worker result, session ID, captured `base_sha`, saved
`head_sha`/`tree_sha`, step, checkpoint revision and time. Outcomes are `done`,
`nothing_to_do` (no worker commits or file changes), `red` (checks still fail),
`blocked` (setup or execution failed), or `interrupted`. Deadline interruptions
carry a typed `interruption: "deadline"`. Exit codes are 0 for success, 1 if any
task is red, blocked or interrupted, and 2 for invalid input, output or checkpoint
errors. A checkpoint/event-append error stops the entire batch before another
action. An ordinary task setup failure produces a blocked
JSON line and does not skip later tasks. If a task cannot be locked or has an
incompatible receipt, its existing state is preserved; a pre-worktree failure has
an empty `worktree` in the result. Locks use kernel ownership and are inherited
by the worker and check/save supervisors. Killing the runner does not allow a
second runner to start while its child still holds that lock.

Worktrees remain in `.oxa-worktrees/<repository>/<id>`
beside the repository (or the separate git directory). Private result receipts and
locks live under `~/.oxa/agent/work/<repository-hash>/` (or the configured agent
directory), outside the worker's writable Git bind. New receipts are published
atomically without overwriting an existing receipt; a validated matching receipt
is replaced atomically under the task lock when continuing. Worktrees stay outside that git
directory so the normal sandbox can bind git objects and refs for commits.
Before finishing, the runner saves staged, unstaged and untracked work with an
ordinary sandboxed commit, including red, blocked and interrupted work. Normal
hooks remain active. Unresolved merges are kept in progress; a rejecting hook or
remaining dirty files block completion. No automatic cleanup, push or PR creation
is implemented. Keep retained work until its saved commit has been published and
inspected. Review task prompts and bases before running them.

`done` here describes locally checked and saved work. It does not mean published,
externally accepted or merged. Publication must verify a clean committed export,
bind checks to that commit and verify base-object integrity before pushing;
ignored files in a retained working directory are not commit-bound evidence.

To recover a task, run the same task file again. An unfinished checkpoint resumes
its saved step with the original base, commands, session and offline policy.
Recovery loads those inputs before resolving the current base branch or settings,
so a deleted base ref or invalid new checks cannot strand an existing attempt.
Finished successful work is returned without another worker or check run; add
`--rerun` to the task-file or issue command to start another attempt. A task spec
change requires a new ID. Unsupported or corrupt checkpoints are preserved and
refused. A matching linked worktree is reused; the
source checkout is never used as the task worktree. An existing branch checked
out elsewhere is blocked. Dirty retained work is preserved and saved before
retrying. A clean retained branch gets an ordinary sandboxed
merge of the captured base; no reset, stash, force checkout or automatic fetch runs.

An unfinished merge is passed to the worker with its old pending commit, the
captured current base and at most twenty conflicted paths. The worker must finish
that merge and merge the captured base too. Aborted or unresolved merges and branch
switches remain blocked even if acceptance passes. Worker, merge and acceptance
failures are retained together. Receipt replacement keeps private modes.

`oxa work status --json` (also `--status`) reads checkpoints without acquiring a
task lock, changing state, constructing a model or calling GitHub. Its separate
`liveness` is `running`, `finished`, `interrupted`, or `unknown` when the kernel
cannot establish ownership. A missing original parent is not proof of a stopped
child: status also checks the inherited stdin lock. Without `--json` it prints
one short line per task.

The [checkpoint integration contract](work-state.md) defines the runner's public
event hook. Event/situation/standing stores and their CLI adapters are a separate
integration; this slice does not infer facts from worker prose or raw logs.
