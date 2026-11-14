# Controller-owned snapshot Git worktrees

`oxa orchestrate` can place each editing worker in a real linked Git worktree:

```sh
./target/debug/oxa orchestrate \
  --engine 127.0.0.1:PORT --model MODEL --local-only \
  --directory ./new-run --source ./checkout --file src/main.rs \
  --edit --permission-mode workspace --git-worktrees \
  --role implementer --role reviewer -- "Propose changes for review"
```

Replace the verified endpoint/model placeholders. File contents must be approved
for delegation as described in [WORKSPACES.md](WORKSPACES.md). Git worktrees require
both editing authorization and workspace mode; the flag alone cannot widen a
grant. The default remains private copies, with no Git process. Read-only grants
stay read-only copies. The library equivalent is
`Grant::editing(snapshot, new_paths)?.with_git_worktrees()?`.

## Source and metadata boundaries

The controller creates a new private bare repository containing ONLY approved
snapshot bytes. It never opens the source repository, reads its Git config,
imports its history/remotes/objects, creates a source branch or changes the source
index. An explicitly selected source directory need not be a Git repository.
This is an approved snapshot's Git history, not a worktree linked to the original
checkout. It preserves the original disclosure boundary instead of making an
entire source repository implicitly available to another agent.

Snapshots use SHA-256 Git objects and one deterministic synthetic base commit,
with the brief's Oscar commit identity and a fixed timestamp. Each editing worker
gets a detached linked worktree at that base. A native multi-worker run shares a
repository for equal snapshot manifests; the A2A RPC backend shares the pool
across its tasks too. Distinct manifests get separate repositories. There are no
worker branches to push, no remotes and no automatic commits of worker output.

The raw approved text is inserted with `hash-object --no-filters` and a bounded
NUL-delimited index manifest. Worktrees use `--no-checkout`; `read-tree` initializes
only the index, and Rust writes the approved files itself. Source attributes,
ignore rules, checkout filters and working-tree encoding do not transform input
or execute a source-provided command. Review diffs/hashes are still produced by
the existing Rust workspace collector, without invoking Git diff/text conversion.

Only the worktree file directory is mounted into the worker. Git's controller
repository/admin directory is outside that mount. The root `.git` pointer is
masked with a read-only empty file in bubblewrap; reading it cannot reveal the
controller path or object store, and writing/unlinking/replacing it fails at the
mount boundary. Worker tools stay limited to `read`, `edit`, `write`; this flag
does not enable Bash, Git tools, extensions, MCP or permission escalation. The
existing offline and parent-mode checks remain in force.

After stopping and reaping the worker, the controller verifies that the original
host `.git` pointer is unchanged. Only that verified root metadata entry is omitted
from file collection; nested/proposed Git metadata remains forbidden. All other
scope, symlink, hard-link, special-file, executable and review-size checks remain.
The journal records snapshot manifest and base commit hashes, not original source
paths, raw file contents or Git diagnostics.

## Process and storage lifecycle

Git is the trusted installed `/usr/bin/git`, required only for this explicit
backend. It is checked before run/listener setup; there is no fallback to an
uncontrolled executable or unsandboxed worker. Commands run with a cleared
environment, private HOME, no system/global config, empty init template, disabled
hooks/fsmonitor/signing/automatic maintenance and forbidden transport protocols.
No fetch, pull, push, submodule, checkout hook, external diff or shell command is
used. Ambient Git directories/object stores/config injection are not inherited.

The controller services Git stdin/stdout/stderr nonblockingly, without reader
threads. Output/diagnostics are bounded to 64 KiB combined. Each command is capped
at two seconds and the remaining worker/run deadline; cancellation flags are
checked during command I/O and while waiting for a shared pool. Commands have an
owned process group, parent-death signal and kill/wait cleanup on ordinary failure,
timeout or cancellation. Failed/expired preparation yields an explicit worker
outcome and does not launch inference; other eligible workers remain reportable.
OS filesystem calls and scheduling can add overhead, as elsewhere in the profile.
Preparation runs in separate owned jobs counted against parallelism. The
supervisor continues servicing other workers, deadlines and per-worker controls
while Git or a shared-pool wait is in progress. Audit failure stops active workers,
cancels every preparation job and joins the owned threads.

Repositories and worktrees are retained in private run/agent directories for
inspection. Worktree directories are 0700, initial text and pointer/mask files
0600. Controller roots are private; internal Git object files remain under those
roots. Nothing in this module automatically removes a source checkout, prunes an
external repository, resumes an old run or reuses an existing worktree destination.
A failed preparation can leave a partial owned repository/worktree for diagnosis;
it is not cached as a successfully prepared repository. After all owned tasks
have stopped and results are saved, the operator can delete the entire private
run/agent directory, including its pool, as one retention unit.

The ordinary [review flow](REVIEW.md) selects proposals and stages an explicitly
approved candidate. Applying to an existing checkout, validation with additional
tool grants, conflict resolution, committing reviewed worker output and publishing
or merging still require further work and the brief's review gates. This flag
neither authorizes a paid/cloud call nor changes those gates.

## Validation

Owned loopback tests cover linked-worktree registration/shared base, raw CRLF text
and hostile attributes, separate worker edits, original-source preservation,
metadata masking/write refusal, pointer tampering, cancellation/pool deadlines,
CLI scope requirements, poisoned ambient Git config/directory/object-store values,
and A2A proposal collection. The feature-built binary smoke check runs the same
CLI worktree scenario. No installed model or production service is contacted.
