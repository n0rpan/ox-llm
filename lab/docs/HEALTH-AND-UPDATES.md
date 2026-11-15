# Health digest, update, rollback and failure drills (v0.1.4)

Three things that let the local AI stack run without being watched: a daily health digest made of numbers
only, a one-step update and rollback from a release that is already staged on disk, and four failure drills.

## Daily health digest

```
local-ai digest [--day YYYY-MM-DD|today|yesterday] [--json]   one day (default: today so far)
local-ai digest --write [--day D]                              save the day (default: yesterday) + status line
local-ai digest --status [--json]                              the one line the panel shows
local-ai digest --soak [--json]                                soak progress
```

The digest reads the request ledger (`~/.local/state/local-ai/ledger/requests-YYYYMMDD.jsonl`) and the
`engine_died` events in `~/.local/state/local-ai/engine-stderr.log`. It skips every row whose origin is
`lab` or `gate`, so gates, drills and the update's test chat never count as use. LAN-share rows (`remote`)
are real use and are counted, and listed separately under `by_origin`. The digest holds counts, rates,
percentiles and fixed labels. It never holds prompt text, request bodies, session ids, keyed hashes or
paths.

Files are owner-only (directory 0700, files 0600) under `$XDG_STATE_HOME/local-ai/digest/`, or under
`LOCAL_AI_DIGEST_DIR` when that is set:

- `digest-YYYYMMDD.json`: one day, schema `local-ai-digest/1`;
- `status.json`: the panel's status line, schema `local-ai-digest-status/1`.

`packaging/systemd/local-ai-digest.{service,timer}` runs `local-ai digest --write` every day at 00:20
(Persistent=true, so a missed run is caught up after login). Install the two files into
`~/.config/systemd/user/`, then run `systemctl --user daemon-reload && systemctl --user enable --now local-ai-digest.timer`.

### What it measures

- **Requests:** status classes, retries (`miss_class=retry`) and duplicate bodies (`history_change=same`).
- **Cache:** token reuse rate. Eligible follow-up turns are appends whose previous reply came back unchanged; for those it reports prefix hits, C3 misses, unexplained (`unknown`) misses and the miss classes.
- **History (client-neutral):** `history_rewrites` (a history that is not an append of the session's previous one: ledger `history_rewrite`, or `history_change` edited/truncated, so rows written without the core diagnostics count too), `fold_suspects` (the core's structural `fold_suspect` label; counted, never an alert: it is the known issue for clients that fold the summary into the prompt), `rewrite_classes` (how the previous reply came back) and `client_compactions` (only the client's own H5 `X-Oxe-Turn-Kind: compaction` label, never guessed) with their wall-time median. The digest knows no client's wording and no client-specific swallow label.
- **Queue and replay:** requests that waited for the slot, their wait p50/p95 (`queue_wait_ms`), session switches, and duplicate-request replays by label (`cut-served`, `cut-done`, `cut-aborted`). A queue-full 503 is counted under HTTP 5xx.
- **Memory guard:** guard stops grouped as `psi`, `memory_floor`, `oom`, `swap`, `pids` and `other`, plus all engine deaths.
- **Latency:** TTFT p50/p95 over all turns and over cache-hit turns, and restore time, as day totals for information.
- **Per model (`series`):** one series per model alias, plus the context mode (`fast`, `deep` or `max`) when the row has one, for example `qwen3.6-35b-a3b-local/fast`. Each series has TTFT p50/p95 on cache-hit turns, append-prefill and decode tok/s as p50 per depth band (0-8k, 8-32k, 32-64k, 64-96k, 96k+), and the median wall time of H5 compactions. Every trend alert compares a series only with the same series on earlier days, so switching models or modes never looks like an engine regression.
- **Errors:** ledger error labels, HTTP 5xx, and streams cut before the end. A client that cancels is not counted as a cut stream.
- **Store:** used share of the cache budget, when the controller answers.

### Alerts (OEM-DONE §2(d); estimates, recalibrate after 7 days)

| Alert id | Fires when |
|---|---|
| `client_retries`, `duplicate_prefill`, `http_5xx`, `stream_cut`, `engine_death`, `guard_trip`, `scope_oom`, `swap` | any |
| `unknown_cache_misses` | unknown misses > 1% of eligible turns |
| `prefix_hits_low` | prefix hits < 90% of eligible turns |
| `restore_slow` | restore time p95 > 5 s |
| `decode_slower:<series>:<band>`, `prefill_slower:<series>:<band>` | p50 more than 10% below the same series' trailing 7-day median (needs >= 5 samples and >= 3 earlier days) |
| `ttft_hit_slower:<series>`, `compaction_slower:<series>` | more than 1.5x the same series' trailing median (needs >= 3 earlier days); `compaction_slower` uses H5-labelled compactions only |
| `store_full` | store > 90% of budget |

A day is green when it has no alert. The soak passes with 7 consecutive green days, >= 300 organic requests
and >= 10 history rewrites over those days (the client-neutral stand-in for "compactions"). Idle days, with no organic request, are skipped: they neither count
nor break the run. Three green days is the ship bar (`ship_ready`).

### Status line contract (`status.json`, for the panel)

```json
{"schema": "local-ai-digest-status/1", "day": "2026-11-12", "state": "healthy",
 "line": "Healthy", "alerts": [], "alert_text": [], "requests": 123, "generated_at": 1763000000,
 "soak": {"green_days": 4, "ship_ready": true, "pass": false, "requests": 410, "history_rewrites": 6}}
```

- `state` is one of `healthy`, `attention`, `no_data` and `stale`. `stale` means more than 36 h since `generated_at`; check it when reading the file directly. `local-ai digest --status --json` applies the check for you.
- `line` is ready to show: "Healthy", "1 thing to look at", "N things to look at", "No use recorded yet" or "Health check has not run lately".
- `alert_text` lists short plain phrases in the same order as `alerts`.
- `soak` is absent until the first `--write`.

## Update and rollback

```
local-ai update [STAGED-RELEASE-DIR] [--no-chat]   install a staged release, check it, roll back by itself on failure
local-ai update --check [DIR]                      check only, change nothing
local-ai rollback [--check]                        put the previous release back
```

`local-ai update` never downloads anything. The release must already be staged on disk in the O-119 format:
`manifest.json`, `SEAL.json`, the staged files, `rollback/rollback.py` and `tools/install_staged_release.py`.
Some stages also carry a `tools/stage_*_final.py` generator; its `precheck` and `materialize` steps are then
used too. Without a directory, `update` picks the newest stage it can install under `LOCAL_AI_STAGE_ROOTS`
(default `~/.local/share/ai-stack/staged`). The very first time, when the installed CLI does not yet have
`update`, run the staged release's own CLI: `python3 <stage>/runtime-source/local-ai update <stage>`.

Steps:

1. **Check.** The seal, the live files (they must be exactly what the stage expects to replace), the stage's own `rollback.py --check`, and the generator's `precheck` or the installer's dry run. A refusal changes nothing (exit 2).
2. **Record.** The new release dirs are unpacked, and `service-state.json` records which services run and which release the chat controller runs.
3. **Stop.** The chat service and Studio are stopped, and `is-active` must show both down before any file is replaced. If that cannot be proven, nothing is installed, the services are started again as they were, and `update` exits 2.
4. **Install.** The installer replaces the files (atomic per file; it restores itself if it stops part-way).
5. **Start.** `daemon-reload`, then the chat service starts, plus Studio if it was running.
6. **Wait for readiness.** `/_local/status` must answer, and the controller's main process must be proven to run the staged release (its working directory is the manifest's `release_dir`). If its identity cannot be told, it is not ready. A stage without a `release_dir` (synthetic) needs only the status answer.
7. **Test chat.** One tiny chat, sent with `X-Local-AI-Origin: lab`. If no model was loaded before, it is unloaded afterwards.

If step 4, 5, 6 or 7 fails, the stage's `rollback.py` runs the same steps in reverse: it stops both services, puts the previous files back with their bytes and modes, runs `daemon-reload`, and starts the services that were running before. `update` then waits until the controller is proven to run the previous release (the recorded one, or at least not the staged one) and exits 1. If that proof does not come, it exits 3.

`local-ai rollback` runs the rollback of the release installed last (recorded in
`$XDG_STATE_HOME/local-ai/update/state.json`, or found by matching the live files), then waits for readiness.
The readiness wait needs the same identity proof. Previous release dirs are never deleted.

Exit codes: 0 done; 1 the update failed and the previous version is back; 2 refused, nothing changed;
3 needs attention, and the message says what to run.

## Install, rollback and smoke checks

The local staged-release lifecycle keeps its seal, preimage, service-identity and automatic
rollback checks. The helper exposes the same operations plus a bounded local-model smoke chat:

```sh
python3 tools/oem_drills.py install ~/project/stage
python3 tools/oem_drills.py rollback
python3 tools/oem_drills.py smoke
```

The stage must already exist and pass the installer's checks. The smoke request is marked as
lab usage, refuses remote models, and unloads a model only when it was not already loaded.
It performs no process killing, memory-pressure injection or cache corruption.

Model-free recovery and ownership regressions remain in `tests/test_controller_recovery.py`.
Run those and the smoke tests with the normal isolated lab test runner.

## Error messages

User-facing errors are short and end with what to do. Status codes, error `type`/`code`, exit codes and
ledger labels did not change in v0.1.4; only the wording did. A memory-guard stop keeps its stable reason
code in the controller (`last_failure.reason`, the ledger and the engine log), and `local-ai status` shows
it as a sentence, for example: "The model was stopped to keep the laptop responsive (memory was nearly full).
Close a few apps and try again."
