# Issue 94 acceptance and remaining gates

Snapshot: integration `2b33683`, following main `c679898`. This is an acceptance
inventory, not a completion claim. The authoritative use case is
[issue 94](https://github.com/oxstack/ox-llm/issues/94): a full working evening of
reviews, fixes and merges through the queue, driven from oxa with cloud models
without an external coding-agent driver substituting for oxa's goal loop.

## Requirements and evidence

| Requirement | Current evidence | What is still required |
| --- | --- | --- |
| Protocol/design foundation | [Design](../docs/design/a2a-orchestrator.md), attributed A2A text/data profile, authenticated bounded loopback transport; 32 protocol tests pass at this snapshot | This is the implemented profile, not a claim of complete A2A interoperability with arbitrary third-party agents |
| Long-running goal per session/worker | [Goal ledger](SESSION_GOALS.md), cumulative token/time/turn budgets, DONE/blocked states, persistent native conversations; full orchestrator suite passes | Native Bash background work must participate in goal lifecycle; real model behavior remains unverified |
| External client and TUI share one session | [Control API](SHARED_SESSION.md), two writers with revision admission, CLI status/events, steering, pause/resume and session rebinding; actual TUI demo passes | Observe this during the real working session, including recovery from a disconnected control client |
| Background work and completion wakes | [Owned background manager](BACKGROUND_TASKS.md), same-parent wakes, unread-result guard, cancellation/deadline/reaping tests pass | Native `bash` jobs use a separate registry; the [diagnostic](NATIVE_JOBS_DEMO.md) still reaches DONE while its heartbeat continues |
| Worker roles/models/budgets, steering, results and status | [Worker lanes](WORKER_LANES.md), fixed private grants, native/A2A collection, result verification after reap; actual mixed-provider mock review passes | Real GPT-author/Sonnet-reviewer judgment and a working project lane have not been exercised |
| Long cloud reliability | [Continuation/compaction demo](CLOUD_GATEWAY_DEMO.md), separate session accounting, tier/cache reconciliation; 150 cloud tests pass in the recorded run | Real provider costs, continuation longevity and cancellation/billing reconciliation remain unproven |
| Esc stops owned inference | [Cancellation](CLOUD_CANCELLATION.md): real TUI closes owned OpenAI and native Anthropic mock transports; unknown usage retains a reservation; repeated interruption preserves cleanup | Provider-side cancellation/billing evidence; transport EOF alone is insufficient |
| GitHub in online Workspace mode | [Private capability integration](UPSTREAM_INTEGRATION.md) passes synthetic sandbox/redaction tests | Authenticated `gh` executed by oxa under its actual Workspace sandbox; this agent's GitHub connector reads do not prove that path |
| Full working evening | No acceptance run yet | Real reviews, fixes, validation and owner-authorized queue operations from oxa; deterministic mock replies do not establish this |

The most recent broad lab run executed 2,306 tests with three failures and six
skips. Two scanner findings reproduce on an untouched baseline. The third was a
missing runtime installation entry introduced by the SDK bridge; it is fixed,
and all ten installation integration tests pass. That broad run is not
retroactively green. No scanner, allow-list or test expectation was relaxed.

## Coordinator decisions

1. Choose how native Bash jobs join A2A ownership, DONE guards, cancellation and
   completion wakes. The existing question is pending. Main's progress counters
   and bounded job reads do not establish that ownership.
2. Obtain the owner's explicit go and concrete route/credential/budget scope for
   a live experiment. No live credentials have been read for this work. Paid
   experiments remain outside tests and CI.
3. The full evening's real PR writes and queue operations need the owner's scope
   and an operator who is allowed to perform them. This worker's standing brief
   forbids merges. A cloud-smoke go does not change that rule.

Release mode keeps exactly four rolling drafts: protocol #22, core #88,
background/goals #129 and assembled CLI/TUI #176. The latter preserves the full
integrated implementation. Superseded drafts are closed with replacement links;
source branches are retained. No reviews are requested or awaited before release,
and no CI polling is part of this acceptance work.

## Proposed first live smoke, only after explicit go

This proposal is deliberately smaller than the final evening and cannot satisfy
that acceptance on its own. It is concrete enough to approve or adjust before any
credential is read or request is sent:

- Use a separate, owner-approved literal-loopback gateway instance and private
  state/ledger/credential location on the development machine. Do not reuse or
  alter an installed production service. The owner supplies the allowed parent,
  author and reviewer model aliases and the gateway launch configuration.
- Parent: ten-minute goal, 20,000 observed tokens and eight prompt turns. Two
  fixed version-2 gateway worker profiles: five minutes, 10,000 observed tokens,
  four prompt turns and a 60-second per-turn limit each. Worker lifetime totals:
  20,000 tokens and 1 MiB captured output; each worker has a 512 KiB output cap.
- Propose a gateway daily cap of USD 5 and 512 maximum output tokens per provider
  request, subject to the owner's explicit approval. Admission must retain the
  real context window and configured pricing; a conservative reservation that
  exceeds the cap refuses the run. Never shrink metadata, discount unknown usage
  or automatically increase a limit to make the experiment proceed. These are
  local controls, not a claim of a hard provider billing ceiling.
- Work only on a disposable copy of a small, owner-selected source file. The
  author may propose an edit to that file; the reviewer receives the exact
  collected bundle and no file or shell tools. Source application, PR writes,
  commits, pushes and merges are outside this first smoke.
- Use the existing private [gateway profiles](GATEWAY_WORKERS.md) and
  [shared-session socket](SHARED_SESSION.md). An external observer records the
  actual session ID, explicit goal budget and admission events, without issuing
  substitute prompts for each autonomous turn. Do not repurpose mock demo
  scripts by disabling their owned-endpoint or synthetic-key guards.
- Exercise a completed author/reviewer handoff and, under a separately bounded
  request, Esc cancellation. Check actual worker reaping, provider request
  termination where observable, unknown-usage reservations and independent
  ledger/provider cost evidence. If billing cannot be verified, record it as
  unverified. Do not infer a zero charge from EOF or a local canceled state.
- Retain only operator-approved, redacted evidence: source hashes, goal/worker
  state transitions, admission IDs, usage/cost totals and stop evidence. Raw
  credentials and provider transcripts do not go into the repository or PR.

Before starting, the coordinator must fill in the missing endpoint/model aliases,
allowed credential source and approved monetary cap. The actual private profiles
and request JSON can then be prepared and checked without making inference calls.
A separate explicit authorization is required for the later full-evening scope,
authenticated GitHub operations and queue actions. No authorization is implied by
this document or by a passing mock suite.
