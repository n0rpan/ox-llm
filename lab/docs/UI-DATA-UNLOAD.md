# Panel data: "Unload now" per loaded model

Addendum to `docs/UI-DATA.md` (which lives on branch `claude/ui-rust-era`; link this page from its
"Controller endpoints" section when the branches meet). Source only, not deployed. Controller at
127.0.0.1:8092, same-origin rules as the other `/_local/*` routes.

## Why

The Rust Flash engine stays loaded for 600 s after its last request (other models: 120 s), holding about
27 GiB at a 20 GiB expert cache. The user must be able to free that memory at once.

## Read: `GET /_local/status` (new fields; polling never loads or wakes a model)

| field | type | meaning |
|---|---|---|
| `loaded_model` | string or null | alias of the model whose engine process is running; null when nothing is loaded |
| `resident_bytes` | integer or null | resident memory (VmRSS) of that engine process; null when nothing is loaded or it cannot be read |
| `idle_unload_seconds` | number or null | idle timeout of the loaded (else the selected) model: 7200 by default since v0.2.0 (O-160 smart keep-warm), a profile's own value if set; null = no idle unload |
| `idle_unload_remaining_seconds` | number or null | seconds until the automatic unload; null while a request is running, while loading, or when nothing is loaded |
| `unload.available` | boolean | true when a model is loaded and no request is running: the button is enabled |
| `unload.method`, `unload.path` | string | `POST`, `/_local/unload` |
| `state` | string | unchanged: `ready`, `loading`, `unloaded` |
| `keep_warm` | object | v0.2.0: effective `idle_unload_seconds`, `psi_some_avg10`, `min_available_mib`, `pressure_grace_seconds`; `last_unload` {reason: idle, pressure_psi or pressure_available, ...} and `last_sample` |
| `model_preload` | object or null | v0.2.0: the last `POST /_local/preload`: {state: loading, ready or failed, model, error, t} |

At most one language model is loaded at a time, so "per loaded model" is one row: show `loaded_model`, its
`resident_bytes` (GiB, one decimal) and a countdown from `idle_unload_remaining_seconds` (re-read on each poll;
do not run a client-side clock for longer than one poll interval). When the value is null and `state` is
`ready`, show "in use".

## Act: `POST /_local/unload`

- Body: empty, or `{"model": "<alias>"}` to unload only if that alias is the loaded one (use this from a
  per-model button so a stale page cannot unload a different model).
- `200 {"state": "unloaded"}`: the engine process was stopped (SIGTERM, then SIGKILL after 10 s) and the
  inference lease released. Rust engines write their warm-up file on SIGTERM, so the next start is fast.
- `409 {"error": "A request is running. Try again when it finishes."}`: nothing was stopped.
- `409 {"error": "That model is not loaded."}`: the alias in the body is not the loaded model.

Command line: `local-ai unload` (existing; same route, no alias).

## Not provided

No per-model memory when nothing is loaded (use the profile's admission numbers for an estimate), and no
"keep loaded" switch in the panel. v0.2.0 smart keep-warm (O-160, `keep_warm.py`): an idle model stays loaded
for 2 h unless memory pressure (PSI `some avg10` >= 10 %, or MemAvailable < 1024 MiB, sustained 3 s, at least
60 s after the last request) unloads it first. Knobs: env `LOCAL_LLM_KEEP_WARM_IDLE_SECONDS` (`off` = never;
600 = the old behaviour), `LOCAL_LLM_PRESSURE_PSI_SOME_AVG10`, `LOCAL_LLM_PRESSURE_MIN_AVAILABLE_MIB`,
`LOCAL_LLM_PRESSURE_GRACE_SECONDS`, or the runtime JSON `keep_warm` block with the same field names as
`status.keep_warm`; a Flash profile's own `oxe_flash.idle_unload_seconds` (60-86400) still wins for its idle
time. The legacy `LOCAL_LLM_IDLE_SECONDS` is ignored.

## Preload: `POST /_local/preload` (v0.2.0)

Body empty or `{"model": "<alias>"}`; same loopback/same-origin rules as the other control routes, never on a
LAN share. Starts the load in the background and answers at once: `202 {"state":"loading","started":true}`;
again while it loads `202 ... "started":false`; once loaded `200 {"state":"ready","started":false}`. It never
switches away from another loaded model (`409 other_model_loaded`); `409` while an image runs; `404`/`503`
for an unknown model / unfinished setup. Follow `status.model_preload` and `status.state`.
