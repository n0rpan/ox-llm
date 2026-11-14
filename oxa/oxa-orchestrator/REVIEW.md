# Review selected proposals and stage an approved candidate

The default-off `orchestrator` feature also provides `oxa orchestrate-review`.
It reads a saved A2A run report, validates explicitly selected completed workers,
checks their proposals against an operator-selected source snapshot and prints a
JSON preview. It makes no inference request, starts no worker/listener, discovers
no agent configuration and executes no shell, Git command, hook or test.

From `oxa/`, after building `oxa-cli --features orchestrator`, preview a saved
report using the original source file scope:

```sh
./target/debug/oxa orchestrate-review \
  --report ./report.json --source ./checkout \
  --file src/main.rs --file src/lib.rs --select worker-1 \
  > ./preview.json
```

Repeat `--select` for each proposal to include. Repeat `--allow-new` for eligible
new-file paths, when needed. The report must be a plain single-link file of at
most 16 MiB. At most 16 workers can be selected from a report with at most 128
workers. Selected workers must have completed tasks, known stop status and exactly
one workspace review artifact. Unselected failed workers are not promoted to
success or included. The preview explicitly attributes each selected proposal to
its controller run, worker, A2A task and context.

Inspect the complete preview, especially the before/after content, display diffs,
new-file scope and `conflicts`. All proposals are untrusted data. Hash validation
checks content consistency and scope; it does not authenticate the report's origin
or establish that a proposed edit is correct. No command in returned text executes.
Terminal controls remain escaped in JSON output.

Overlapping paths produce a conflict even when the proposed edits are identical.
The preview exits 1 and has no candidate manifest. Choose a different explicit
worker selection and preview again. There is no automatic winner, conflict
resolution or Git merge. A disjoint selection may combine several proposals.

To stage the exact inspected content, repeat the arguments and supply the
preview's `approval_sha256` plus a NEW destination:

```sh
./target/debug/oxa orchestrate-review \
  --report ./report.json --source ./checkout \
  --file src/main.rs --file src/lib.rs --select worker-1 \
  --approve PREVIEW_SHA256 --directory ./candidate
```

The SHA-256 binds the original source manifest, eligible new-file scope, selected
workers and task identities, full before/after contents/hashes/diffs, conflicts
and candidate manifest. Canonical ordering makes selection/path ordering irrelevant.
Changing any of these requires reviewing a new digest. It is a content approval,
not a one-time token or credential; the same content can be staged at a different
new destination. Approval and destination must both be explicit command arguments.
Worker messages cannot grant approval or invoke this command through their tools.

The command captures the selected source files again and verifies the proposals.
Changes to any approved input, including unmodified context files, invalidate the
original snapshot manifest. It also checks touched paths against their claimed
base, including requiring proposed new paths to be absent. Source checks are per
file, not an atomic repository read or a lock. A candidate represents the captured,
approved versions; later source changes are not incorporated. Any future apply
operation must revalidate again and handle concurrent writers.

Before creating state, staging requires a symlink-free destination path with no
parent traversal, at most 4096 path bytes. Each directory from the filesystem root
through the destination parent must be owned by the effective user or root.
Non-owner-writable directories are refused unless sticky semantics protect their
owner/root-owned entries (for example a private user directory under `/tmp`).
This includes ancestors: a private final directory inside a replaceable group
namespace is insufficient. Same-UID processes and root remain trusted; operators
must retain that protection during publication. Choose a physical private path
if a collaborative directory or symlink would be refused. No existing permissions
are changed automatically.

Staging writes the approved snapshot with selected changes to a private temporary
directory in the destination parent, syncs the files/directories, then publishes
with a descriptor-anchored Linux no-replace rename. Existing files, directories
and symlinks are never overwritten. Normal errors clean up the unpublished
staging directory. A crash/termination may leave a private `.oxa-review-*` staging
directory; it is not a completed candidate. A published candidate contains a
`.oxa-review.json` receipt with the approval/source/candidate hashes and task
attribution. File contents and receipt are private (0600, directories 0700).
The receipt records the staged version; it is not a tamper-proof attestation.
Rehash candidate files at any future validation/apply boundary.
A failure to sync the parent after publication may report an error with a complete
candidate already present; inspect its receipt rather than blindly retrying.

The candidate contains only approved files and selected changes, not a complete
repository unless that entire file scope was approved. Unselected source files,
Git metadata, hooks, credentials, executable bits and parent configuration are
not copied. The receipt name is reserved and cannot be delegated as an input or
new worker path. The original checkout is never mounted or overwritten. The
candidate may be inspected or explicitly delegated to a later reviewer; its receipt
is not automatically forwarded to that reviewer.

This advances the review/merge workflow; Git worktree lifecycle, applying into an
existing checkout with concurrent-writer protection, running validation under a
separately authorized tool grant, committing and merging remain unfinished.
Nothing here authorizes a cloud call or changes the brief's coordinator gates.

Library equivalents are `review::proposals_from_report`,
`PreparedReview::prepare`, `preview` and `stage`. The immutable prepared object
owns the content behind its approval hash; callers cannot edit a preview to alter
what `stage` writes. Report eligibility is checked by `proposals_from_report`;
programmatic callers supplying `Proposal` directly must establish provenance and
completion themselves. Snapshot disclosure/retention and bounds are described in
[WORKSPACES.md](WORKSPACES.md). These bounds also apply to candidate contents.
