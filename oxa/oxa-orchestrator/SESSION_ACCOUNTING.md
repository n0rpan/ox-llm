# Stable cloud session accounting and tiered gateway reconciliation

Remote-model SDK requests previously omitted all hint headers, including the
generic `X-Session-Id`. The gateway therefore fell back to hashing the first user
message. That is not the TUI's stable session identity: goal changes or compaction
can change it, and sessions with identical initial text can collide. Distinct
model aliases in earlier demos concealed the missing explicit identity.

## Default-off engine hook

`oxa-engine/session-accounting` preserves only `X-Session-Id` for remote model
requests with a nonempty session ID. The existing default-off CLI `orchestrator`
feature enables it. The remote body contract, cancellation capability and local
X-Oxe/idempotency hints stay as before. Default engine builds omit the remote
header, as checked by the same HTTP regression test with the feature off.

The only additional existing-crate hooks are the engine feature, its four-line
remote-header block, and the CLI feature chain. Two redundant borrows in the
same engine file are removed to satisfy strict Clippy; they do not change behavior.
There are no Core, config, provider, controller, sandbox or workflow changes.

The gateway already consumes the generic header and stores its keyed hash in the
ledger. Both actual provider mocks assert that this header never reaches them.
The regression first failed with an absent HTTP header, then passed after the
hook. A real-terminal compaction run additionally requires every parent request,
including summaries, to retain the hash of the known parent session. Its worker
has one separate nonempty identity.

## Pricing dependency test

The mixed-provider review demo can additionally use #162's pricing gateway pinned
at `ec24622`. It is read from a separate checkout with `--gateway-root`; no provider
branch is edited or merged. With the feature-built binary and the private SDK
environment described in [REVIEW_LANE_DEMO.md](REVIEW_LANE_DEMO.md):

```sh
/tmp/oxa-review-sdk/bin/python -I oxa-orchestrator/tools/review_lane_demo.py \
  --pricing --shared-model --gateway-root /absolute/path/to/pricing-checkout
```

`--shared-model` assigns the author and parent the same gateway alias and upstream
GPT model. Ledger reconciliation uses separate session hashes, not model grouping.
Only the isolated fixture's private install salt is used to correlate the known
parent ID; no actual installation ledger or salt is read. The Sonnet reviewer
receives the exact collected proposal through the native Anthropic SDK. Source
contents remain unchanged, both workers are reaped, and Ctrl-D exits 0.

`--pricing` uses synthetic configured prompt tiers and explicit cache rates:

| Total prompt | Ordinary input / million | Output / million | Cache read / million | Cache write / million |
| --- | ---: | ---: | ---: | ---: |
| At most 15 | 1 | 2 | 0.1 | 1.25 |
| Above 15 | 5 | 10 | 0.5 | 6.25 |

The reviewer adds five synthetic cache-write tokens to its 12 ordinary input and
three cache reads. Its 20-token total prompt selects the upper tier. The fixture
uses a synthetic daily gateway cap of 10 to admit the full-window worst-case
reservations at these rates; this is not a paid-run allowance or a default change.

The real-TUI run reconciles ten usage-settled gateway rows:

| Session | Requests | Tokens | Synthetic catalog USD |
| --- | ---: | ---: | ---: |
| Parent | 7 | 140 | 0.000196 |
| Author, same model alias | 2 | 40 | 0.000056 |
| Sonnet reviewer | 1 | 28 | 0.00017275 |

The reviewer row must retain three cache-read and five cache-write tokens.
Each session's cost is summed with decimal arithmetic and compared with an
independently calculated expectation. These are gateway catalog estimates using
synthetic rates, not real provider bills or a new monetary budget in oxa.

The scenario uses only the owned Responses/Messages mocks and empty HOME/XDG
state. It does not invoke the production controller entry point or its public
price-refresher lifecycle. Unexpected public price fetches are rejected by a
fixture guard. #162's live metadata refresh and provider-reported bills are
separate contracts, not established by this demo.

The active-stream Esc scenario also passes against this gateway: it closes the
upstream, reaps the worker, preserves unknown child usage and records one positive
conservative reservation. That reservation is not an actual bill and does not
resolve #71. The quiet-stream cancellation gap from #160 remains open. Formal
reviews, authenticated GitHub work and the full working evening remain gates;
paid cloud use still requires Oscar's go.

## Native SDK unwinding and per-turn failure

An admitted native inference call owns an accounting closure. Normal completion
emits its actual SDK usage exactly once; an engine or stream-callback unwind
attempts an end record with unknown usage. The callback panic remains contained
by the existing Core boundary. It is not converted into successful inference,
zero usage or permission to refund a reservation. If the accounting sink itself
fails, the parent still lacks settlement evidence; the guard neither retries
output nor claims delivery. Cleanup also contains a secondary sink panic.

The regression invokes the real scripted engine with a panicking stream
callback inside the existing containment boundary. Before the fix, the ledger
keeps an active call forever. After it, the ledger is drained but remains
unknown, with exactly one ready/start/end sequence and no inferred usage.

Separately, a completed assistant reply cannot hide a later SDK failure in the
same turn, such as failed summary or prefill. The native owner tracks that
failure independently of the lifetime usage ledger. A time-only goal previously
accepted an earlier structured DONE reply after such a failure. The owned RPC
regression fails with DONE before the fix and passes as blocked/native runtime
failure afterward, with unknown cumulative usage and confirmed process cleanup.
Ordinary token-bounded fan-out rejection is retained as a positive control.

The failure marker clears when the existing owner admits a new turn, including
explicit pause/resume, or when a genuinely new valid assistant reply completes
after the failure. A time-only recovery regression preserves that distinction:
the new reply may complete its goal, but cumulative usage remains unknown.
Neither route clears or repairs accounting uncertainty; token-bounded
continuation remains restricted. Runtime failure does not gain an automatic
restart or retry path.
These are local lifecycle/accounting regressions, not provider billing proof.
