# Opt-in orchestrator foundations

## A2A carry: bounded follow history (#629)

The default-off `a2a` feature exposes `follow::History`, extracted from
#129 `f89ba2e`'s `control.rs` publisher/history and exclusive-after event pages.
It retains at most 128 events / 512 KiB and returns at most 16 events per page.
The reconnect cursor is the last returned event, separately from the latest
sequence. A retention gap requires a fresh snapshot from the session owner.
The #436 pagination regression intent is retained with the exclusive cursor.
Oversized individual events are rejected before changing history or sequence.

This is an in-memory primitive for an existing owner, with no socket, CLI,
thread, task/goal ledger or delivery receipt. Callers must supply approved
summaries; it neither captures nor redacts raw transcripts. Live follow/control
wiring remains a separate carry slice. Normal builds leave `a2a` off; the test
self-dependency enables it so ordinary hosted crate tests exercise the port.

## Existing core foundations

The `a2a`-gated `follow::publisher::Publisher` ports #129 `f89ba2e`'s
session-bound publisher onto the retained history. Owner-supplied status updates
coalesce without advancing event cursors; reply events carry explicit bounded
UTF-8 previews and the identity observed when published. Latest observations and
individual bindings are byte-bounded; invalid publication changes neither cursor
nor latest observation. Clones share one owner's history. Callers still approve
summaries and enforce disclosure authority. No transport, task/goal state owner,
CLI integration or automatic transcript capture is introduced.

Independently testable foundations from rollup #88 for [issue #282](https://github.com/oxstack/ox-llm/issues/282).
Private audit storage, approved text snapshots and private review copies are
present here. No worker, listener, CLI command,
recovery/replay operation, installed service or inference route is enabled.

The `core` feature exports `AuditJournal`. Its caller explicitly supplies a new
journal path under an existing mode-0700 operator-owned directory and a byte
limit in 1..=16 MiB. Ancestors must be operator/root controlled; symlinked
ancestors and parent traversal are refused. Same-UID processes and root remain
the trusted namespace boundary. Records are mode 0600, create-only and fsynced. Creation syncs the pinned parent
directory before returning a usable writer, so successful records also have a
persisted directory entry. Filesystem sync failures are propagated.

Each accepted JSONL record has schema, sequence, elapsedMs, worker, event and
explicit caller-supplied details. `run_started` records name the ownership
protocol. The caller is trusted and must supply bounded labels/hashes/counts,
not raw prompts, outputs or credentials. The library does not capture any of
those values implicitly, and this is not a replay format or an authorization to
signal a recorded PID.

A nonblocking inode lease rejects another controller. An append failure closes
further admission and retains ownership until the writer is dropped. Validation
and size rejection happen before writing; an actual storage error can leave a
partial trailing record, which is uncommitted and must never be replayed. The
writer does not promise rollback after a failed write or sync. Normal
release explicitly unlocks even if an unrelated pre-exec fork temporarily holds
the open-file description; CLOEXEC prevents inheritance across exec. Fatal death
still follows the kernel's last-description-close rule. The writer is not cloneable.

```sh
cargo check -p oxa-orchestrator --no-default-features --lib
cargo test -p oxa-orchestrator
cargo test -p oxa-orchestrator --features core
cargo clippy -p oxa-orchestrator --all-targets -- -D warnings
```

Normal builds leave `core` off. The test-only self dependency enables it for the
ordinary workspace test command too, so CI's default test inventory includes the
seventeen actual filesystem/fork/workspace regressions. Existing binaries do not depend on this
crate. No workflow or scanner change is needed. See [slice plan](SLICES.md) for
the remaining dependency order and preserved rollup sources.

## Explicit source snapshots

With `core`, `workspace::Snapshot::approve(root, paths)` captures only selected
UTF-8 files. The caller must first approve those contents for delegation;
filename filtering cannot establish that arbitrary source text is secret-free.
This operation grants no worker access and performs no writes or execution.
`text` returns a selected in-memory value, and `paths` lists the approved names.
Later source edits do not change the snapshot. Debug prints only the file count
and manifest digest, never paths or content.

`RelativePath` validates construction and JSON deserialization, rejecting traversal,
absolute paths, control characters, empty components, backslashes and protected
credential/configuration names. Bounds are 32 files, 256 KiB per file, 1 MiB total,
16 nested directories and 128 virtual tree entries. Unselected source entries do
not count or get read. The root itself is explicitly selected and canonicalized;
symlinks below that root are refused, including ancestor directories. Files must
be plain single-link files on the root filesystem. Linux O_PATH/openat validation
precedes opening the pinned inode through `/proc/self/fd`, so a FIFO is rejected
without blocking. Metadata changes during the bounded read cause refusal. This
is not an atomic multi-file repository snapshot; the caller approves a quiescent
source. The deterministic manifest hashes sorted path/content-digest pairs.

## Private copies and review data

`Grant::read_only` and `Grant::editing` retain an immutable snapshot. An editing
grant names every eligible new file and refuses duplicate paths and file/parent
collisions. `Arc<Grant>::materialize` creates a fresh mode-0700 tree beneath a
private operator-owned parent, containing only approved mode-0600 text files.
Creation is bound to the validated parent inode. Existing names are never
replaced. A failed materialization may leave a partial tree; the caller decides
how to clean up that owned path. Same-UID/root remains the namespace boundary.

After stopping and reaping the worker, the trusted controller calls `collect`.
The library cannot establish that process-lifecycle precondition itself. It
refuses changed read-only copies, unapproved files, executable files, symlinks,
hardlinks, special files and oversized/deep/excessive trees. These grants govern
review eligibility; a future supervisor must separately confine a worker's tools
and mount access. No worker or sandbox integration is added in this slice.

A versioned `ReviewBundle` contains selected before/after text, digests and a
linear-time deterministic diff. Its serialized size is bounded to 128 KiB.
Raw review data intentionally includes approved source content; callers must not
log or forward it as metadata. Bound an untrusted transport before deserializing.
`verify` checks the original manifest, allowed paths, digests, duplicate changes
and diff/content agreement; then `verify_current_base` checks source freshness.
Neither operation applies edits, provides an atomic apply lock or authorizes
publication. Git worktrees and publication remain later slices.
