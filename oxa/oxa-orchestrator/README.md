# Opt-in orchestrator foundations

First independently testable slice of rollup #88 for [issue #282](https://github.com/oxstack/ox-llm/issues/282).
Private audit storage and explicitly approved text snapshots are present here. No worker, listener, CLI command,
recovery/replay operation, installed service or inference route is enabled.

The `core` feature exports `AuditJournal`. Its caller explicitly supplies a new
journal path under an existing mode-0700 operator-owned directory and a byte
limit in 1..=16 MiB. Ancestors must be operator/root controlled; symlinked
ancestors and parent traversal are refused. Same-UID processes and root remain
the trusted namespace boundary. Records are mode 0600, create-only and fsynced.

Each accepted JSONL record has schema, sequence, elapsedMs, worker, event and
explicit caller-supplied details. `run_started` records name the ownership
protocol. The caller is trusted and must supply bounded labels/hashes/counts,
not raw prompts, outputs or credentials. The library does not capture any of
those values implicitly, and this is not a replay format or an authorization to
signal a recorded PID.

A nonblocking inode lease rejects another controller. An append failure closes
further admission and retains ownership until the writer is dropped. Normal
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
eleven actual filesystem/fork/snapshot regressions. Existing binaries do not depend on this
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

Private materialization, edit grants, returned-change collection and publication
are subsequent slices. No snapshot method applies changes to a checkout.
