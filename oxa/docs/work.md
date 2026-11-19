# Unattended work

## Worker mode

`oxa work` workers start in their default mode inside the tool sandbox. When the owner's
own settings (`permissions.defaultMode` in the user layer) say `yolo`, the runner starts
every worker with `--yolo`: no prompts and no tool sandbox. Without yolo nothing changes.
A project's settings are not read for this and cannot raise the worker mode.

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

Add `--landing-shadow` to record latest-review holds and queue-age alarms in `landing`.
The sample records the observed main ref in `landing_main` independently of queue membership
and verified merge-group rejection evidence in `queue_rejections`, including the
removal event time.
Use `--queue-evidence` to record only the rejection evidence alongside the board.

To verify the provenance of test-report archives, pass `--queue-report-policy FILE` with an owner-only
JSON file in a private directory. Set the trusted workflow path, the exact names
of required checks that consume those reports, and every JUnit producer and artifact.

```json
{
  "workflow_path": ".github/workflows/checks.yml",
  "required_check_names": ["workspace test gate"],
  "reports": [{"producer_name": "Workspace shard 1", "artifact_name": "test-report-1"}]
}
```

That option also enables queue evidence. The shadow journal records the checked
run, attempt, required check IDs, SHA-256 digests, and failed test identities after
verifying and reading every configured JUnit archive for that attempt.

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
Active leases make entries ineligible without hiding them. Snapshot adapters must supply the
complete open set, current-head required checks, unresolved findings, source-removal
facts and review-policy approval for the PR's own diff. Callers
select private repository-scoped claim storage outside worker-writable binds.
`Claims::open(root).snapshot()` supplies durable state for `Board::read`.
Before dispatch, `claim(entry, policy, holder, now, ttl)` atomically reserves a
lease and an attempt under a kernel file lock, then syncs state before returning.
`budget_pools.limits.max_attempts_per_day` sets the daily attempt threshold as an
advisory default of three per UTC day, and zero warns immediately: a claim past the
threshold is still reserved, and `oxa work board --refill` prints one operator
warning on stderr with the attempt ordinal, the threshold and its UTC window. The
attempt count never fences admission: `enabled: false`, the pause marker, wait
actions, stale-head and protected-configuration checks, active leases and Claims
failures still do.
A crash after reservation consumes an attempt.
`release(number, lease)` fences stale workers and never refunds attempts. Active
leases survive UTC midnight; corrupt state, or state that vanished after a PR's
first reservation, fails closed. Execution, fresh-head validation and CLI polling
remain in the adapter; core claim storage never spawns workers or writes to GitHub.

On a lab host, an owner can share the worker's Cargo check artifacts between
worktrees. Set `CARGO_TARGET_DIR` in the environment that starts the worker
and add that directory to the owner-level `oxa.sandbox.writePaths` setting.
The variable does not grant a writable mount. Keep this cache dedicated to the
workers. The acceptance supervisor still uses its own isolated check output.

A managed worker can load project resources whose content matches the saved
approval for its source repository. The runner verifies that the checkout is
a managed worktree before reusing that approval. An explicit worktree denial
still wins. Inheritance is per launch, and creates no permanent worktree grant.
The managed worker refuses inherited approval when project MCP, extensions,
or settings can load dependencies outside the approval record. Worker
settings, MCP and agent definitions come from the user configuration. The
work runner takes acceptance checks from the approved source.

`oxa work tasks.json` opts into sequential tasks in separate git worktrees.
Run it inside the project repository. The JSON file is an array:

```json
[{"id":"docs-fix","model":"your-model","branch":"agent/docs-fix","base":"main","prompt":"Fix documentation links."}]
```

`base` defaults to `main`. IDs contain 1–128 letters, digits, hyphens or underscores.
In a task file, optional `max_time` is a positive integer number of seconds and
defaults to 2700 (45 minutes). Newly generated issue, pull request, and native
queue tasks get four hours. Existing checkpoints retain their saved budget.
The task's saved deadline is shared by its worker, repair and crash recovery.
A repair does not get a fresh budget. Each child receives its remaining
allowance as `--max-time` for one graceful wrap-up turn. If it remains alive 60
seconds after that allowance, the runner sends SIGTERM, then SIGKILL after another
10 seconds. The result is interrupted and its worktree is retained. `kill` must be
available as a trusted executable outside the project; status reads do not need it.
IDs and branches must be unique within one file. Existing task branches can be
continued. For a GitHub issue use `oxa work --issue 123 --model your-model`.
This explicitly invokes your installed,
authenticated `gh` CLI to fetch the issue title, body, state and comments, and that
one lookup is the whole task packet the worker receives: the worker stays inside a
sandbox that cannot read the issue, so the state and the comment thread (oldest
first, bounded to 16 KiB with an explicit truncation marker) arrive with the text.
If comments are truncated, the worker asks its runner for the missing data.
Repeating the same issue task, including with `--rerun`, reuses its saved packet.
The worker's local tools run in a sandbox with tool networking disabled.
The worker starts without GitHub authentication, MCP servers, extensions or web
search. The runner owns GitHub lookup and the configured model connection stays
available. These restrictions also apply when the saved issue task is rerun
from a task file.
The runner refuses GitHub issue responses over 4 MiB before starting a worker.
A reply body that is not text, or a malformed state, refuses the task instead of
dropping it. Issue lookup is refused when the environment or user settings enable
offline mode.
Issue lookup also requires source-project trust. Git and GitHub CLI host calls use
validated executables outside the repository. Worker and check-supervisor PATHs
exclude project directories, relative entries and unsafe aliases; project tools
can still run by explicit path inside the sandbox.
Use `oxa work --pr 123 --model your-model` for a pull request in the source
repository. The runner supplies its body, discussion, unresolved inline reviews,
failing checks, and changed files from the reviewed commit. Fetch that commit and
set the local PR branch to it before running. The runner refuses a missing or
different local tip. The worker starts at the reviewed commit and keeps the same
tool restrictions as an issue worker. Truncated sections request missing data
from the runner. Each reviewed commit has its own task ID and frozen packet.
When a newer reviewed commit is already on the local branch, a finished clean
task worktree for the same PR is left in place at a detached HEAD. The next round
gets its own worktree. Uncommitted changes or an active task block the transition.
For runner-owned issue and PR packets the checkout, acceptance, save, and saved
verification supervisors run under the same GitHub and network restriction.

Branches must be literal names; checkout-history aliases such as `@{-1}` are refused.
A local base branch that lags behind the commit the base names (for example a local
`main` after a fetch, though the runner itself never fetches) is fast-forwarded to that
commit for the new worktree, so the task's own diff against its base shows only its own
work. The branch is moved only when it is a strict ancestor of the base and no worktree
has it checked out; otherwise it is left as it is. That move is host metadata, so no
repository hook runs for it.
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
User-level resources follow normal settings for tasks without a GitHub packet.
External acceptance uses the approved source project and the batch's frozen command list. The runner
creates detached Git metadata with callbacks suppressed on the host, then creates
the task branch and populates the fresh worktree inside the normal sandbox.
Reference hooks, checkout hooks and smudge filters stay inside it; a reference hook
that rejects the task branch still blocks the task.
Missing bwrap blocks before checkout or a worker starts. Configured hook directories
keep their normal read-only protection: a checkout that needs to populate a tracked
protected hook directory blocks with the worktree retained.
Acceptance commands are frozen from the captured base commit before checkout
callbacks can change settings. The runner executes those `done` commands outside the
agent after it exits, even without edits. The runner also derives a local compile
precheck from the Cargo packages touched in the diff against the captured base and
in untracked files. The owner can configure `oxa.work.localChecks` in user settings.
It defaults to `["cargo check"]`; the runner supplies `-p <crate>` for each selected
package. Workspace-wide changes and shared inputs outside packages in a tree with
Cargo manifests require hosted coverage. These changes do not select a package
for a local compile check. A successful local compile still leaves Rust work
unverified pending tests in hosted lane CI at the exact head.
The worker can name additional acceptance crates by ending its final reply with
`<oxa-acceptance>["crate-name"]</oxa-acceptance>`; a block that is not readable crate
names, or a crate that is not part of the repository, keeps the task unverified.
The source project must pass the existing
trust and executable-content approval before a task starts. Invoking `work` does
not approve an untrusted project. Acceptance runs in the normal bwrap sandbox;
missing or unusable bwrap blocks the task. Checkout has a ten-minute limit. Each
check and the change inventory have a ten-minute ceiling within the remaining task deadline.
`in subdirectory: command` selects a folder inside that sandbox. The runner rejects
paths that escape the worktree. Each working-tree check round also
gets a private writable output directory beside the worktree (exposed as
`OXA_WORK_OUTPUT_DIR`, `CARGO_TARGET_DIR` and `TMPDIR`), so generated build outputs
never dirty the work the round judges; committed rounds keep their export-local one.
The final Git inspection is also sandboxed, with a read-only workspace, no network
and fsmonitor disabled. Clean filters stay inside that sandbox; an inspection
error blocks the task rather than producing a clean verdict.
Agent-internal done checks may also run when the project is trusted.
Each round records the evidence its verdict rests on: the task's record carries one
`checks` entry per check run with its origin, command, exit status, timeout and
resulting status. Saved verification (`oxa work verify`) evaluates the exact saved
commit and still requires hosted test evidence for Rust changes or declared suites.

The runner reads the child's exit code, terminal `oxa_result` and session header,
without retaining its event stream. It checkpoints the session ID as soon as the
header arrives. A blocked or failed worker remains blocked; an interrupted worker
remains interrupted. Neither consumes a repair attempt. Failed acceptance after a successful
worker continues that same saved session once, then acceptance runs once more.
JSON lines include the worker result, session ID, captured `base_sha`, saved
`head_sha`/`tree_sha`, step, checkpoint revision and time, and the `checks` evidence
of the last acceptance round. Outcomes are `done`,
`nothing_to_do` (no worker commits or file changes), `red` (checks still fail),
`unverified` (checks were absent, could not run, or still require hosted evidence),
`blocked` (setup or
execution failed), or `interrupted`. Deadline interruptions
carry a typed `interruption: "deadline"`. Exit codes are 0 for success, 1 if any
task is red, unverified, blocked or interrupted, and 2 for invalid input, output or checkpoint
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
directory), outside the worker's writable Git bind.
The configured agent directory must also remain outside repositories, task
worktrees and Git metadata; an unsafe placement refuses admission before writing
owner state. New receipts are published
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

`oxa work verify <id>` rechecks a saved successful task from its exact head in a
fresh sandboxed checkout with an independent, strictly validated object store.
It uses the captured base's checks and requires unchanged head/tree identities
and a clean tracked tree before and after checking. The checkout and Git metadata
are read-only during committed checks, so transient source substitutions cannot
pass. A separate private writable output mount is supplied as
`OXA_WORK_OUTPUT_DIR` and `TMPDIR`; `CARGO_TARGET_DIR` points to its `target`
subdirectory. Checks needing other generated outputs must direct them into that
output mount. Retained ignored helpers are absent. A separate index built from the verified commit stays outside the checks'
writable binds; final read-only inspection uses it rather than trusting index
flags or staged entries left by a check. Verification preserves the saved offline restriction and any current
offline request. It prints a JSON observation with the task, head, tree, base and
check time, and retains the task's work unchanged. It starts no worker and does
not push or create a pull request. The library returns a non-deserializable
`VerifiedTask` holding the task lock and checked object graph; a publisher must
consume that live verification rather than trust an old receipt or printed JSON.

To recover a task, run the same task file again. An unfinished checkpoint resumes
its saved step with the original base, commands, session and offline policy.
Recovery loads those inputs before resolving the current base branch or settings,
so a deleted base ref or invalid new checks cannot strand an existing attempt.
Finished successful work is returned without another worker or check run; add
`--rerun` to the task-file or issue command to start another attempt. A task spec
change requires a new ID. Unsupported or corrupt checkpoints are preserved and
refused. A matching linked worktree is reused; the
source checkout is never used as the task worktree. An existing branch checked
out elsewhere is blocked. A new task branch that is already published starts at
its pushed head instead of the base: the branch is created at the lone
remote-tracking tip of the same name when that tip descends from the captured
base (never for several diverging tips or an unrelated one), so reviewing or
continuing a pull request works from its head. Fetch first; no fetch is run here.
Dirty retained work is preserved and saved before
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
`oxa work tasks.json --publish` pushes the accepted, saved commit to `origin` and
creates or updates its PR through `gh`. Sign in with `gh auth login --hostname HOST`
for the host in `origin`. Publication uses that host's saved login. Red acceptance never pushes; green
acceptance makes the PR ready. Blocked tasks do not create PRs. Drift-only accepted reruns may update an existing PR.
The runner checks existing PR queue membership before every push and refuses a
queued PR; use a new task branch for follow-up work. It refuses branch changes
made after acceptance until `--rerun` checks them. PR bodies include task ID and
acceptance state, without raw prompts, transcripts or failed-check output.
Execution preserves a saved offline restriction across restarts; later explicit
publication uses the current network policy without changing that restriction.
A publication failure keeps a publication checkpoint; another `--publish` retries
without replaying the agent. Offline mode refuses network publication.
Before each push, the runner asks through the controlling terminal for approval
of the accepted commit, branch and PR destination. Piped input cannot approve it.

### What the publication scan refuses

Before it asks for approval, `--publish` (and a lane push) scans every commit it would push
that `origin` does not have: the commit objects, the names of added files and every added
line. It refuses, and names only the file and line, when it finds a private key, a known
access token format (GitHub, Anthropic, AWS, Slack, Google, GitLab; also when a letter or digit
directly precedes the prefix), a `password`/`token`/`secret`-style assignment or quoted
literal with an opaque value (spaces inside quotes do not hide it), a `Bearer` header or URL
password, or the value of a secret-named environment variable or setting. Lines added to a
secret store (`.env`, `credentials`, key and certificate files, registry logins such as
`.docker/config.json`) are refused. An empty secret-store file and a template such as
`.env.example` are not refused by name: they publish unless their content holds one of the
secrets above. A path that itself contains secret text is refused and withheld.

Deleting a credential in a later commit does not unblock publication, because the earlier
commit is still pushed. Rewrite the unpublished commits (`git rebase` or a fresh branch), and
rotate a credential that left the machine. The scan reads only git objects; it does not
change them.

`--report-problems` independently authorizes filing the task's own structured
problem notes using the `report-issue` workflow. This flag must be launched by the
user outside an agent tool. Each new report or comment shows its exact cleaned
text and requires controlling-terminal confirmation, as `oxa report-issue submit`
does. Without confirmation it stays a local draft. Reports use the user setting `oxa.reportIssue.repo`;
project settings cannot choose a destination. Notes, titles and queries pass
through the existing report scrubber. The runner performs two or three symptom
queries against open issues and PRs and reads the top matches before writing.
It comments only new notes on a matching issue, links an existing fixing PR, or
files a new report with related links. Search failure, offline mode or missing
report configuration keeps a local scrubbed draft and records its path/detail.
A later explicit `--report-problems` retries local reports without replaying the
agent. A draft-write failure records a failed report and finishes the task checkpoint;
`--report-problems` can retry it. Reports contain only scrubbed structured symptom
summaries, without raw assistant output or acceptance logs.


Queue mode watches a directory of regular, non-hidden `*.json` task files.
Each file may contain up to 256 tasks and occupy up to 1 MiB. The per-file task
count is an advisory, configurable threshold: `--max-input-tasks N` sets it for
this run (default 256, between 1 and 4096); a file with more tasks still
contributes its first N tasks, and one notice names the limit and what was kept.
The kept records then go through the normal validation and admission, so a
duplicate id, a shared branch or a retired identity can still refuse an
individual task. The count notice is informational: it never counts as a failed
input or a failed task. The byte limit, the directory bounds and the identity
retention below do not change with that option. A queue accepts
up to 256 task files and 16 MiB of input per scan. The controller retains at
most 4,096 task identities and 16 MiB of task text across scans. Archive
completed inputs and restart the controller if it reaches the retention limit.
After adding task files, run
`oxa work --queue ./tasks --max 2 --min-free-mib 1024`; add
`--max-input-tasks 64` to bound each file more tightly for that run.

Files are admitted oldest modification time first (filename breaks ties), with
array order preserved inside a file. At most `--max` tasks run; surviving workers
from an interrupted controller still occupy slots until their locks release.
Task IDs must be unique across the queue. Completed task state prevents agent
replay on restart. Files remain available for inspection and manual archiving.
Write new files with an atomic rename so the runner reads a complete task file.

The default free-space floor is 512 MiB. The runner checks the repository, common
git directory, worktree storage and queue filesystems before each admission. A low
or unreadable free-space value pauses new work while active jobs finish. Invalid
JSON files report an error and do not skip other work. `--once` drains available
work and exits; when disk space blocks pending tasks it exits 1 with their files
retained. Without `--once`, the directory stays under watch for new files.
Appending tasks to an existing file queues only new IDs. Discovered task definitions
are immutable; changing one requires a new ID. Correcting or archiving an invalid
input clears its current queue errors. Archiving inputs does not interrupt admitted
work. A controller or monitor error stops admission and drains active workers
before returning an error.

Queue mode supports `--publish`, `--report-problems` and `--rerun` with the same
explicit policy as single-file mode. Its worker threads preserve the parent's
offline policy. Standard output emits JSON lines tagged `state`, `task`, `error`
or `notice`; a `notice` is informational and never counts as a failed input or
task.
The latest state line is also replaced atomically at
`<queue>/.oxa-queue-status.json` for a monitor. It contains a heartbeat timestamp,
active task IDs, pending/finished/failed counts, free bytes and disk-pause details.
A controller lock prevents two queue controllers from exceeding the worker cap.

The native board adapter runs one previously admitted packet from that board's
existing spool. Use `oxa work --native-queue --config PATH` from the source
repository with the same private config used by refill and `nativeExecution`
explicitly enabled. The owner-protected config supplies the registered `worker`
and `host_id`. Launch requires a matching hosted claim and the original binding
before taking the packet. This execution path uses the offline policy. It moves the
matching packet into `started` and resumes from the task checkpoint after
interruption. The original claim releases after the whole task finishes and
its worker process exits.

Set `nativeActivity` to `true` in the same protected config to record verified
worker results on the original hosted claim. The setting defaults to off.
A saved worker result keeps its launch identity across checkpoint replay.

Problem-report workflows hold a per-user kernel lease across drafting, duplicate
search, match reads and writing, so concurrent workers share one reporting lane.
