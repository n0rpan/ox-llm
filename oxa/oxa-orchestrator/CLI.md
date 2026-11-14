# Opt-in orchestration command

Build the feature explicitly from `oxa/`:

```sh
cargo build --locked -p oxa-cli --features orchestrator --bin oxa
./target/debug/oxa orchestrate --help
```

Default builds have no new command entry point or dependency on the
orchestrator. Base `oxa --help` stays unchanged even in the feature build.
No listener starts until this command is invoked with a valid plan.

Use an explicitly verified local inference endpoint and exact model ID:

```sh
./target/debug/oxa orchestrate \
  --engine 127.0.0.1:PORT --model MODEL --local-only \
  --directory ./new-orchestration-run \
  --role planner --role reviewer \
  -- "Review the proposed design and identify missing tests"
```

Replace `PORT` and `MODEL` with the operator-verified endpoint and model.
The command has no default engine address. `--local-only` asserts that the
endpoint cannot forward to cloud inference; a loopback address alone does
not prove that. Production/cloud routes and paid calls remain unsupported.

Workers receive explicit role/goal data through A2A and run oxa RPC sessions
in private bubblewrap sandboxes. They are always offline, with tools disabled by default, and no
extensions, MCP, skills, project context discovery or saved sessions. The
selected permission mode is passed through and verified, with `plan` as the
command default. Permission mode alone does not expose files or tools.
Results are proposals; this profile does not edit or merge a checkout.

To delegate selected file contents, add `--source ./checkout --file src/main.rs`.
Repeat `--file` for each explicitly approved UTF-8 file. They are copied into
separate read-only worker workspaces; only the `read` tool is enabled. Add both
`--edit --permission-mode workspace` to enable `read`, `edit` and `write` in each
private copy. `--allow-new relative/path` registers a new path for review and
requires `--edit`. `--git-worktrees` additionally selects an owned snapshot Git
worktree per editing worker; see [GIT_WORKTREES.md](GIT_WORKTREES.md).
Selected file contents are delegation input: approve them for
disclosure and do not select secrets. There is no recursive source discovery.

The final Task includes structured `oxa.workspace.review.v1` Data artifacts for
changed files, with hashes, before/after content and display diffs. Source files
are never written. Multiple workers may propose overlapping changes; no merge or
apply occurs. File capture errors happen before run state or inference starts.
See [WORKSPACES.md](WORKSPACES.md) for exact bounds, validation and limitations.
Use `oxa orchestrate-review` to inspect a saved report, explicitly select completed
proposals and stage an exact approved candidate; see [REVIEW.md](REVIEW.md).

Worker status appears on stderr as state changes. Status writes use a separate
nonblocking file description with at most 512 pending bytes. Backpressure drops
intermediate updates instead of blocking deadlines or canceling useful work.
Caller descriptor flags remain unchanged; regular-file status logs append, and
unsupported stderr sockets/devices are quiet. The final report is authoritative. Stdout contains one final
JSON report, including each outcome, task/result, usage and stop confirmation.
JSON escaping keeps terminal control characters in result text from executing.
Redirect stdout explicitly if results should be saved. Audit logs contain
hashes and metadata, rather than raw goals, results or transport capabilities.

The output directory must be new, with an existing parent. It is private and
contains `run/transcript.jsonl` plus per-task logs under `agents/`. Approved
private workspace copies also persist under those per-task directories. Existing
directories are refused without overwriting their files. The ephemeral server
capability never goes into these logs or into worker prompts.

Ctrl-C or SIGTERM cancels the plan, cleans up the owned server/workers and
returns a partial JSON report when output remains available. Interrupted runs
exit 130 for SIGINT or 143 for SIGTERM. Full success exits 0; runtime/partial
failure exits 1; invalid arguments exit 2. A second interrupt can stop output
after worker cleanup. Signal handlers are restored when the command returns.
Use `oxa orchestrate-inspect --journal PATH` to inspect a completed or interrupted
journal without any network access or task replay; see [RECOVERY.md](RECOVERY.md).

Worker counts and parallelism are bounded to 16. Defaults use three roles and
at most four simultaneous workers. `--worker-timeout` and `--timeout` bound
active and whole-run time. `--response-bytes` (1024..16777216, default 1048576)
counts incoming wire bytes, including discovery, polling, headers and cleanup.
`--reported-tokens` bounds observed usage; it cannot preempt unreported usage
or establish a monetary ceiling. A run stops at whichever cap is reached first.
Native worker pipe output is separately bounded to 1 MiB.

## Validation and integration surface

Default hosted workspace tests exercise command parsing and the full command
implementation through the real-worker fixture, with owned loopback fake
engines. Local validation also builds the actual feature-enabled `oxa` binary
and runs the same success, help/validation, approved-file proposal and SIGINT scenarios through its
four-line main hook. The smoke suite also exercises crash inspection, Git
worktrees, stalled stderr, structural limits and unsafe output parents. To reproduce that smoke check from `oxa/`:

```sh
cargo build --locked -p oxa-cli --features orchestrator --bin oxa
OXA_ORCHESTRATE_TEST_BINARY="$PWD/target/debug/oxa" \
  cargo test --locked -p oxa-orchestrator --test real_workers -- --cli-smoke
```

The binary override is for this checkout's test build, not an installed runtime.
All inference in these tests is synthetic. No production model, oxc service,
cloud credential or other machine is used.

Existing-file hooks are limited to the optional feature/dependency in
`oxa-cli/Cargo.toml`, its lock entry, the gated main delegation and explicit
network-surface review entries with matching checker fixtures. The checker
still inspects optional dependency paths conservatively and rejects new
connect sites outside the reviewed files. No Actions workflow/settings change.
Manifest ownership and the new connect-site review entries require coordinator
review before any merge. This command remains part of an unmerged draft lane.

## Saved final report

After owned servers and workers stop, the command saves the same terminal-safe
JSON it prints to stdout as `<directory>/report.json`. This includes partial and
canceled run reports. A consumer closing stdout can cause exit 1 while the saved
report still records completed workers; a nonzero command exit alone does not
justify repeating inference. SIGINT/SIGTERM exit codes remain 130/143.

The file is 0600 inside the new private run directory. The controller serializes
directly to `.report.json.pending`, caps output at 272 MiB, syncs the file, publishes
with a no-replace rename using a pinned directory descriptor, then syncs the
parent. It refuses an existing destination or temporary entry. Ordinary write or
serialization failure removes its own pending file, returns failure, and never
restarts a task. A parent-sync failure can leave a complete published report.
A controller killed before publication can leave only the pending file; that is
not a completed checkpoint and is never consumed automatically.

Reports contain delegated results and approved source/proposal text, unlike the
hash-only audit journal. Keep the directory private. The report is not a signed
receipt or a persistent scheduler: it does not checkpoint active streams, restore
lost tasks, or authorize replay. `orchestrate-inspect` remains read-only and network-free; with an explicit
worker/checkpoint pair it can recover a journal-bound worker result even if no
final report was published. See [RECOVERY.md](RECOVERY.md). `orchestrate-review --report <directory>/report.json` accepts saved
reports within its separate 16 MiB review-input bound; larger diagnostic reports
are retained but cannot be passed directly to that review command.
