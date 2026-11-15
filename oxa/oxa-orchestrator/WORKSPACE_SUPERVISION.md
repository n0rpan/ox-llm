# Owned private-copy worker lanes

Preparation slice after `a2a/core-workspace-mounts`, for #88 and #282. This is
library behavior behind the existing default-off `core` feature. It adds no CLI,
TUI, listener, model route, automatic source apply, or publication operation.
The existing `run` entrypoint retains its tool-free profile.

`run_with_workspaces` accepts the same explicit worker plan plus a map from
worker ID to `WorkspaceAssignment`. Only the operator constructs that map:
each entry contains an immutable `Arc<Grant>` and a new destination under an
existing private parent. Unknown worker IDs fail before journal creation.
Failed preparation retains any partial copy; an existing destination is never
replaced. Caller approval of source contents remains necessary.

Each lane runs these stages:

1. Prepare its private copy on an owned filesystem thread. This occupies one
   parallelism slot; the supervisor continues polling other processes.
2. Launch the worker with that exact copy mounted at `/work`, read-only for a
   read-only grant. The worker's mode/tool policy follows the immutable grant.
   Its RPC session verifies the corresponding mode and rejects ungranted tools.
3. Stop and reap the process before starting collection on an owned thread.
   Collection retains the lane slot, validates outputs against the grant, and
   returns a bounded `ReviewBundle` in `WorkerReport.review`. Output outside the
   grant produces `policy_violation` and no review. Partial/truncated results
   retain their original unsuccessful outcome even when review collection works.
4. Join filesystem work before publishing the terminal report. The report and
   `RunReport::succeeded` cannot indicate success while collection is pending.

The worker's time allowance starts at preparation admission and includes
collection. Run cancellation, targeted cancellation and deadlines withdraw
unused filesystem results. Cancellation/deadline skips collection of a stopped
process and never authorizes source edits. Observed process bytes/tokens remain
in aggregate accounting while collection is pending. Reviews contain raw
approved file text and must be handled as local result data, not audit metadata.

Cancellation of filesystem work is cooperative. An in-flight OS operation may
delay joining; this API makes no hard elapsed-time guarantee for kernel I/O.
An unconfirmed process cleanup is reported as lost and never grants collection
authority. The supervisor cannot safely override unrelated same-UID/root actors
mutating the operator-owned namespace. Private copies are retained; deletion is
an explicit operator operation outside this API.

Workspace runs use journal profile `workspace-rpc-v1`; inspection requires the
same explicit cleanup evidence as `tool-free-rpc-v2`. Journal metadata contains
hashes, counts and policy, never source paths, file contents or result text.
Inspection still grants no recorded-PID signaling or replay authority.

Local acceptance: 80 ordinary crate tests pass, including actual bwrap mounts,
shutdown-time writes included in the review only after reap, bounded lane
admission, read-only writes refused by the mount, out-of-scope output rejected,
preparation/collection cancellation and deadlines, targeted cancellation,
existing-destination preservation, and a real writing worker with confirmed
cleanup and no delayed writes. Strict all-target Clippy and formatting pass.
Fixtures use synthetic data, cleared environments, and owned local processes;
there are no paid/provider calls or TUI behavior claims.
