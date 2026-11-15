# Issue 94 acceptance and remaining gates

Runtime baseline: integration `fc551b2`, following main `c679898`; coordinator policy and the one-shot live result are recorded below. This is an acceptance
inventory, not a completion claim. The authoritative use case is
[issue 94](https://github.com/oxstack/ox-llm/issues/94): a full working evening of
reviews, fixes and merges through the queue, driven from oxa with cloud models
without an external coding-agent driver substituting for oxa's goal loop.

## Requirements and evidence

| Requirement | Current evidence | What is still required |
| --- | --- | --- |
| Protocol/design foundation | [Design](../docs/design/a2a-orchestrator.md), attributed A2A text/data profile, authenticated bounded loopback transport; 32 protocol tests pass at this snapshot | This is the implemented profile, not a claim of complete A2A interoperability with arbitrary third-party agents |
| Long-running goal per session/worker | [Goal ledger](SESSION_GOALS.md), cumulative token/time/turn budgets, DONE/blocked states, persistent native conversations; full orchestrator suite passes | Native detached work is now refused in A2A sessions; use background_command until post-release issue #258. The live smoke failed as detailed below |
| External client and TUI share one session | [Control API](SHARED_SESSION.md), two writers with revision admission, CLI status/events, steering, pause/resume and session rebinding; actual TUI demo passes | Observe this during the real working session, including recovery from a disconnected control client |
| Background work and completion wakes | [Owned background manager](BACKGROUND_TASKS.md), same-parent wakes, unread-result guard, cancellation/deadline/reaping tests pass | Native registry integration is deferred to issue #258. The A2A-only guard refuses native starts/reads and preserves foreground Bash; [policy regression](tools/native_jobs_policy_demo.py) passes |
| Worker roles/models/budgets, steering, results and status | [Worker lanes](WORKER_LANES.md), fixed private grants, native/A2A collection, result verification after reap; actual mixed-provider mock review passes | The approved Sonnet-parent/two-Haiku-worker smoke ran once; both workers failed and no verified edit/review handoff completed |
| Long cloud reliability | [Continuation/compaction demo](CLOUD_GATEWAY_DEMO.md), separate session accounting, tier/cache reconciliation; 150 cloud tests pass in the recorded run | Real provider costs, continuation longevity and cancellation/billing reconciliation remain unproven |
| Esc stops owned inference | [Cancellation](CLOUD_CANCELLATION.md): real TUI closes owned OpenAI and native Anthropic mock transports; unknown usage retains a reservation; repeated interruption preserves cleanup | Provider-side cancellation/billing evidence; transport EOF alone is insufficient |
| GitHub in online Workspace mode | [Private capability integration](UPSTREAM_INTEGRATION.md) passes synthetic sandbox/redaction tests | Authenticated `gh` executed by oxa under its actual Workspace sandbox; this agent's GitHub connector reads do not prove that path |
| Full working evening | No acceptance run yet | Real reviews, fixes, validation and owner-authorized queue operations from oxa; deterministic mock replies do not establish this |

The latest broad lab run, following `2261a3a`, executed 2,310 tests with seven
reported failures and six skips. Five failures are bootstrap manifest pin
mismatches introduced by the optional module inventory change; the other two
are scanner findings reproduced on an untouched baseline. Installation
completeness passes, as do the thirteen focused native SDK/cleanup tests and
thirty-five combined native SDK tests, but the broad suite is not green. Both
real-TUI native TLS modes pass: cancellation and completed tier/cache review.
No scanner, allow-list, qualification flag or test expectation was relaxed.

A later bootstrap isolation reveals an additional integration gate: adding the
optional SDK bridge to `bootstrap.py`'s installation inventory changes that
executable input, so five frozen manifest records no longer match. The affected
records are `build-manifest.json`, `checks-manifest.json`,
`download-manifest.json`, `source-fetch-manifest.json` and `install-checkpoint.json`
under `lab/bootstrap/`. The native TLS tests and installation completeness check
do not qualify those records. Their checks remain intact; no receipt or checksum
is refreshed automatically to make the draft pass. This is introduced packaging
drift, not one of the baseline scanner findings.

## Coordinator decisions and release policy

The coordinator selected **background_command only** for detached work in A2A
sessions until v0.2.0 ships. The new guard lives in the A2A crate and rejects
native Bash `background:true` starts and `job` reads at both access and execution
boundaries. Foreground Bash delegates to the original tool. Normal sessions are
unchanged, and `oxa-tools::JobRegistry` has not been modified.

The finding and post-release bridge design are tracked in
[issue 258](https://github.com/oxstack/ox-llm/issues/258), labelled `post-0.2`.
The historical heartbeat reproduction remains evidence for the integration gap;
use the old integration snapshot for that diagnostic. The current
[policy demo](tools/native_jobs_policy_demo.py) fails without the guard and passes
with it: both native starts and reads are refused, no detached file is created,
and a real foreground command succeeds in the same TUI session. Existing
background_command wake/cancellation/reaping coverage remains in the full suite.

Packaging of the optional SDK bridge remains a separate coordinator decision:
five frozen bootstrap input pins differ after its inventory addition. Those
records and all checks remain intact. Reviews stay deferred until after release.
The four rolling drafts remain #22, #88, #129 and assembled integration #176.

## Approved single live smoke: executed, incomplete

The coordinator authorized one run, a ten-minute parent goal with 20,000 observed
tokens and eight turns, two workers with 10,000 tokens/four turns/five minutes
each (60-second turn limits), and an independently enforced USD 5 cap. Worker
output caps were 512 KiB each with a 1 MiB lifetime pool. The existing shared loopback gateway was used without restart or
reconfiguration. Parent model: `anthropic/claude-sonnet-5-5`. Both author and
reviewer models: `anthropic/claude-haiku-5-5`. Only the gateway handled provider
credentials; no provider credential file was opened, copied or printed.

A private local relay forwarded only this run's inference to the approved gateway,
retained actual context metadata, and capped each response at 512 output tokens.
It stamped a unique nonsecret ledger client tag on each request. The monitor
summed `cost_usd` only for those tags in the owner-designated request ledger,
leaving other shared traffic out of the total. Before admission it included
outstanding conservative full-context/cache-write reservations based on the
nonsecret configured prices. Budget admission/attribution checks passed locally
without inference before the run. A durable one-shot marker prevents automated
re-execution. No rerun was made or scheduled.

The shared gateway advertised USD 200/day in its live status, rather than the
USD 10/day in the coordinator's description. It was left unchanged; the smoke's
separate USD 5 cap did not rely on that shared setting.

| Result | Observed value |
| --- | --- |
| Elapsed time including cleanup/settlement | 22.50 seconds |
| Own requests / settled ledger rows | 10 / 10, all HTTP 200 and settled usage |
| Parent requests / observed tokens / ledger cost | 7 / 26,770 / USD 0.062676 |
| Combined worker requests / gateway tokens / ledger cost | 3 / 7,405 / USD 0.0058525 |
| Individual worker gateway session token totals | 5,289 and 2,116, each below 10,000 |
| Total attributed ledger cost | **USD 0.0685285** |
| Maximum settled spend plus outstanding reservations | USD 1.2890125, below USD 5 |
| Parent runtime outcome | budget_exhausted, after three goal turns |
| Author and reviewer runtime outcomes | both blocked/native_runtime_failure, process cleanup confirmed |
| Worker terminal usage in their runtime reports | unknown, despite settled gateway usage |
| Original source | unchanged |
| Verified author edit/reviewer handoff | not completed; collected author bundle was empty |
| Additional cancellation phase | not attempted because the handoff prerequisite failed |

The parent budget was configured at 20,000 observed tokens, but detection occurs
after request usage arrives. The final parent request consumed 6,931 tokens and
brought its total from 19,839 to 26,770; the runtime then exhausted the budget.
This exposes the difference between an observed token threshold and a strict
pre-request token ceiling. It is not reported as a successful 20,000-token run.
The USD cap remained below its limit throughout the independently recorded
ledger/reservation accounting.

The author made one read/tool request, then a response reached the 512-token
output cap with finish_reason=length. The reviewer response likewise reached
512 tokens with finish_reason=length. Both native workers reported
native_runtime_failure and unknown runtime usage. The parent inspected the failed
results and described a proposed arithmetic diff, but the collected author bundle
contained no applied edit and neither worker achieved runtime DONE. A narrative
completion reply did not override the parent's runtime budget-exhausted verdict.
These observations need an offline truncation/accounting regression before a
future live attempt; they do not establish a fully diagnosed root cause.

The run used a disposable arithmetic file and fixed private role/model/file grants.
No source change was applied, no PR write or merge was attempted, and the shared
gateway and its other goal loop were not restarted or reconfigured. Raw provider
transcripts and private run artifacts are not committed here. The published result
contains only aggregate numeric/accounting and lifecycle evidence.

This single failed smoke does not satisfy issue 94's working-evening acceptance,
prove real cancellation/billing termination, or exercise authenticated GitHub from
oxa's Workspace sandbox. Any further paid attempt requires another explicit go;
the original one-run authorization has been consumed. Real PR/queue operations
remain separately scoped to the owner/operator permitted to perform them.

## Settlement/admission cleanup follow-up

The assembled CLI/TUI integration fixes the #129 review finding for parent
settlement and turn admission: changing goal state now revokes already-running
owned tasks, while explicit background/worker waiting states preserve them.
Failed admission after resuming a waiting goal at its turn limit publishes a
rejection and cancels its children. This fix is in the assembled integration;
the historical #129 source snapshot does not independently contain it.

The new real-session settlement and admission regressions both fail before the
fix (the child completes after the parent exhausts its budget) and pass after
it. The rebuilt real CLI/tmux acceptance also checks Esc, confirmed process
reap, absence of a delayed descendant write and no follow-on inference. The
Esc case waits for the rendered TUI to be READY before sending input. This does
not qualify input sent before the terminal is ready. See
[background cleanup acceptance](BACKGROUND_TASKS.md).
No native JobRegistry change, shared-gateway operation or paid request is part
of this follow-up. The earlier one-shot live result and its remaining acceptance
gaps are unchanged.

## Native worker response allowance and truncated usage

Operator worker profiles now accept an optional positive `response_tokens`
allowance, enforced by the existing child engine adapter and acknowledged before
parent dispatch. It never expands a smaller model/request limit. The profile
and shared worker status expose it separately from lifetime token budgets and
retained output bytes; model arguments cannot widen it.

Owned native-worker regressions show the same synthetic reply truncating at
512 and completing at 2048, with actual fake-gateway request limits, no silent
retry and confirmed process cleanup. The goal wrapper previously discarded
settled truncated usage because it recognized only successful completion as
known; its regression fails before the fix and passes after it. Such a goal
now stops blocked as `native_response_truncated` with its settled usage retained.
Unknown/aborted requests remain unknown. See [worker policy](WORKER_LANES.md).

This is offline regression evidence, not a diagnosis of every failure in the
historical smoke. Its reported result and authorization remain unchanged. No
external relay limit was raised, no live gateway operation was performed, and
no paid rerun is made or authorized by this change. Lifetime token thresholds
can still be exceeded by an in-flight request; strict pre-request total-token
admission and real provider cancellation/billing remain separate gates.

Local validation for this follow-up: the complete interactive-control
orchestrator suite and real CLI/tmux demo pass. An earlier broad run timed out
in the worker-compaction status wait while other acceptance work was running;
all isolated compaction cases and a full sequential rerun pass with the original
timeouts and assertions. The cause of that initial timing failure is not
established, so this is not a claim that long-run timing reliability is solved.

## Control-client backlog and disconnect recovery

The control client no longer blocks on a full Unix-listener backlog before
starting its I/O deadline. Its new owned-socket regression fails before the fix
and passes afterward, and the rebuilt actual CLI returns transport failure
promptly against a deliberately full private listener. No mutation retry or
second runtime is introduced.

The real CLI/tmux demo now loses a steering response on purpose, reconnects
through the actual control CLI, recovers the correlated admission event from
the captured sequence without a history gap, and verifies the same session
receives the clarification exactly once. Its goal loop completes and the TUI
exits cleanly. This closes a missing local reconnect acceptance case; a real
working-session disconnect, owner replacement, retained-history gaps and
provider/billing behavior remain unverified. See
[shared-control acceptance](SHARED_SESSION.md). No paid run, shared gateway
change, credential access or GitHub write is part of this follow-up.

## Contained inference panic and stale DONE follow-up

The owned worker adapter now closes an admitted inference accounting entry with
unknown usage when an engine/stream callback unwinds. Its regression fails
before the fix because the ledger retains an active call forever, then passes
with one end record, a drained ledger and unknown usage. Sink failure remains
unconfirmed and does not cause a retry or invented zero charge.

An owned native-goal regression also demonstrates a time-only goal accepting an
earlier structured DONE reply after a later inference failed. The fix tracks
failure within the current turn, so this now returns blocked/native runtime
failure with unknown usage and confirmed process cleanup. The ordinary fan-out
case already refused success and remains a positive control. A genuinely new
valid reply can recover a time-only goal under its existing
budget policy; its positive regression still retains unknown cumulative usage.
Beginning a later admitted turn or receiving that new reply does not clear the
accounting uncertainty. See
[accounting lifecycle](SESSION_ACCOUNTING.md).

No Core, native JobRegistry, provider SDK, shared gateway or credential path is
changed. No paid request is made. The historical smoke and remaining live-provider,
strict total-token admission and working-evening gates remain unresolved.
