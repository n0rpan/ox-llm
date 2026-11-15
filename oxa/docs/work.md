# Unattended work

`oxa work tasks.json` opts into sequential tasks in separate git worktrees.
Completed tasks are skipped on restart; add `--rerun` to deliberately run them again.
Run it inside the project repository. The JSON file is an array:

```json
[{"id":"docs-fix","model":"your-model","branch":"agent/docs-fix","base":"main","prompt":"Fix documentation links."}]
```

`base` defaults to `main`. IDs contain letters, digits, hyphens or underscores.
IDs and branches must be unique within one file. A new branch starts from `base`;
an existing branch is continued with a normal merge of that base before the agent
starts. No remote fetch happens automatically. For a GitHub issue use
`oxa work --issue 123 --model your-model`. This explicitly invokes your installed,
authenticated `gh` CLI to fetch the issue title and body. Issue lookup is refused
when the environment or user settings enable offline mode.

Each task runs a child `oxa -p` with its model. The installed offline policy,
including a parent `--offline` flag, is passed to every child. Normal agent
permission, trust and sandbox rules apply. Configure project trust before unattended use. The runner
executes the base version of `.oxa/settings.json`'s `done` commands outside the
agent after it exits, even without edits. These are trusted project shell commands:
invoking `work` authorizes their execution as your user. `in subdirectory: command`
runs in that worktree subdirectory; escaping it with the directory is rejected.
Agent-internal done checks may also run when the project is trusted.
On continued branches, acceptance commands are read from the captured base commit,
even when the task branch has changed its own settings.

Failed acceptance output goes to one additional child run with the original prompt,
then acceptance runs once more. JSON lines report `done`, `nothing_to_do` (no
commits or file changes), `red` (checks still fail), or `blocked` (setup or agent
execution failed). Exit codes are 0 for success, 1 if any task is red or blocked,
and 2 for invalid input or output errors. A task setup failure produces a blocked
JSON line and does not skip later tasks. If a task cannot be locked or already has
a receipt, its existing state is preserved; a pre-worktree failure has an empty
`worktree` in the result. Locks use kernel ownership, so an abandoned lock file
after process death does not permanently block a task. A surviving child keeps the
lock until it exits, preventing overlap with a restarted runner.

Worktrees and uncommitted changes remain in `.oxa-worktrees/<repository>/<id>`
beside the repository (or the separate git directory). Result receipts live inside
the common git directory at `oxa-work/<id>.json`. Worktrees stay outside that git
directory so the normal sandbox can bind git objects and refs for commits.
Uncommitted task work, including red work, is saved as a commit with the repository's
configured identity. Commit hooks run normally. A missing identity or rejected commit
blocks the task and retains every file. Unresolved conflicts remain in progress for
the agent; they are never staged as a fabricated resolution. There is no automatic
cleanup. Publication is explicitly opt-in as described below.

To continue an interrupted, red or blocked task, run the same file again. Use
`--rerun` to continue a completed task after its base changes. Interrupted execution
always resumes its captured base, checks and progress, even with `--rerun`. The retained receipt
must match its ID, branch and worktree. A clean worktree is reused; a branch checked out elsewhere
is blocked. Existing dirty work is saved when possible before a later retry.
Merge conflicts are passed to the agent with the merge still in progress, the
conflicted file list and merge output. An unfinished merge can be continued on a
later run. The agent must finish the merge commit and keep the task branch selected;
unresolved or aborted base merges remain blocked even when checks would pass. Inspect
retained work before removing it with `git worktree remove`. Review task prompts
and bases before running them.

`oxa work --status` prints one line per recorded task, sorted by ID:

```text
docs-fix done 1800000000 finished
```

Columns are task ID, state, last step-progress time (Unix seconds), and current step.
Running tasks report steps such as `agent`, `acceptance`, `repair` or `saving`.
The state file is replaced atomically with owner-only permissions at each transition.
It records the task specification, captured base/checks, repair output and current
step. Reusing an ID with a different specification is refused; use a new ID.
Restart continues the saved step. An agent or check interrupted within its step
may run again; tasks and checks should tolerate that. Checkpoints after an agent
step avoid replaying it when only acceptance or saving remains.

Status probes the task lock. An interrupted phase with no surviving worker is shown
as blocked; the checkpoint stays intact for the next task-file invocation.


`oxa work tasks.json --publish` pushes the accepted, saved commit to `origin` and
creates or updates its PR through `gh`. Red acceptance creates a draft; green
acceptance makes the PR ready. Nothing-to-do and blocked tasks do not create PRs.
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
After adding task files, run
`oxa work --queue ./tasks --max 2 --min-free-mib 1024`.

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
offline policy. Standard output emits JSON lines tagged `state`, `task` or `error`.
The latest state line is also replaced atomically at
`<queue>/.oxa-queue-status.json` for a monitor. It contains a heartbeat timestamp,
active task IDs, pending/finished/failed counts, free bytes and disk-pause details.
A controller lock prevents two queue controllers from exceeding the worker cap.

Problem-report workflows hold a per-user kernel lease across drafting, duplicate
search, match reads and writing, so concurrent workers share one reporting lane.
