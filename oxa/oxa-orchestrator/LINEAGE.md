# Collect results across continuation runs

A resumed child owns a subset of the original manifest. Collecting only its local
report can leave earlier workers' results in ancestor checkpoints. The read-only
`lineage::collect` API combines explicitly selected, exclusively owned run journals
in root-to-leaf order and returns a full A2A report for the root manifest.

```sh
./target/debug/oxa orchestrate-inspect \
  --lineage ./original/transcript.jsonl \
  --lineage ./continuation/transcript.jsonl \
  --lineage ./later-continuation/transcript.jsonl > root-report.json
```

The library equivalent is `lineage::collect(&[&root_owner, &child_owner, &leaf_owner])`.
Each owner comes from `ExclusiveRun::claim`; keep all owners alive during collection.
The CLI claims them before reading results. A live writer or another recovery
owner makes the command fail without waiting. It neither writes nor removes files,
launches workers, reads capabilities, contacts agents, cancels tasks nor signals
PIDs. Source files and saved results remain unchanged. The command accepts 1..16
explicit paths and does not scan for or follow paths supplied by result data.

## A complete root report requires a verified chain

Every selected node needs a complete unique A2A manifest and the cooperative
journal ownership protocol. Each parent must have a complete, untorn journal
captured by its durable `resume.json` receipt. The receipt must bind that exact
parent hash and run ID to the next child's distinct run ID and actual pinned
physical destination directory. A copied journal at a different destination
cannot substitute for that child.

Receipt worker IDs must be unique and equal exactly the parent's never-started
queue and the child's full manifest. Child role/task/endpoint/worker-limit bindings
must match the original queued jobs. Parent limits, receipt limits and child
limits must agree, preserving boot-clock start/deadline, parallelism, timings,
retention choice and the queued subset's unused response reservation. The
collector refuses skipped/reversed nodes, repeated journals, changed receipts,
changed destinations, substituted manifests and altered allowances.

After verifying all edges, the collector loads only journal-bound worker checkpoint
files from those pinned run directories. It requires one durable report for every
root worker, rejects duplicate or unrelated reports, and retains each original
outcome, usage, task/context, artifacts and stop evidence. It does not replace a
failed leaf with an ancestor's successful worker or count an unresolved worker as
absent. An unfinished leaf worker means collection fails rather than emitting a
smaller report that could look successful. Individual durable results remain
available through the existing explicit `--worker`/`--checkpoint` inspection mode.

A complete worker report can survive a torn final `run_finished` marker. Full
root collection requires all worker finish bindings, not that final marker. A
returned report still describes recorded outcomes: failed, canceled and budget outcomes and unconfirmed stops retain their meaning, and `succeeded()` follows the existing
all-required report semantics. It is not a fresh probe or proof of arbitrary
remote process state. Expired deadlines or a later boot do not prevent reading
already-collected artifacts; this operation never spends or launches anything.

## Bounds, retention and review

Each journal remains bounded to 16 MiB, each receipt to 64 KiB and each worker
checkpoint to 64 MiB, using the existing pinned plain single-link readers and
mutation checks. Root manifests have at most 128 workers; a selected lineage has
at most 16 generations. Original manifests, receipts and result bytes are
consistency evidence, not signatures against a trusted same-UID operator/root
modifying them. Advisory inode locks require cooperating writers on the same
host/filesystem. No directory lineage or original endpoint can be reconstructed
from a hash; the operator supplies the intended paths.

Worker results must have been explicitly persisted originally via `--save-results`
or `persist_results`. Missing, changed, linked or unbound checkpoint files fail
collection. This operation does not change that consent, regenerate lost artifacts
or replay work. Its stdout contains delegated result/approved source text. The
operator chooses where to retain it and should keep that output private.

A collected report can enter `orchestrate-review --report root-report.json` with
the original selected source scope, worker selection and exact approval gates.
That reader retains its separate 16 MiB input bound. Collection alone authorizes
no source mutation, command execution, candidate staging, validation grant or merge.

Owned loopback tests build three generations with distinct original jobs, retain
results from both ancestors and a failed leaf, and verify each task is sent once.
They refuse unresolved leaves, skipped/reversed/repeated nodes, substituted
receipts and copied destinations, and preserve complete worker results with a torn
final run marker. The real RPC resume scenario also collects through the library
and actual CLI hook, compares the result with original full fan-in, checks ownership
exclusion and verifies no new inference or journal mutation.
