# Main-session inference accounting and compaction

The feature-built TUI can charge every logical SDK inference on its engine to an
active goal, including summary and zero-output prefill calls that do not produce
an assistant reply event. The existing dependency augmentation hook installs the
source only when `OXA_SESSION_CONTROL_DIRECTORY` is configured. The attachment
finds it by the exact engine `Arc` used by its Core session. There are no new hooks
in existing crates, no separate driver session and no stdout accounting protocol.

Automatic main-goal compaction is an additional operator opt-in:

```sh
OXA_GOAL_AUTO_COMPACTION=1 OXA_SESSION_CONTROL_DIRECTORY=/private/agent/control \
  oxa
```

Use an `orchestrator` feature build and the protected control directory described
in [SHARED_SESSION.md](SHARED_SESSION.md). The switch is captured at attachment
startup and enables compaction only when the source is present. Agent text,
steering and goal retargeting cannot enable it. Automatic agent retries stay off.
The configured model and compaction policy remain Core-owned; this source does
not choose a provider or grant any new tools or permissions.

## Admission and settlement

A new goal starts with the source closed. A goal cannot be funded over an active
unscoped inference; the correlated `set_goal` request is rejected instead of
assigning that request to a new ledger. Background work is asked to cancel; the
operator can retry after it settles. Each admitted goal turn opens inference with
its remaining cumulative token and boot-clock time allowance. SDK calls check
that gate before entering the inner engine. Pausing, cancellation and time exhaustion close it and cancel every outstanding
source token. Normal turn settlement freezes admission and waits for outstanding
returns before publishing final usage or DONE; budgets and interrupts still apply
while it waits. A later summary or prefill cannot bypass the closed gate.

Successful SDK returns contribute prompt plus completion tokens exactly once,
independently of chat/compaction presentation events. Zero-output prefill is not
free merely because there is no reply. Presentation events are used for legacy
accounting only when there is no matching source. The metered goal uses SDK
returns instead of adding presentation usage or counting a compaction event twice.

Failed SDK calls leave usage unknown. Unfinished calls keep the turn unsettled
rather than reporting known zero. Failed inference refuses
further SDK calls in that goal turn and blocks automatic continuation, including
for time-only goals. An explicitly resumed time-only goal can continue with an
unknown cumulative total; a token-bounded goal cannot. A new turn cannot reset a
still-running request. Admission failures before the SDK is called add no usage.
Non-inference engine methods continue to forward to the original engine.

Token caps are observed limits, not reservations or provider monetary caps. A
completed SDK call can overshoot the remaining allowance; its full reported usage
is retained and another call is refused. Root model changes and configured
summary-model overrides retain their existing authorization and contribute to
the same token ledger. USD prices, cached-token billing and upstream HTTP retries
are not inferred from token totals.

Embedders must augment the final dependencies before opening the session. If they
replace the engine afterward, the attachment cannot match the source: it retains
the legacy presentation-event accounting path and leaves automatic goal compaction
off. Ordinary non-goal TUI work retains its existing behavior.

## Mock evidence

```sh
cargo test -p oxa-orchestrator --features interactive-control --test shared_tui -- --main-compaction-only
```

Six actual TUI fixtures run a 14 KiB goal against an owned loopback fake with a
50,000-token context: successful three-turn compaction; DONE at exactly the
measured cap; summary cap exhaustion;
unrecoverable summary failure with a token budget; the same failure with only a
time budget; and cancellation during a summary. They require actual compaction
inference, retained summary context in later chat, the same session/goal/model,
unchanged allowances, and reconciliation of the final known ledger with all SDK
requests. Failure/cap/cancel cases prevent a third goal turn; failed usage stays
null. The canceled summary connection closes at the fake endpoint. The TUI and
broker remain alive after settlement to catch unwanted follow-on inference.

Two additional TUI fixtures submit a delayed prefill through the same wrapped
session engine while a reply runs. They verify that the goal waits for its usage,
can cancel during that wait, closes the canceled connection, and refuses a late
prefill before it reaches the fake SDK endpoint.

Engine-source tests separately exercise chat, summary and zero-output prefill,
refusal before SDK entry, pending-call cancellation, unknown usage, expired
admission and explicit time-only continuation. The complete orchestrator suite
also covers shared control, worker lanes, background tasks, pause/resume and TUI
session swaps.

No real gateway, provider keys or paid calls were used. This does not verify
provider billing/cancellation, Responses continuation recovery (#64), gateway
settlement (#47/#48/#71), upstream #60 integration or the full working-evening
acceptance. Coordinator and T2/T3 reviews remain outstanding.
