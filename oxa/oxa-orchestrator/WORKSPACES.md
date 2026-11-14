# Approved workspace proposals

The library can give a trusted local RPC worker a private copy of explicitly
selected UTF-8 text files. The source checkout is never mounted into the worker.
The library and default-off `oxa orchestrate` command support explicit grants.
Without `--source`/`--file`, the command still has no file inputs or tools. These
copies remain the default. Editing grants can explicitly use controller-owned
Git worktrees seeded only from approved files; see [GIT_WORKTREES.md](GIT_WORKTREES.md).
[REVIEW.md](REVIEW.md) describes proposal selection, conflicts and approved candidate
staging. Applying into an existing checkout and merge coordination remain future work.

The CLI requires a source root and repeated `--file` paths; see [CLI.md](CLI.md).
`--edit --permission-mode workspace` authorizes private edits, and repeated
`--allow-new` registers eligible new paths. Capture completes before output
directory creation, listener startup or any inference request.

The library operator calls `workspace::Snapshot::approve(root, paths)` and wraps it in
`Grant::read_only` or `Grant::editing`. Set `WorkerSpec.workspace` or
`RpcAgentConfig.workspace` to that immutable grant. Editing also requires the
parent's `PermissionMode::Workspace`; plan and ask cannot authorize it. A2A
messages and model output cannot replace the grant or select a source root.
Every delegated file's contents must be approved for disclosure to the worker
and its registered local inference route. Filename filtering does not prove
that arbitrary source text contains no secrets.

Only selected files are copied. There is no recursive source discovery, parent
context, Git directory, configuration discovery or environment inheritance.
Paths must be relative, bounded and free of traversal, control characters and
protected credential/configuration components. Descriptor-relative input reads
refuse symlinks, hard links, special files, filesystem crossings, non-UTF-8 data
and observed mutations during capture. Source capture is per file, not an atomic
multi-file repository snapshot; approve a quiescent source for consistent input.

Each worker gets its own copy under the new private run directory, bound at
`/work` in bubblewrap. Read-only grants mount it read-only and expose only `read`.
Editing grants expose `read`, `edit` and `write` against the private filesystem.
The existing oxa permission policy handles paths outside the working directory;
the supervisor denies permission dialogs. Bash, MCP, extensions, project context,
network tools and other file tools stay disabled. The trusted oxa executable and
normal namespace/offline restrictions remain required. This is not a sandbox for
arbitrary executables, or a kernel disk/memory quota.

After the worker has stopped and been reaped, collection rejects unapproved
files, symlinks, special files, executable output and changed read-only inputs.
Only existing selected paths and explicitly registered new-file paths can enter
a proposal. Empty directories have no review representation. A failed or canceled
worker may leave a partial proposal; its task outcome remains explicit.

Changed files produce an A2A Data artifact with `kind: oxa.workspace.review.v1`
and a `review` containing worker identity, snapshot manifest, relative paths,
before/after text, SHA-256 hashes and a deterministic unified diff for display.
The diff uses one hunk around the changed span with three context lines and a
linear-time algorithm. File existence is represented by nullable before/after
fields, including empty-file creation/deletion. The structured content is the
review authority; the display diff is not an apply command.

Call `ReviewBundle::verify` with the original approved snapshot and new-file
scope before presenting a proposal as valid. `verify_current_base` checks the
touched source paths for concurrent changes without writing anything. `overlaps`
reports paths changed by both proposals; it does not attempt a merge. There is no
apply operation, shell execution, automatic approval or merge in this module.
The separate review module can stage exactly approved contents in a new candidate
directory. It never overwrites the source. Revalidate again at any future apply
boundary: a prior check does not lock files.

Bounds: 32 total eligible files, 256 KiB per file, 1 MiB snapshot/collected text,
128 KiB serialized review, 128 total entries including parent directories, and
16 nested directory levels. Selected input layout and the complete eligible
new-file layout are validated against those same collection bounds before launch. Oversize
reviews yield an output-budget outcome; invalid filesystem output yields a policy
violation. A review cannot silently turn either outcome into success. These bounds
are separate from accepted RPC-wire output limits and reported-token limits.

Snapshots and generated files persist in private run directories (0700, initial
files 0600). They contain the explicitly delegated text and are not journal-only
metadata. The JSONL transcript records manifest hashes, tool names, argument hashes
and change counts, never raw file contents, tool arguments or diffs. Retention and
manual deletion are the operator's responsibility; no source file is overwritten.

Reproduce with the owned loopback fake engine, without installed models:

```sh
cargo test --locked -p oxa-orchestrator --lib workspace::tests
cargo test --locked -p oxa-orchestrator --test real_workers
```
