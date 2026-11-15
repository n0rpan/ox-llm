# Esc while the main session waits for workers

A main session can be idle while its workers are still generating. Previously,
Esc in that state followed the TUI's idle navigation path. No main turn was
aborted, so the existing aborted-turn cleanup hook never ran.

With the default-off `session-control` feature and an attached control owner,
the TUI first offers an idle interrupt to that same session. An active or blocked
goal is paused; owned workers and background commands receive cancellation. The
main goal's token ledger, turn count and allowance stay unchanged. Cleanup and
results remain observable through the existing socket, events and worker footer.
Paused goals require explicit resume. Without a parent goal, completion wakes
stay suspended until a new operator command is admitted, so canceled work cannot
start another inference through its result wake.

The UI and worker threads share only weak attachment registrations. The hook
matches the session ID and protected agent directory, declines ambiguous owners,
and rechecks identity under the owner lock. Busy main turns retain their existing
abort path. When no attached goal or owned work can be interrupted, existing idle
Esc navigation remains available.

The only new existing-crate hook is five lines in
`oxa-interactive/src/driver.rs`, behind `session-control`. The implementation and
integration fixtures live in `oxa-orchestrator`.

## Verification

```sh
cargo test -p oxa-orchestrator --features interactive-control --test shared_tui -- --idle-interrupt-only
```

Actual TUI Esc input interrupts two sandboxed native RPC workers, both through
local profiles and explicit gateway profiles backed by owned loopback fakes. A
third case starts workers without a parent goal. The fixtures require both
inference requests to be in flight before Esc, verify connection closure and
process reaping, check unchanged parent accounting, and reject unrelated session
or directory matches. They also verify that the main session makes no follow-on
inference after cleanup.

These are mock connection and process guarantees. They do not establish that a
real gateway cancels an upstream provider request or stops provider billing.
Cloud acceptance and the required coordinator/T2/T3 reviews remain outstanding.
