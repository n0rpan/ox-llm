# Read-only fan-in across a selected run chain

With default-off `core`, `lineage::collect` accepts 1..16 exclusively claimed
A2A journals in explicit root-to-leaf order. It discovers no paths and contacts
no agent. The caller keeps every claim alive. It does not launch, replay,
mutate, delete, apply source changes or signal processes. Historical result
recovery does not require the original inference deadline to remain live.

Every selected manifest must be complete and unique. Each parent must have an
untorn unfinished journal and a private bounded `resume.json` binding its exact
journal digest, root run ID, deterministic child ID and approved destination.
The child must contain exactly the parent's queued IDs with unchanged worker
bindings and inherited run policy/deadline; only the queued response allocations
carry forward. Ambiguous parent lifecycles, changed receipts, missing or skipped
links and repeated journals fail closed.

Child destination verification uses the kernel path of its captured directory
inode. A copied journal/checkpoint in a different directory does not replace the
explicitly approved child. Receipt creation now normalizes the spelling of its
validated parent inode, so harmless `.` components cannot cause a mismatch.
Renaming an approved child changes that path binding and is refused; the API
does not search for an alternative child or repair receipts.

Worker results load through captured private directory handles and verified
checkpoint hashes; every selection still has to match its receipt path binding.
Every worker from the selected root must have exactly one retained report;
unresolved, duplicate or unrelated outcomes cannot silently disappear. A torn
final whole-run marker can be ignored only when complete worker finishes and
checkpoints already account for every result. Original failure/stop uncertainty
remains in the returned report and cannot be upgraded by a successful leaf.
Selecting a child as the root recovers that child's scope, not unseen ancestors.

Raw results remain untrusted data. A protocol-level stop assertion is not an
independent proof of provider billing or local process reap. Same-UID/root
namespace actors remain within the operator trust boundary. No CLI/TUI, Git
publication, registration loading, production service or paid route is added.

Local acceptance: 118 ordinary crate tests pass. A real three-generation mock
run preserves ancestor results and a failed leaf, rejects incomplete/reordered/
repeated links and altered receipts, rejects a copied journal plus valid copied
checkpoint, and handles a torn final marker without resending. The `./child`
regression failed before the destination normalization and passes afterward.
All agents are authenticated owned loopback fixtures; counters stay at one send
per worker throughout lineage collection. No real provider or gateway is used.
