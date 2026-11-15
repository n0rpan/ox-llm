# Passive A2A journal and checkpoint inspection

`a2a_recovery` is a separate default-off `core` API for the scheduler's A2A
transport. It does not broaden the native inspector: native inspection still
rejects remote journals. Neither inspector sends requests, starts workers,
replays input, resumes a goal, deletes data, applies edits or signals a PID.
Original-agent reconciliation remains a later slice.

`InspectedRun::inspect` reads a pinned, operator-owned private single-link plain
file below a protected private parent, with stable metadata checked before and
after capture. It reuses the native reader's checks: no symlink, special file,
unsafe parent, cross-device input or changed file is admitted. Bounds remain
16 MiB/journal, 64 KiB/record and 100,000 complete records. A bounded incomplete
last line is fingerprinted but never interpreted as an event.

A2A validation requires a complete queued manifest before launch, fixed task
and context bindings, valid hashes/response reservations, ordered lifecycle and
matching terminal counts. Native process events and unimplemented resume
bindings are rejected. Completed outcomes require consistent terminal evidence;
local run_finished can still coexist with explicit unconfirmed remote cleanup.
The snapshot preserves that uncertainty. Missing legacy ownership is inspectable
but cannot establish an exclusive recovery claim.

`ExclusiveRun::claim` owns the cooperative journal-inode lease until drop and
refuses a live writer or another claimant. It grants no process authority, and
it does not extend across replacement of the operator-owned path. Same-UID/root
namespace actors remain within the trusted operator boundary.

`load_checkpoint` requires explicit recorded retention consent and a finished
worker record binding the exact file SHA-256. It reads at most 64 MiB through
the same private pinned-file guards, then checks envelope version/run/worker,
outcome/stop, task identity/state, collected wire data and result digest. Pending,
orphan, altered, wrong-worker or mismatched results confer no recovery authority.
Raw retained text remains result data, not something to log as audit metadata.

Local acceptance: 99 ordinary crate tests pass. Existing A2A scheduler fixtures
now inspect their actual journals and exercise exclusive claim/release. Tests
include checkpoint roundtrip/tamper and absent-consent refusal, live-writer and
legacy exclusion, partial tails, malformed lifecycle/hash/stop evidence, and the
100,000-record boundary. Existing native private-file/lease tests remain active.
No provider, shared gateway, paid request or UI change is involved.
