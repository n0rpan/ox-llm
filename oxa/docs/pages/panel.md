# ox AI panel: the data contract

The ox AI panel is the Omarchy bar widget that shows the state of the Rust suite: cloud and local models, usage, the lead session, workers and storage. The panel runs no program for data except `oxa`, and it reads only the JSON that `oxa` prints. This page is the contract between the two. The example files it names are in `oxa/omarchy-panel/tests/fixtures/`; the oxa and panel tests read the same files.

## Rules

- The panel runs exactly one program for data: `oxa`. It opens no file, makes no HTTP request and never names a path under the configuration folder, a ledger or a key file. Everything it knows arrives as JSON on standard output.
- The `oxa` commands the panel uses are read-only: no model call, no network request, no write and no credential content. They may check a key file with `stat` (exists, mode) and nothing more.
- Free text from `oxa` is not shown. Only identifiers are: model ids, worker names, branch names, file names and host names. Sentences come from the panel's own words.
- Every command prints one JSON object and exits 0, or prints an error object and exits 1:

  ```json
  {"type":"oxa_error","version":1,"command":"usage","code":"ledger_unreadable"}
  ```

  The `code` is one of a closed set the panel has words for. An unknown code is shown as the code itself. See `error.json`.
- Every object has `type` and `version`. The panel reads version 1 and ignores fields it does not know. A version above 1 means the panel is older than `oxa`.
- The panel finds `oxa` at the widget setting `oxaPath`. An empty setting means `$HOME/.local/bin/oxa`.

## `oxa status --json`

Fixture: `status.json`. `oxa status <session>` keeps its meaning (one session's status file); only the form without a session id prints the suite status.

| Field | Meaning |
|---|---|
| `oxa.version`, `oxa.panel_build` | oxa's version and the id of the panel files it carries. The panel shows "Panel update" when its own build id differs. |
| `capabilities` | The commands this binary has: any of `models`, `usage`, `storage`, `panel_install`, `local_add`, `studio`, `settings_flag`. A command is listed only when this binary has it. |
| `default_model` | The model used when none is named. |
| `local.state` | `off`, `not_wired`, `ready`, `running` or `error`. `engine_binary` is `found` or `missing`. `running` lists `{model, sessions}` for engines that a live session proves. |
| `cloud.state` | `off`, `ready`, `no_credentials` or `error`. `key_file` is `present`, `missing` or `unsafe_mode`; `chatgpt_login` is `present` or `missing`. Both come from `stat` only. |
| `lead.state` | `off`, `idle`, `pursuing`, `awaiting_owner`, `paused`, `stalled` or `unknown`. `open_asks` is a count; ask text is never included. `serve.url` is the loopback address of the lead page. `capacity` is the owner's user-only per-host worker capacity map (`{"<host>": <count>}`), or `null` when unset; display only. |
| `sessions` | Session status files whose recorded process is alive. |
| `workers` | Counts by state and up to the newest rows. A finished row carries `outcome`. |
| `usage_state` | The worst pool state from `usage`, or `unknown`. |

Each `fix` field names the one action that helps: `local_add`, `enable_local`, `install_engine`, `add_key`, `fix_key_mode`, `enable_cloud`, `fix_config`, or `null` when nothing is needed.

## `oxa models --json`

Fixture: `models.json`. Plain `oxa models` prints the same models as a table: where each runs (`local` or `cloud · <provider>`), its limits and which one is the default.

- `models[].kind` is `cloud` or `local`. `billing` is `metered`, `subscription` or `local`. `pool` names the usage pool the model draws from, so the panel can join it with `usage`.
- `models[].state` is `ready`, `no_credentials`, `unpriced`, `not_wired`, `engine_missing` or `unavailable`.
- `thinking` and `images` are `true`, `false` or `null`.
- Unwired local rows are `.gguf` files, and folders holding `.gguf` files, directly inside the configured model folders. Their `id` is the file name without `.gguf`. A file already wired in the configuration is a wired row, not a second one.
- `fix` is `local_add`, `add_key`, `add_price`, `install_engine` or `null`.
- Listing models reads no key and no ledger, and makes no provider contact.

## `oxa usage --json`

Fixture: `usage.json`. Only sums leave the ledger: no row, session id, digest or model text.

- `day` is the UTC day of the sums. `resets_ms` is when the day's counters reset. `ledger` is `ok`, `absent` (no request yet today: all zeros, not an error) or `unreadable` (then the command exits 1 with `ledger_unreadable`).
- Each pool has `billing`, `providers`, `enforcement` (`none`, `warn` or `refuse`, a fact about what oxa does at the limit) and `state`.
- `state` is `none` (no limit configured), `ok`, `low`, `over` or `unknown`. oxa computes it; the panel holds no threshold.
- `budget_usd`, `request_cap` and `token_cap` are `null` when not configured. `credit` is `null` or `{amount_usd, used_usd, next_refill_ms}`.
- `windows` lists provider plan windows that oxa has already observed. This command never fetches one, so an empty list means "not observed".

## `oxa storage --json`

Fixture: `storage.json`. Sizes come from file metadata; file contents are not read.

- `disk` gives free and total bytes of the volume that holds the configuration.
- `entries[].kind` is `model`, `sessions`, `ledger`, `catalog_cache` or `debug_log`.

## Cadence and the old-binary guard

| Command | When | Timeout |
|---|---|---|
| `oxa status --json` | every 5 seconds while polling is on; at once on open and after an action | 3 seconds |
| `oxa models --json` | on open; every 60 seconds while the Models tab is shown; after an action | 5 seconds |
| `oxa usage --json` | on open; every 30 seconds while the Usage tab is shown; at once when `usage_state` changes | 5 seconds |
| `oxa storage --json` | when the Storage tab is shown; every 60 seconds while it stays shown | 10 seconds |

One command of a kind runs at a time. A poll that finds its command still running is skipped.

The panel runs `models`, `usage` and `storage` only after a `status` answer from the same round lists that word in `capabilities`. Older binaries answer `status --json` with a usage error (`old_binary.txt` is an example of that text), so they never run an unknown word as a prompt.

## Faults

The hero line and each tab name the fault, the exact command and the fix. Nothing empties the whole panel because one command failed.

| Fault | How it is detected | Fix shown |
|---|---|---|
| `missing` | the program did not start (no output, exit 126 or 127) | install the suite, or set the oxa path |
| `old` | exit non-zero with no error object | install a newer oxa, then run `oxa panel install` |
| `failed` | an error object and exit 1 | run the command in a terminal to see the message |
| `timeout` | still running after the command's timeout (the panel stops it) | run the command in a terminal |
| `unreadable` | exit 0 but not an object of the expected type (`empty.txt` is an empty answer of this kind) | run the command in a terminal |
| `newer` | `version` above 1 | run `oxa panel install` |

After a fault the status poll continues every 5 seconds, so the panel recovers by itself when oxa is installed or updated.

## Changing the contract

A change to a field or an enum changes the fixtures in the same pull request, and the oxa and panel tests read the fixtures. A new field is added without a version change. A removed or renamed field, or a changed meaning, bumps `version`.
