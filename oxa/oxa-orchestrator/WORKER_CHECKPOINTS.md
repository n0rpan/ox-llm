# Opt-in retained A2A worker results

The `core` A2A scheduler's `A2ARunSettings.persist_results` is explicit consent
to retain raw task/message results in its new private run directory. Set it to
false for memory-only results. The scheduler adds no provider configuration or
route credential fields. Result text is untrusted and may still be sensitive;
do not log or forward it as audit metadata. No CLI or default setting changes.

After joining a worker, the scheduler may publish `worker-<id>.json`: a version-1
envelope with run ID and `A2AWorkerReport`. Serialization streams into a new
mode-0600 hidden pending file through a pinned private directory descriptor.
It is limited to 64 MiB including JSON/control escaping and the final newline.
Control characters, including C1 controls, remain escaped JSON data. The file is
synced, then renamed with `RENAME_NOREPLACE`, followed by directory sync. The
journal's finished record stores the SHA-256 of the exact published bytes.

An existing final/pending file, symlink, unsafe directory, serialization failure
or storage failure returns an error. Cleanup only removes the owned pending
name; existing destinations remain intact. A directory-sync failure can leave a
complete published checkpoint without a finished journal record. Never delete
that data or repeat inference merely because publication returned an error.
Pending cleanup on ordinary error is best effort; crash recovery is separate.

Checkpoint retention is not proof of remote process cleanup or billed usage:
read the report's stop confirmation and observed accounting. It adds no replay,
resume, source apply, publication, auto-delete, or authority to signal recorded
PIDs. Passive journal/checkpoint verification is described in
[A2A_INSPECTION.md](A2A_INSPECTION.md); original-agent reconciliation remains
a later slice.

Tests cover actual private file publication, no replacement, control escaping,
serialization/bound failures, pending/final symlinks, an owned child with real
short-write/EFBIG failure, explicit consent, report roundtrip and exact-byte
journal digest, and obstructed publication without a second initial send.
