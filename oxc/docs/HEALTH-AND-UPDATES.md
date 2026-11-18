# Health digest, update, rollback and failure drills (v0.1.4)

Three things that let the local AI stack run without being watched: a daily health digest made of numbers
only, a one-step update and rollback from a release that is already staged on disk, and four failure drills.

## Daily health digest

```
oxc digest [--day YYYY-MM-DD|today|yesterday] [--json]   one day (default: today so far)
oxc digest --write [--day D]                              save the day (default: yesterday) + status line
oxc digest --status [--json]                              the one line the panel shows
oxc digest --soak [--json]                                soak progress
```

The digest reads the request ledger (`~/.local/state/oxc/ledger/requests-YYYYMMDD.jsonl`) and the
`engine_died` events in `~/.local/state/oxc/engine-stderr.log`. It skips every row whose origin is
`lab` or `gate`, so gates, drills and the update's test chat never count as use. LAN-share rows (`remote`)
are real use and are counted, and listed separately under `by_origin`. The digest holds counts, rates,
percentiles and fixed labels. It never holds prompt text, request bodies, session ids, keyed hashes or
paths.

Files are owner-only (directory 0700, files 0600) under `$XDG_STATE_HOME/oxc/digest/`, or under
`OXC_DIGEST_DIR` when that is set:

- `digest-YYYYMMDD.json`: one day, schema `local-ai-digest/1`;
- `status.json`: the panel's status line, schema `local-ai-digest-status/1`.

`packaging/systemd/oxc-digest.{service,timer}` runs `oxc digest --write` every day at 00:20
(Persistent=true, so a missed run is caught up after login). Install the two files into
`~/.config/systemd/user/`, then run `systemctl --user daemon-reload && systemctl --user enable --now oxc-digest.timer`.

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

- `state` is one of `healthy`, `attention`, `no_data` and `stale`. `stale` means more than 36 h since `generated_at`; check it when reading the file directly. `oxc digest --status --json` applies the check for you.
- `line` is ready to show: "Healthy", "1 thing to look at", "N things to look at", "No use recorded yet" or "Health check has not run lately".
- `alert_text` lists short plain phrases in the same order as `alerts`.
- `soak` is absent until the first `--write`.

## Update and rollback

For v0.2.x upgrades, follow the
[native install and upgrade guide](https://github.com/oxstack/ox-llm/blob/main/docs/NATIVE-INSTALL.md).
It covers package verification, moving from the legacy controller, and using
the native binaries.

The [archived two-service update and rollback procedure](https://github.com/oxstack/ox-llm/blob/e64b985a2ff6abf51d406471382ed95a18fd161c/oxc/docs/HEALTH-AND-UPDATES.md#update-and-rollback)
preserves the legacy staged-release process, including its seal, preimage,
service identity and rollback checks.

## Install, rollback and smoke checks

Use the native guide for v0.2.x installation and validation. The
[archived developer procedure](https://github.com/oxstack/ox-llm/blob/e64b985a2ff6abf51d406471382ed95a18fd161c/oxc/docs/HEALTH-AND-UPDATES.md#install-rollback-and-smoke-checks)
retains the legacy OEM drill commands and their isolated test guidance.

## Error messages

User-facing errors are short and end with what to do. Status codes, error `type`/`code`, exit codes and
ledger labels did not change in v0.1.4; only the wording did. A memory-guard stop keeps its stable reason
code in the controller (`last_failure.reason`, the ledger and the engine log), and `oxc status` shows
it as a sentence, for example: "The model was stopped to keep the laptop responsive (memory was nearly full).
Close a few apps and try again."
