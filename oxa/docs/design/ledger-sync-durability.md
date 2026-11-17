# Ledger sync cost and the durability contract

Status: design only; no runtime change, no decision taken. Task #879 (parent #864, owner O-383),
milestone v0.2.2, label `after-0.2.1`. Nothing here is implemented, and no gain is claimed.

Verification is source-only at `69a81515ad3ce39dbf048112fd0cfad476f4b73f`. No build, benchmark,
provider call or hardware run was performed for this packet. The reviewer's "four syncs" and
"5–30 ms" figures are a path hypothesis and an unmeasured estimate; the sync *count* below is
derived from the source, the *cost* is not measured.

## 1. What the current code does

One metered request touches two private files in the daily ledger directory: the append-only row
file `requests-<day>.jsonl` and the reservation snapshot `requests-<day>.jsonl.holds`, both under
the same `.budget-lock` flock.

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
| C2 | First append, between the directory `fsync` and the settlement snapshot | row contents and name | row durable, hold still present | I1, I2, I4 hold | conservative over-reserve, never overspend |
| C3 | Existing append (name already durable), between the row `fdatasync` and the directory `fsync` | the name from an earlier successful directory `fsync`; the row's data and size from `fdatasync` | row durable, hold still present | I1, I2, I4 hold | the directory `fsync` adds no durability for this row; it is redundant *if* name durability is already established and provable |
| C4 | Torn write, mid `write_all` of a row or of the snapshot | the previous complete rows and the previous snapshot | row: an unfinished final line without `\n`, not counted by the reader ([spend.rs:84](../../oxc-gateway/src/spend.rs#L84)) and repaired before the next append ([spend.rs:201](../../oxc-gateway/src/spend.rs#L201)); snapshot: partial JSON, so the next open fails closed ([reservations.rs:111](../../oxc-gateway/src/reservations.rs#L111)) | I1, I2, I4 hold | fail-closed; no unlogged spend |
| C5 | Directory creation and first `.holds` initialization | nothing | crash before the ancestor syncs: the `.holds` name may be absent, but no hold was ever admitted, because admission returns only after `save` succeeds ([accounting.rs:192](../../oxc-gateway/src/accounting.rs#L192)); crash after them but before the snapshot `fdatasync`: an empty or partial body, which the next open rejects | I1, I2, I4 hold | no admission is possible before the snapshot is durable; a failed sync leaves the file untrusted, as the comment at [reservations.rs:198](../../oxc-gateway/src/reservations.rs#L198) states |
| C6 | Name replacement or same-inode rewrite | the previous rows | `SpendReader` resets on a `(dev, ino)` change or a shorter file ([spend.rs:52](../../oxc-gateway/src/spend.rs#L52)) and on a changed 64-byte tail ([spend.rs:59](../../oxc-gateway/src/spend.rs#L59)); a replacement that loses rows under-counts spend, holds are unaffected | I1, I2, I4 hold | pre-existing, not a sync-ordering issue; an elision must invalidate its name witness on identity change |
| C7 | Process death after forwarding, before settlement | the hold snapshot | no `Drop` runs, the hold stays, `failed` is not set; other processes count it in `reserved()` ([reservations.rs:147](../../oxc-gateway/src/reservations.rs#L147)) | I1, I2, I4 hold | conservative and deliberate: no timeout or PID-based release of possibly paid work |
| C8 | Sync error at any of the four points | the previous state | row or directory sync error: `finish` sets `failed` on both the process state and the snapshot, keeps the hold, and later admissions refuse ([accounting.rs:438](../../oxc-gateway/src/accounting.rs#L438)); snapshot sync error: `state.failed` and a ledger refusal ([accounting.rs:445](../../oxc-gateway/src/accounting.rs#L445)); ancestor sync error on the fresh path: the empty file stays untrusted | I1, I2, I4 hold | fail-closed; covered by the injected-EIO test |
| C9 | Concurrent processes | the daily flock | every admission and settlement state change happens under the flock ([reservations.rs:92](../../oxc-gateway/src/reservations.rs#L92)); the row append takes the same lock and the writer mutex ([ledger.rs:418](../../oxc-gateway/src/ledger.rs#L418)) | I1–I4 hold | a directory `fsync` by one process is not observable by another, so cross-process name-durability evidence needs a durable witness |

## 4. When inode and name durability are already established

`fdatasync` on a regular file flushes its data and the metadata needed to read it back, including
the size. It does not flush the directory entry that names the file. The row file's name is created
by `create(true)` in `append_private` ([spend.rs:184](../../oxc-gateway/src/spend.rs#L184)), and the
only directory `fsync` that covers that name is [spend.rs:211](../../oxc-gateway/src/spend.rs#L211).
The `.holds` name is created by `create_new` ([reservations.rs:94](../../oxc-gateway/src/reservations.rs#L94))
and covered by the fresh ancestor syncs.

Two consequences follow.

1. After any successful `finish`, the row file's name is durable, because the directory `fsync`
   precedes the hold removal. A later append to the same `(path, dev, ino)` does not need another
   directory `fsync` for its own row: the row's `fdatasync` covers its contents and size, and the
   name was already made durable.
2. That fact is durable on disk, but a *process* may only rely on it if it can prove the name it is
   appending to is the name that was synced. The process that performed the directory `fsync` knows
   it. A second process does not: it sees the file, and existence in the page cache is not
   durability. Cross-process evidence needs a durable witness, for example the row file's
   `(dev, ino)` recorded in the `.holds` snapshot, whose own name is already durable from the fresh
   path. The witness must be invalidated when the identity changes or the file is missing, and it
   must be durable before the first hold removal that relies on it.

The directory `fsync` also flushes the other names in that directory (`.holds`, `.budget-lock`,
`.salt`). Removing it does not remove the fresh-path ancestor syncs, which are what establish those
names.

Adjacent observation, out of scope: `install_salt` syncs the temporary file and hard-links it to
`.salt` without a directory `fsync` ([ledger.rs:60-77](../../oxc-gateway/src/ledger.rs#L60)). The
`.salt` name becomes durable at the first admission, through the fresh ancestor syncs. A crash
before any admission can lose the salt, which changes history bindings and refuses replay; it does
not affect spend, holds or caps. This packet does not propose a change for it.

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

Option B is a bounded change: the witness can live in the existing snapshot, so no new file and no
extra sync are needed, but `append_private` is a public function used by the ledger and by tests, so
its signature or its caller changes. Option C removes the per-request snapshot rewrite, which is the
part that scales with the number of open holds, but it replaces the recovery contract that the
current fail-closed rules and their tests encode.

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
`fsync,fdatasync,ftruncate` and can inject `fsync:error=EIO:when=1`. The ordering test at line 551
already asserts that the directory `fsync` sits between the row's `fdatasync` and the hold removal.

The implementation PR adds, without touching production code:

1. **Crash points.** Model a crash at a sync boundary with strace
   `-e inject=fsync:signal=SIGKILL:when=N` or `-e inject=fdatasync:signal=SIGKILL:when=N`: the
   syscall never runs and the process dies, so the on-disk state is exactly "crash at that
   boundary". A crash anywhere between two syncs is modelled by killing at the later one, because
   no other durable state changes in between. The sync-error window uses the same injection with
   `error=EIO` instead of `signal=`, which is what the existing test already does. A test-only exit
   inside the write callback covers the boundaries that are not syscalls (mid-row, mid-snapshot).
2. **Windows.** One case per row of the table: W1 = C1, W2 = C2, W3 = C3, W4 = C4 (row and
   snapshot), W5 = C5, W6 = C6, W7 = C7, W8 = C8 at each of the four sync points, W9 = C9 with the
   crash injected in one of eight concurrent workers.
3. **Assertions after each window**, in a fresh process:
   - no unlogged spend: for every request a worker reported as forwarded, either a complete row is
     in the ledger or its hold id is still in the snapshot; concretely, the sum of complete row
     costs plus `reserved()` of the snapshot is at least the sum of the reservations the workers
     reported;
   - no lost hold: every hold id in the pre-crash snapshot is either still present or absent
     together with a matching durable row;
   - no double settlement: a second `finish` for the same id refuses, and replaying the same row
     twice does not double-count, which the existing reader test already covers
     ([accounting.rs:321](../../oxc-gateway/tests/accounting.rs#L321));
   - no overspend: after recovery, the existing eight-worker cap test
     ([process_budget.rs:277](../../oxc-gateway/tests/process_budget.rs#L277)) admits no more
     requests than the cap allows.
4. **Preserved tests.** Every existing test in `tests/accounting.rs`, `tests/process_budget.rs` and
   the library suites stays, unweakened. The ordering assertion stays meaningful: under B it becomes
   "the witness is durable before the hold removal", under C "the journal record is durable before
   the hold removal".
5. **Environment.** Isolated HOME/XDG, synthetic loopback mock upstream, no real keys and no
   provider traffic, CI only, `CARGO_BUILD_JOBS=3`.

## 8. Measurement plan

- **Filesystem.** A disk-backed filesystem on the CI runner, not tmpfs; record the filesystem type
  and mount options in the receipt. The owner's filesystem is unknown to this packet, and a hardware
  measurement needs the owner's explicit handoff.
- **Sync counts.** `strace -f -qq -yy -e trace=fsync,fdatasync,ftruncate` around one worker; report
  counts per phase and per request, first request of the day versus steady state. This is a count,
  not a timing.
- **Latency.** The existing loopback mock upstream with a fixed synthetic response; measure
  admission to Done wall time for one and for eight concurrent requests, at least 200 samples,
  reporting median, p95 and p99 together with the sync count of the same run. Report the directory
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

- Source-only verification at `69a81515ad3ce39dbf048112fd0cfad476f4b73f`; no build, benchmark,
  provider call or hardware run.
- The 5–30 ms estimate is unmeasured and is not repeated here as a claim.
- CI filesystems may not represent the owner's machine; strace changes timing, so counts are
  reliable and latencies are indicative.
- The elision's benefit is bounded by one directory `fsync` per request. Whether that is worth a new
  durability witness is the decision in section 6, and it is the orchestrator's to make.

## 10. Non-goals

No `fsync` or `fdatasync` removal, no settlement change, no journal, no change to caps, ledger rows,
account isolation or redaction, and no change under `oxc/`.
