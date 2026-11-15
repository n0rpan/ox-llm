# Real CLI / tmux demo (loopback mocks only)

This checks the feature-built `oxa` executable in a real terminal, in addition to
`shared_tui`'s embedded TUI fixtures. It covers the daily-driver path from #94:
external control and TUI share a session, an owned worker appears in the footer,
Esc stops it while the parent waits, `/new` rebinds control, and a budgeted main
goal compacts without losing its context or ledger.

## Run

On the x86_64 Linux development machine, with Python 3.9+, tmux and working
bubblewrap/user namespaces:

```sh
cd oxa
cargo build -p oxa-cli --features orchestrator -p oxa-fake
python3 oxa-orchestrator/tools/live_tui_demo.py
```

An alternate build directory can be supplied with `--bin-dir <directory>`.
The script creates a uniquely named tmux server with no user configuration, an
empty temporary HOME/workspace, private operator settings/profiles and two owned
loopback fake engines. Child environments are cleared. Offline mode stays on;
context files, extensions, skills, MCP and web search stay disabled. The only
selected main tool is `worker_lane`. Workers receive no file grants or shell.
No installed oxa, production gateway, actual model, keys or network service is
used. The fake processes and owned tmux server are stopped in `finally`.

`--no-builtin-tools` would remove `worker_lane` too; the demo uses the explicit
`--tools worker_lane` allow-list instead. It retains normal tool selection rules.

The printed temporary directory contains synthetic request records, four captured
terminal frames and `result.json`. A failure also preserves the terminal frame
and latest socket status. These artifacts are for local inspection; full request
bodies and terminal captures are not copied into the repository.

## Observed run

Feature binary built from `450d909` (main-session usage draft). The script:

1. Sets and resumes a goal through the Unix socket, then steers the in-flight
   turn. It checks the clarification reached the next model request in the same
   session and reconciles that goal's token ledger with the SDK records.
2. Funds a second goal, starts a native worker through A2A, waits for actual worker
   inference and the visible footer row, and sends an actual terminal Escape.
   The parent pauses without changing its ledger/allowance; the worker becomes
   canceled and is reaped. The terminal footer also shows cancellation.
3. Types `/new`, verifies the attached session changes and revisions remain
   monotonic, then funds a 14 KiB goal. It observes three turns, two actual
   summaries, retained summary context, and two zero-output prefills under the
   same new session and original allowance. The captured terminal shows both
   compaction and final goal completion.
4. Checks no follow-on inference appeared, sends Ctrl-D and requires exit 0.

Condensed output from the successful run:

```json
{
  "shared_session_steer": true,
  "idle_escape_paused_parent": true,
  "worker_footer_verified": true,
  "worker_canceled_and_reaped": true,
  "tui_session_rebound": true,
  "compacted_goal_turns": 3,
  "compacted_goal_tokens": 176051,
  "sdk_kinds": [
    "chat", "chat", "compaction", "prefill",
    "chat", "compaction", "prefill"
  ],
  "tui_exit": 0
}
```

This is a reproducible mock demo, not the cloud working-evening acceptance.
Provider billing/cancellation, #64 continuation recovery, #47/#48/#71 settlement,
upstream #60 integration and coordinator/T2/T3 review gates remain outstanding.
No change is merged by this demo.

The upstream-integration version also funds a second worker after cancellation,
waits for its verified result to wake the same parent, and checks successful
completion before `/new`. See [UPSTREAM_INTEGRATION.md](UPSTREAM_INTEGRATION.md)
for the updated receipt and Core environment-note compatibility.
