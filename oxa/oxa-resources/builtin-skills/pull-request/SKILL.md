---
name: pull-request
description: The pull-request workflow - branch, commit, push, open a PR, read reviews and follow up, with the rules for merge queues, force-pushes and merging. Use when the task is to open, update or answer a pull request.
---

# Pull requests

AGENTS.md and CONTRIBUTING.md override anything here (branch names, tiers, draft rules).

## Branch and commits
- Branch from the base: `agent/<task-id>-<slug>` unless the project says otherwise.
- Measure your own change from where the branch left the base, not from the commit it started at:
  `git diff $(git merge-base origin/<base> HEAD) HEAD`. A branch whose base has moved since it
  started shows the base's own progress as deletions in a diff between the two commits, so a
  small change reads as a destructive one. Look at that drift before describing the change.
- Commit with the repository's configured identity (`git config user.name`, `user.email`). Never invent or set one. If it is empty, stop and ask.

## Open
1. Run the project's done checks first.
2. `git push -u origin HEAD`.
3. `gh pr create --draft --base <base> --title ... --body ...`. The body states what changed and why, how it was verified (each command and its result) and which model wrote it.
4. Mark it ready (`gh pr ready N`) only when the done checks are green.

## Follow up
1. `gh pr view N --comments` and `gh api repos/OWNER/REPO/pulls/N/reviews`; read the newest first.
2. Answer every finding with a commit or a reasoned reply.
3. Comment once with a list: finding -> commit (or reason).

## Rules
- Before every push check the merge queue (`gh pr view N --json isInMergeQueue,mergeStateStatus`). Never push to a queued PR.
- Never force-push. Update with a normal merge of the base branch.
- Never merge, enqueue or enable auto-merge unless the user says so.
- If a push or `gh` call fails on credentials, report it. Do not work around it.
