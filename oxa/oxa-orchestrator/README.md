# Opt-in orchestrator foundations

First independently testable slice of rollup #88 for [issue #282](https://github.com/oxstack/ox-llm/issues/282).
Only private audit storage is present here. No worker, listener, CLI command,
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
seven actual filesystem/fork regressions. Existing binaries do not depend on this
crate. No workflow or scanner change is needed. See [slice plan](SLICES.md) for
the remaining dependency order and preserved rollup sources.
