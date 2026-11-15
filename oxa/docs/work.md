# Unattended work

`oxa work tasks.json` opts into sequential tasks in separate git worktrees.
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

Each task runs a child `oxa -p` with its model. Normal agent permission, trust and
sandbox rules apply. Configure project trust before unattended use. The runner
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
and 2 for invalid input or runner errors.

Worktrees and uncommitted changes remain in `.oxa-worktrees/<repository>/<id>`
beside the repository (or the separate git directory). Result receipts live inside
the common git directory at `oxa-work/<id>.json`. Worktrees stay outside that git
directory so the normal sandbox can bind git objects and refs for commits. No automatic
cleanup, commit, push or PR creation is implemented yet.

To continue a task, run the same file again. The retained receipt must match its
ID, branch and worktree. A clean worktree is reused; a branch checked out elsewhere
is blocked. Uncommitted changes are retained and blocked until you commit them.
Merge conflicts are passed to the agent with the merge still in progress, the
conflicted file list and merge output. An unfinished merge can be continued on a
later run. The agent must finish the merge commit and keep the task branch selected;
unresolved or aborted base merges remain blocked even when checks would pass. Inspect
retained work before removing it with `git worktree remove`. Review task prompts
and bases before running them.
