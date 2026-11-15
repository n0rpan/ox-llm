# Exact-content candidate staging

With default-off `core`, `review::PreparedReview::prepare` accepts an immutable
approved snapshot, explicit eligible new files and 1..16 selected proposals.
It revalidates every content/scope hash and diff, sorts identities/paths for a
deterministic preview, and detects overlapping paths. Even identical overlapping
edits require a different explicit selection; they are never silently merged.
The combined candidate stays within snapshot file/byte/depth/entry limits.

The preview digest binds source and candidate manifests, new-file scope, full
selected review data and controller/run/task/context attribution. The prepared
candidate is private immutable state; editing a cloned preview cannot replace
it. `stage` requires the exact prepared digest. That digest is a content binding,
not authentication or proof that a human approved: the caller owns its trusted
permission/approval flow and must not treat a model-supplied digest as consent.

Staging writes only to a NEW candidate directory. It creates an owned random
mode-0700 staging root through a protected parent inode, writes mode-0600 files
and a receipt, syncs files/directories, and publishes with a no-replace rename.
Existing empty/nonempty directories and symlinks remain intact. The receipt
records approved manifests and selected review digests/identities; it adds no
Git repository, hook, subprocess, source apply, merge, agent call or listener.

Unpublished staging cleanup is best effort and owns only the freshly created
random directory. Partial data is never renamed into the candidate destination.
A parent-sync failure can leave a complete published candidate; publication does
not roll back or delete that approved result. The operation is synchronous and
claims no hard elapsed-time guarantee for filesystem I/O.

Saved-report extraction is bounded to 16 MiB and refuses links/special or changed
files, duplicate IDs, absent selections, reported incomplete/unknown-stop tasks
and multiple/missing review artifacts. These completion/identity fields remain
untrusted report data, not an independent process-reap or authentication proof.
`prepare` must still verify content and scope against the caller's snapshot.
Raw preview/proposal data may be sensitive and must not enter metadata logs.

Staging never modifies the source. It creates content against the immutable
approved base and does not re-read a changing source checkout. A later apply or
publication operation must separately check source freshness and authorization.
Private snapshot worktrees are described in
[PRIVATE_GIT_WORKTREES.md](PRIVATE_GIT_WORKTREES.md). Repository publication and
CLI/TUI hooks remain subsequent slices.

Local acceptance: 126 ordinary crate tests pass, including eight review tests.
They cover deterministic disjoint selection, exact approval, overlap refusal,
forged/stale-base reviews, saved-report selection and provenance fields, unsafe
namespaces, preserving an existing empty destination, an oversized combined
candidate from independently valid reviews, and a real owned-child short-write
failure with no candidate published. Original source inputs remain unchanged.
No provider, shared gateway, paid request or UI is involved.
