# Opt-in supervision of quiet cloud connections

The #160 diagnostic found a concrete #94 reliability gap: terminal Escape stopped
and reaped the native worker, but its gateway retained a quiet Responses upstream
because forwarding blocked on `getresponse`/`read1`. Client closure was detected
only on a later write. The same automated loopback regression fails before this
hook and passes afterward.

## Operator switch and scope

The gateway's existing `cloud` configuration object may explicitly set:

```json
{"orchestrator_disconnect_watch": true}
```

Missing, false or non-boolean values leave the existing behavior unchanged. No
configuration file, installed service or default is changed by this branch.
Only an operator-controlled gateway configuration enables the hook; model or
worker request fields cannot enable it.

For the shared OpenAI/OpenRouter HTTP transport, the hook reuses the existing
`ClientDisconnect` watcher after sending the complete upstream request and before
waiting for response headers. It peeks at the fully read downstream socket,
without consuming pipelined bytes or changing its timeout. EOF shuts down the
owned upstream transport and wakes a blocked read, including an HTTPS upstream.
The watcher is joined before that transport is released.

The downstream must be plain HTTP, as used by the local TUI/worker gateway route.
The existing watcher cannot peek at a downstream TLS socket, so that path is
explicitly skipped. Native Anthropic SDK forwarding is separate and is **not
covered by this hook**. The OpenAI capability flag for provider cancellation is
unchanged; transport shutdown is not a provider billing guarantee.

## Accounting

An observed client disconnect with no complete provider usage is recorded as
HTTP 499 / `client_disconnected`, with the existing positive conservative
reservation. It is not turned into a zero bill or an automatic retry. The original
write-before-reservation-release and fail-closed ledger behavior are preserved.

A completed provider response can race with the client closing after receiving
its final event. Valid terminal usage retains its measured settlement; normal
closure must not turn that completed request into a worst-case reservation.
Explicit failures, including oversized responses, retain their error and
conservative charge even when a late disconnect is also observed. TLS testing
exposed the completion race, and a deterministic gateway regression protects it.

The TUI's canceled worker still reports unknown usage if it did not receive final
SDK counters. Its parent pauses with its original budget and token ledger.
Neither this hook nor a goal change invents usage or clears that debt. HTTP 499 is
explicit client-cancellation metadata; the ledger's existing completeness labels
are otherwise unchanged. This does not fully resolve #71's Anthropic accounting
and estimation request.

## Verification

With the feature-built binaries from the existing stack:

```sh
cd oxa
python3 -I oxa-orchestrator/tools/cloud_gateway_demo.py --case cancel-quiet --disconnect-watch
python3 -I oxa-orchestrator/tools/cloud_gateway_demo.py --case cancel-headers --disconnect-watch
python3 -I oxa-orchestrator/tools/cloud_gateway_demo.py --case cancel-quiet --disconnect-watch --tls-upstream
python3 -I oxa-orchestrator/tools/cloud_gateway_demo.py --case cancel-stream --disconnect-watch
```

The TLS fixture creates an ephemeral self-signed certificate/key under its private
temporary root and trusts only that owned certificate for the provider mock.
Verification is not disabled. A separate owned fake local backend keeps plaintext
metadata probes off the TLS listener. No actual cloud or installation key is read;
no certificate/key is committed. This fixture additionally requires `openssl`.

The actual tmux cases require upstream closure within three seconds, a reaped
worker, the same paused parent with its 40-token ledger unchanged, unknown child
usage, exactly four gateway rows with one reservation, and clean Ctrl-D exit.
Completed rows must retain HTTP 200 and usage settlement. The existing two-summary
compaction/control-client scenario also passes with supervision enabled.

`lab/tests/test_cloud_disconnect.py` is discovered by the normal lab suite. It
checks quiet Responses and OpenRouter streams, EOF before headers, explicit
cancellation accounting, complete-usage races, preservation of explicit failures,
default-off behavior, downstream-TLS exclusion, hold release and watcher cleanup.
The existing watcher socket tests cover its peek/lifetime behavior. Run through
the prescribed isolated wrapper:

```sh
cd lab
sh tools/run_lab_tests.sh discover -s tests -p test_cloud_disconnect.py
sh tools/run_lab_tests.sh discover -s tests -p test_client_disconnect.py
```

All product changes are small default-off hook sites in `ctl_cloud.py`: start the
existing watcher, classify observed cancellation without corrupting completed
usage, suppress writes to a vanished client, and join on every exit. No new
transport implementation, provider SDK, listener, permission or retry is added.

A broad lab run during implementation executed 2,325 tests with two failures and
17 skips. Both failures reproduce in the untouched `32059c6` baseline: the client
neutrality scan flags an existing repository-path check in `cloud.py`, and the
tracked-tree scrub test reports machine-dependent host-name hits in eleven
existing files. Neither scanner, allow-list, environment policy nor those source
files is weakened or changed here. The final seven focused cloud-disconnect
regressions and nine existing watcher tests pass; broader lab validation is not
claimed green.

This requires T3 review. Public draft checks, real provider billing, native
Anthropic cancellation, coordinator review and the full working evening remain
separate gates. Paid cloud use still requires Oscar's go.
