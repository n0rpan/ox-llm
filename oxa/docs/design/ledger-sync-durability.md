# Ledger sync cost and the durability contract

Status: design only; no runtime change, no decision taken. Task #879 (parent #864, owner O-383),
milestone v0.2.2, label `after-0.2.1`. Nothing here is implemented, and no gain is claimed.

Verification is source-only at `69a81515ad3ce39dbf048112fd0cfad476f4b73f`, and every claim in this
revision was re-read against that tree. No build, benchmark, provider call or hardware run was
performed for this packet. The reviewer's "four syncs" and "5–30 ms" figures are a path hypothesis
and an unmeasured estimate; the sync *count* below is derived from the source, the *cost* is not
measured.

## 1. What the current code does

One metered request touches two private files in the daily ledger directory: the append-only row
file `requests-<day>.jsonl` and the reservation snapshot `requests-<day>.jsonl.holds`. Admission
and settlement serialize on the daily `.budget-lock` flock taken by `Shared::open`
([reservations.rs:92](../../oxc-gateway/src/reservations.rs#L92)); the row append takes an
exclusive flock on the row file itself, held only for that append
([spend.rs:193](../../oxc-gateway/src/spend.rs#L193)). The two files are therefore not covered by
one lock.

| Phase | Step | Sync |
|---|---|---|
| Admission | `Shared::open` creates `.holds` on the first open of the day | one `fsync` per ancestor directory, then one `fdatasync` for the initial empty snapshot ([reservations.rs:201](../../oxc-gateway/src/reservations.rs#L201)) |
| Admission | `insert` then `save` after the budget check | one `fdatasync` ([reservations.rs:211](../../oxc-gateway/src/reservations.rs#L211)) |
| Settlement | `append_private` writes the row | one `fdatasync` ([spend.rs:208](../../oxc-gateway/src/spend.rs#L208)) |
| Settlement | `append_private` syncs the ledger directory | one `fsync` ([spend.rs:211](../../oxc-gateway/src/spend.rs#L211)) |
| Settlement | `save` after removing the hold | one `fdatasync` ([reservations.rs:211](../../oxc-gateway/src/reservations.rs#L211)) |

Steady state is therefore **four sync syscalls per request**: three `fdatasync` and one directory
`fsync`. The first admission of a day adds one `fsync` per ancestor directory of the ledger
directory plus one extra `fdatasync` for the initial snapshot. `Shared::open` also runs twice per
request (admission and settlement), each time taking the flock and reading the whole snapshot;
`SpendReader::read` reads the ledger incrementally. Those are I/O but not syncs, and they are
confounds for any latency measurement.

The directory `fsync` at [spend.rs:211](../../oxc-gateway/src/spend.rs#L211) is load-bearing, not
decoration: it is the only thing that makes the *name* of the row file durable, and it runs before
`finish` removes the hold ([accounting.rs:437-443](../../oxc-gateway/src/accounting.rs#L437)). The
existing strace test asserts exactly that ordering
([process_budget.rs:551](../../oxc-gateway/tests/process_budget.rs#L551)).

## 2. Invariants that must not change

- **I1 no unlogged spend.** Every request that may have been paid has either a durable complete row
  or a retained hold.
- **I2 no lost hold.** A hold is removed only after its row is durable in contents *and* name.
- **I3 no double settlement.** A hold id is removed at most once; a second `finish` for the same id
  refuses ([accounting.rs:434](../../oxc-gateway/src/accounting.rs#L434)).
- **I4 no overspend.** Admission sums durable rows plus persisted holds under the daily lock, so a
  hold must be at least as durable as the admission decision that relied on it.

Conservative reservation retention, the daily budget caps, per-day account isolation and the
`redact` call before the append ([ledger.rs:425](../../oxc-gateway/src/ledger.rs#L425)) stay exact
under every option below. No option changes what a row contains or who may read it.

## 3. Crash-ordering table

| # | Window | Durable before the step | Crash leaves | Invariants | Verdict |
|---|---|---|---|---|---|
| C1 | First append of the day, between the row `fdatasync` and the directory `fsync` | the hold snapshot (`fdatasync`), and the directory names from the fresh ancestor syncs | row bytes possibly on disk, the row file's *name* possibly absent; the hold is still present because `finish` has not reached the removal | I1, I2, I4 hold | conservative; this is the window the directory `fsync` exists for |
| C2 | First append, between the directory `fsync` and the settlement snapshot's `fdatasync` | row contents and name | row durable, hold still present | I1, I2, I4 hold | conservative over-reserve, never overspend: the day counts the row and the hold it never removed, and no recovery pass clears the double count (section 7 plans none) |
| C3 | Existing append (name already durable), between the row `fdatasync` and the directory `fsync` | the name from an earlier successful directory `fsync`; the row's data and size from `fdatasync` | row durable, hold still present | I1, I2, I4 hold | the directory `fsync` adds no durability for this row; it is redundant *if* name durability is already established and provable |
| C4 | Torn write, mid `write_all` of a row or of the snapshot | the previous complete rows and the previous snapshot | row: an unfinished final line without `\n`, not counted by the reader ([spend.rs:84](../../oxc-gateway/src/spend.rs#L84)) and repaired before the next append ([spend.rs:201](../../oxc-gateway/src/spend.rs#L201)); snapshot: partial JSON, so the next open fails closed ([reservations.rs:111](../../oxc-gateway/src/reservations.rs#L111)); the reader commits its cursor and totals only when every complete row passed ([spend.rs:157-161](../../oxc-gateway/src/spend.rs#L157)), so a later complete row with an invalid cost refuses instead of silently recounting | I1, I2, I4 hold | fail-closed; no unlogged spend |
| C5 | Directory creation and first `.holds` initialization | nothing | crash before the ancestor syncs: the `.holds` name may be absent, but no hold was ever admitted, because admission returns only after `insert` and `save` succeed ([accounting.rs:191-192](../../oxc-gateway/src/accounting.rs#L191)) and the in-memory hold cannot survive the process; crash after them but before the snapshot `fdatasync`: an empty or partial body, which the next open rejects ([reservations.rs:111](../../oxc-gateway/src/reservations.rs#L111)) | I1, I2, I4 hold | no admission is possible before the snapshot is durable (`Shared::open` saves a fresh snapshot before returning, [reservations.rs:142-144](../../oxc-gateway/src/reservations.rs#L142)); a failed sync leaves the file untrusted by other processes, as the comment at [reservations.rs:198](../../oxc-gateway/src/reservations.rs#L198) states and the injected-EIO test checks |
| C6 | Name replacement or same-inode rewrite | the previous rows | `SpendReader` resets on a `(dev, ino)` change or a shorter file ([spend.rs:52](../../oxc-gateway/src/spend.rs#L52)) and on a changed 64-byte tail ([spend.rs:59](../../oxc-gateway/src/spend.rs#L59)); the reader's own `fdatasync` ordering is unchanged here, but a replacement that keeps the length and the tail is recounted or trusted on a stale tail, so spend is under-counted until the next row. Today the hold and the next append bound that window (section 4) | I1, I2, I4 hold | not a sync-ordering window in A; becomes one under B, whose witness must be invalidated on identity change and whose storage must not be the rewrite-in-place snapshot |
| C7 | Process death after forwarding, before settlement | the hold snapshot | no `Drop` runs, the hold stays, `failed` is not set, the row was never written; other processes count it in `reserved()` ([reservations.rs:147](../../oxc-gateway/src/reservations.rs#L147)) | I1, I2, I4 hold | conservative and deliberate: no timeout or PID-based release of possibly paid work; the stale hold is never released, so it over-reserves for the rest of the day |
| C8 | Sync error at any of the four points | the previous state | row or directory sync error: `finish` sets `failed` on both the process state and the snapshot, keeps the hold, and later admissions refuse ([accounting.rs:437-443](../../oxc-gateway/src/accounting.rs#L437)); snapshot sync error: `state.failed` and a ledger refusal ([accounting.rs:445-448](../../oxc-gateway/src/accounting.rs#L445)); a `Drop` that syncs the snapshot sets only that process's `state.failed` ([accounting.rs:468-472](../../oxc-gateway/src/accounting.rs#L468)); ancestor sync error on the fresh path: the empty file stays untrusted, which the existing test checks in its second half | I1, I2, I4 hold | fail-closed; the injected `fsync:error=EIO:when=1` test covers the fresh-path ancestor sync, and the remaining points need the per-point injection of section 7 |
| C9 | Concurrent processes | the daily flock | every admission and settlement state change — every snapshot read and write — happens under the daily flock ([reservations.rs:92](../../oxc-gateway/src/reservations.rs#L92)); the row append takes an exclusive flock on the row file and the writer mutex ([ledger.rs:418](../../oxc-gateway/src/ledger.rs#L418)), so a concurrent admission can read the rows while an append holds the daily lock | I1–I4 hold | an admission that overlapped an in-progress append already counted that request's hold, so the overlap is conservative; a directory `fsync` by one process is not observable by another, so cross-process name-durability evidence needs a durable witness |

## 4. When inode and name durability are already established

`fdatasync` on a regular file flushes its data and the metadata needed to read it back, including
the size. It does not flush the directory entry that names the file. The row file's name is created
by `create(true)` in `append_private` ([spend.rs:184](../../oxc-gateway/src/spend.rs#L184)), and the
only directory `fsync` that covers that name is [spend.rs:211](../../oxc-gateway/src/spend.rs#L211).
Per-request, the `.holds` name creation is covered only by the fresh-path ancestor syncs of its first
`save`; on a later day those names already exist. The directory `fsync` also flushes the other names
a request created in that directory — `requests-<day>.jsonl`, plus `.budget-lock`, an adopted
`.holds` and `.salt` when the first request of the day is also the first process of that ledger —
which is how they too survive a power loss.

Two consequences follow.

1. After any successful `finish`, the row file's name is durable, because the directory `fsync`
   precedes the hold removal. A later append to the same `(path, dev, ino)` does not need another
   directory `fsync` for its own row: the row's `fdatasync` covers its contents and size, and the
   name was already made durable.
2. That fact is durable on disk, but a *process* may only rely on it if it can prove the name it is
   appending to is the name that was synced. The process that performed the directory `fsync` knows
   it. A second process does not: it sees the file, and existence in the page cache is not
   durability. Cross-process evidence needs a durable witness, for example the row file's
   `(dev, ino)` recorded in the `.holds` snapshot, whose name is durable from its initialization
   (the fresh-path ancestor syncs, or an earlier day's directory `fsync`). The witness must be
   invalidated when the identity changes or the file is missing, and it must be durable before the
   first hold removal that relies on it.

   `save` cannot carry that witness as it stands: it rewrites every snapshot with
   `set_len(0)` then `write_all` under the daily lock ([reservations.rs:208-211](../../oxc-gateway/src/reservations.rs#L208)),
   so a crash inside a rewrite truncates the snapshot to its first partial bytes, and the ledger
   directory is never synced for it. Before the first witness this is harmless — the truncated body
   refuses replay (C4) and the holds it held stay conservative in the refused state. From the first
   witness on it is not: `SpendReader` resets on a shorter file
   ([spend.rs:52](../../oxc-gateway/src/spend.rs#L52)) and on a changed 64-byte tail
   ([spend.rs:59](../../oxc-gateway/src/spend.rs#L59)), so a replacement that keeps a length and a
   tail is counted from offset zero or trusted on a stale tail, and spend is then under-counted
   until a row is appended. Holds cap the window for as long as they exist, which is why the current
   contract holds; an elision that removes the directory `fsync` must not widen that window. B
   therefore needs the witness in a file that is only ever appended to or replaced atomically
   (rename), not in the rewrite-in-place snapshot, and C6 is in B's scope rather than adjacent to
   it.

The directory `fsync` also flushes the other names in that directory (`.holds`, `.budget-lock`,
`.salt`). Removing it does not remove the fresh-path ancestor syncs, which are what establish those
names.

Adjacent observation, out of scope: `install_salt` ([ledger.rs:50](../../oxc-gateway/src/ledger.rs#L50))
syncs the temporary file and hard-links it to `.salt` without a directory `fsync`
([ledger.rs:63-77](../../oxc-gateway/src/ledger.rs#L63)). The `.salt` name becomes durable at the
first admission, through the fresh ancestor syncs, and later at the directory `fsync` of the day's
first row. A crash before either can lose the salt, which changes history bindings and refuses
replay; it does not affect spend, holds or caps. This packet does not propose a change for it.

## 5. Options, compared without implementing any

| | A: current | B: parent-sync elision | C: hold+row journal |
|---|---|---|---|
| Syncs per request, steady state | 3 `fdatasync` + 1 directory `fsync` | 3 `fdatasync` | 2 `fdatasync` |
| First request of the day | ancestor `fsync`s + 2 `fdatasync` | unchanged | ancestor `fsync`s + 1 `fdatasync` |
| Snapshot rewrite per request | yes, O(holds), bounded by `MAX_HOLDS`/`MAX_BYTES` ([reservations.rs:14](../../oxc-gateway/src/reservations.rs#L14)) | unchanged | no; one append per phase |
| New evidence needed | none | a durable name witness plus identity invalidation | record framing, replay bounds, compaction, a replacement for the `failed` flag, migration of existing `.holds` files |
| Windows that change | none | C3 only; C1 and C6 become the risk if the witness is wrong | C1–C4 recovery is rewritten |
| Hold-removal ordering | directory `fsync` before removal | witness durable before removal | journal record durable before removal |
| Risk | the measured cost | a wrong witness turns C1 into unlogged spend, the exact failure the `fsync` prevents | largest change; recovery correctness is the whole contract |
| Tier | — | T2/T3 | T3 |

Option B is a bounded change: no new sync is needed, but the witness needs a home whose durability
does not depend on the rewrite-in-place snapshot (section 4), and `append_private` is a public
function used by the ledger and by tests, so its signature or its caller changes. Option C removes
the per-request snapshot rewrite, which is the part that scales with the number of open holds, but
it replaces the recovery contract that the current fail-closed rules and their tests encode.

## 6. The orchestrator decision question

**Which durability contract should the daily cloud ledger keep for v0.2.2?**

- **A — keep the current contract.** Four syncs per request, directory `fsync` before every hold
  removal. No code change; only the measurement and fault-injection harness lands.
- **B — elide the directory `fsync` behind a durable name witness.** Keep the hold-removal ordering
  and the snapshot format's fail-closed rules; add the witness, its identity check and its
  invalidation, and prove C1/C3/C6 again.
- **C — replace the snapshot and the row append with one hold+row journal.** Two `fdatasync` per
  request, replay-based recovery, compaction, and a migration for existing `.holds` files.
- **D — defer.** Keep A, land the measurement and fault-injection harness first, and decide from
  receipts rather than from this packet.

Only an explicit answer releases a separate implementation PR. This packet infers no approval, and
no gain or approval may be inferred from source inspection.

## 7. Synthetic fault-injection test plan

The harness already exists in [process_budget.rs](../../oxc-gateway/tests/process_budget.rs): a
worker mode of the test binary with a line protocol (`go`, `forward`, `release`, `settle`, `fail`,
`partial-fail`, `drop`, `commit-then-crash`, `crash`) and a strace wrapper that traces
`fsync,fdatasync,ftruncate` and can inject `fsync:error=EIO:when=1`. The ordering test at
[process_budget.rs:551](../../oxc-gateway/tests/process_budget.rs#L551) already asserts that the
directory `fsync` sits between the row's `fdatasync` and the hold removal. Only `fsync` is injected
today; the tests use `when=1` once, on the fresh path.

The implementation PR adds, without touching production code:

1. **Three mechanisms, three different claims.** They do not substitute for one another:
   - *Ordering*: the strace trace of `fsync,fdatasync,ftruncate`. This is the only mechanism that
     proves the ordering invariant, and it proves nothing about what survives a crash.
   - *Sync error*: `-e inject=fsync:error=EIO:when=N`, or the same per point for `fdatasync` and
     `ftruncate`, fails one sync while the process stays alive, which exercises the fail-closed
     paths of C8. The existing test injects only the first `fsync`, which on the fresh
     path is the ancestor sync ([reservations.rs:201](../../oxc-gateway/src/reservations.rs#L201)),
     so the row `fdatasync`, the directory `fsync` and both snapshot `fdatasync` points are still
     unexercised.
   - *Process death*: `-e inject=fsync:signal=SIGKILL:when=N` / `fdatasync:signal=SIGKILL:when=N`
     kills the worker at a sync boundary, and the recovery assertions run in a fresh process. A
     test-only exit inside the write callback covers the boundaries that are not syscalls (mid-row,
     mid-snapshot).
2. **What process death cannot prove, and why B needs more.** A SIGKILL is not a power loss: the
   skipped sync leaves the bytes already written dirty in the page cache, and the kernel writes
   them back afterwards, directory entries included. So a kill at a sync boundary leaves the
   *post-sync* on-disk state, not the pre-sync one, and no SIGKILL can produce the C1 state ("row
   bytes present, the row file's name absent"). "Either a row or a hold survives" therefore passes
   trivially under SIGKILL and says nothing about C1 or C3; the kill cases are real tests of hold
   retention, refusal and recovery (C2, C4, C5, C7, C9), not of durability ordering. Discarding
   unsynced writes needs a layer below the filesystem (a VM whose write cache is dropped on power
   off, or a block-device fault layer); none exists in CI today and this packet does not add one.
   Consequence for the decision: A's ordering is provable now, while B's own risk — a wrong name
   witness turning C1 into unlogged spend — is not provable by the current harness, so B must bring
   that harness first.
3. **Windows.** One case per row of the table: W1 = C1, W2 = C2, W3 = C3, W4 = C4 (row and
   snapshot), W5 = C5, W6 = C6, W7 = C7, W8 = C8 at each of the four sync points of section 1 plus
   the fresh-path ancestor syncs, W9 = C9 with the crash injected in one of eight concurrent
   workers. W1, W3 and W5 are asserted by the trace, and their crash form is marked as requiring
   the write-cache layer of item 2 rather than claimed as covered by a kill.
4. **Assertions after each window**, in a fresh process:
   - no unlogged spend: for every request a worker reported as forwarded, either a complete row is
     in the ledger or its hold id is still in the snapshot; concretely, the sum of complete row
     costs plus `reserved()` of the snapshot is at least the sum of the reservations the workers
     reported. This is a lower bound, not equality: a conservative settlement writes the reserved
     estimate ([spend.rs:157-161](../../oxc-gateway/src/spend.rs#L157)) and stays counted until a
     recovery pass exists, which no option in section 5 proposes;
   - no lost hold: every hold id in the pre-crash snapshot is either still present or absent
     together with a matching durable row;
   - no double settlement: a second `finish` for the same id cannot be attempted, because `finish`
     consumes the hold, so the reachable statement is narrower than the one above and the packet
     says so: `finish` refuses only when the id is absent from the snapshot
     ([accounting.rs:434](../../oxc-gateway/src/accounting.rs#L434)) — that refusal is unpinned — and
     a settlement replayed over an existing row appends a second row instead of refusing, a
     recount that the hold bounds (C6) but no refusal. The reader counts a duplicated row twice,
     since it has no row identity ([spend.rs:152](../../oxc-gateway/src/spend.rs#L152)), and the
     existing reader test
     ([accounting.rs:321](../../oxc-gateway/tests/accounting.rs#L321)) only pins the
     concurrent-torn-row count, not replay. Pinning the absent-id refusal is a pure test addition;
     making replay refuse needs row identity, a new row field and a new decision;
   - no overspend: after recovery, the existing eight-worker cap test
     ([process_budget.rs:277](../../oxc-gateway/tests/process_budget.rs#L277)) admits no more
     requests than the cap allows.
5. **Preserved tests.** Every existing test in `tests/accounting.rs`, `tests/process_budget.rs` and
   the library suites stays, unweakened. The ordering assertion stays meaningful: under B it becomes
   "the witness is durable before the hold removal", under C "the journal record is durable before
   the hold removal".
6. **Environment.** Isolated HOME/XDG, synthetic loopback mock upstream, no real keys and no
   provider traffic, CI only, `CARGO_BUILD_JOBS=3`.

No Rust or Python test is added by this packet: the source tree is unchanged, so there is no
behaviour to pin. The first executable artifacts are the kill and per-point EIO cases of this
section, in the implementation PR, and each must fail before the change it guards passes.

## 8. Measurement plan

- **Filesystem.** A disk-backed filesystem on the CI runner, not tmpfs; record the filesystem type
  and mount options in the receipt. The owner's filesystem is unknown to this packet, and a hardware
  measurement needs the owner's explicit handoff.
- **Sync counts.** `strace -f -qq -yy -e trace=fsync,fdatasync,ftruncate` around one worker; report
  counts per phase and per request, first request of the day versus steady state. This is a count,
  not a timing.
- **Latency.** The existing loopback mock upstream with a fixed synthetic response; measure
  admission to settlement on the gateway path — not the worker line protocol, whose `go` replies once
  for the whole run — for one and for eight concurrent requests, at least 200 samples, reporting
  median, p95 and p99 together with the sync count of the same run. Report the directory
  `fsync` cost separately, both as a micro-benchmark of `File::open(dir).sync_all()` on the same
  filesystem and as the strace timestamp delta between the row `fdatasync` and the directory
  `fsync`.
- **Confounds to report.** The O(holds) snapshot rewrite, the per-request flock and full snapshot
  read, the incremental ledger read, and strace's own overhead. The receipt must say whether the
  syncs or the snapshot rewrite dominate; if the rewrite dominates, B buys little and C is the
  larger win at the larger risk.
- **Receipt.** An immutable new file with the exact head, filesystem, command, sample count and raw
  numbers. No claim without it. Existing receipts are never edited.

## 9. Limitations

- Source-only verification at `69a81515ad3ce39dbf048112fd0cfad476f4b73f`, re-read for this
  revision; no build, benchmark, provider call or hardware run.
- The 5–30 ms estimate is unmeasured and is not repeated here as a claim.
- CI filesystems may not represent the owner's machine; strace changes timing, so counts are
  reliable and latencies are indicative.
- The elision's benefit is bounded by one directory `fsync` per request. Whether that is worth a new
  durability witness is the decision in section 6, and it is the orchestrator's to make.

## 10. Non-goals

No `fsync` or `fdatasync` removal, no settlement change, no journal, no change to caps, ledger rows,
account isolation or redaction, and no change under `oxc/`.
