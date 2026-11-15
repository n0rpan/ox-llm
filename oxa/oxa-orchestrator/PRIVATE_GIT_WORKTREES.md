# Controller-owned snapshot worktrees

With default-off `core`, an editing grant may explicitly opt into
`Grant::with_git_worktrees`. Read-only grants cannot opt in. Create an explicit
`workspace_jobs::GitPool`, then use `Preparation::start_git` or provide that pool
in `supervisor::WorkspaceAssignment.git_pool`. Plain materialization refuses a
Git grant instead of silently choosing a different backend. No CLI/TUI hook or
normal default is changed.

A pool creates new private controller metadata and seeds a SHA-256 bare Git
repository solely from approved snapshot paths/bytes. It never opens the source
repository, config, remotes or object database. Equal manifests share one private
seed commit; each copy has its own detached worktree. The seed uses a fixed
synthetic identity/time. Hooks, filesystem monitoring, ambient global/system
config, automatic maintenance, signing, transports and attribute filters are
disabled. Objects are written from stdin without filters, and read-tree populates
only the index; approved files are materialized directly as private raw bytes.
No checkout/filter execution or source metadata is delegated.

Destinations require protected private operator-owned parents. Each pool admits
at most 128 preparation attempts, including failures; partial repos/worktrees
remain for explicit operator cleanup. The pool lock, helper calls and file writes
observe cooperative cancellation/deadlines while other worker lanes keep polling.
Preparation and collection still occupy lane slots. Git preparation does not
create a worker or authorize any source edit/publication.

The native worker receives the copy at `/work`, with its `.git` pointer masked
by an empty read-only file. Tools stay read/edit/write, never Git or Bash.
Controller metadata is outside that mount. After stop/reap, collection validates
the original private pointer before excluding it from the approved entry budget.
A changed, linked or special metadata file is refused. The informational
`WorkingCopy::base_commit` identifies this private seed, not a source commit or
permission to publish it.

Fixed trusted Git helpers use cleared environments, owned process groups,
parent-death signaling and nonblocking bounded I/O. Stdout/stderr share 64 KiB
plus one detection byte. WNOWAIT observes direct-child exit while retaining PID
identity until group cleanup and wait. No recorded PID is adopted or signaled.
Cancellation/error cleanup kills and waits; an in-flight kernel operation can
delay that wait, so no hard elapsed-time guarantee is claimed. These helpers do
not support independently detached child sessions or arbitrary caller commands.

Local acceptance: 134 ordinary crate tests pass. Actual Git tests verify shared
approved objects/raw CRLF bytes despite hostile attributes and an invalid source
Git config, detached copies, metadata tamper refusal, cancellation/deadline and
pool/entry bounds. A retained-pipe late-writer regression failed before the
WNOWAIT cleanup fix and passes afterward. Real bwrap supervisor lanes prove the
metadata mask is empty/read-only and collect edits only after shutdown. No
provider, shared gateway, paid request or production registry is involved.
