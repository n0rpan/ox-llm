# Opt-in orchestrator foundations

First independently testable slice of rollup #88 for [issue #282](https://github.com/oxstack/ox-llm/issues/282).
Private audit storage, JSONL pipe transport and an explicit tool-free native
worker owner are present here. No worker, listener or CLI hook starts on import;
no recovery/replay or installed service is enabled.

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

## Bounded JSONL transport

With `core`, `pipes::JsonPipes` takes the parent's explicitly supplied child
stdin/stdout/stderr handles and makes them nonblocking. It creates no process,
thread or listener. The caller remains responsible for sandboxing, permission
checks, cancellation, deadlines and confirmed descendant cleanup/reaping. EOF
reports only pipe state; it does not establish process completion. Dropping the
transport closes its handles and never joins a reader waiting for EOF.

Each poll services at most eight writes and eight 4-KiB reads per output stream.
Pending input and one output JSONL record are each capped at 256 KiB. Input
serialization is bounded before enqueue. Both request and response records must
be JSON objects. Malformed/non-object/overlong records and an unterminated final
record fail the transport; blank lines are ignored. Diagnostic bytes are discarded
and count toward the shared lifetime output allowance. Reads use at most the
remaining allowance plus one detection byte, and `output_bytes` includes that
observed overflow byte (saturating only at the counter's integer limit).

An I/O, protocol or admission error permanently closes input and discards pending
requests. `close_input` can also explicitly discard input while allowing output
to drain. Polling can successfully return no records without EOF. This slice
adds no native RPC launch profile, provider model selection or automatic retry.
Tests use owned local subprocesses with cleared environments and kill/wait guards;
there are no provider calls. The allowance-overflow regression fails before the
accounting fix and passes afterward.

## Explicit tool-free native process owner

With `core`, `native::NativeWorker` launches only an operator-selected executable
through an operator-selected bwrap executable. `VerifiedLocal` records the
operator assertion that its literal loopback route cannot forward to paid/cloud
inference. Address syntax and model validation cannot prove that assertion. This
slice neither authorizes cloud inference nor reads provider credentials.

The fresh filesystem exposes read-only runtime libraries and the chosen program,
private HOME/tmp/work and a new PID/user namespace. Environment/configuration,
source checkout, MCP/extensions/skills and tools are not inherited. The native
profile uses current main's RPC/plan/offline flags and removes the obsolete
OXA_PROFILE setting. Approved workspace mounts belong to a later integration.
The supplied role must already be approved for delegation. It is passed directly
to the trusted executable and is not journal metadata.

The caller must drive `poll`/`queue` to enforce the monotonic wall deadline; there
is no background watchdog. Transport/admission/deadline failure forces cleanup.
`stop` permits at most one second of graceful abort, then bounded SIGKILL/reap
observation. Confirmed child exit is retained explicitly; a failed cleanup is an
error, never a process-stopped claim. Pipe EOF is not process completion. Group
signals use only the retained unreaped Child; recorded PIDs are never adopted or
signalled. PID-namespace teardown covers descendants that change process group.
Drop attempts the same cleanup, but callers requiring evidence must call `stop`
and check its result before dropping the owner.

The real-bwrap fixtures use a trusted mock runtime, not the production oxa binary
or a paid model. They verify flags, cleared/private environment, hidden source,
read-only runtime mounts, a distinct PID namespace, RPC pipe roundtrip, grace,
deadline/protocol/admission cleanup, closed runtime stdout with a live wrapper,
and an inherited
socket lease closing after a setsid descendant is terminated. Process ownership
is not a scheduler, token-accounting policy or proof of provider billing stop.
