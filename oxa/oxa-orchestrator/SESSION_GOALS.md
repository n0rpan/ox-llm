# Long-running project goals (#94, first priority)

The opt-in CLI can pursue a goal across multiple prompt turns in the SAME native
oxa RPC process and conversation:

```sh
./target/debug/oxa orchestrate --goal \
  --engine 127.0.0.1:PORT --model MODEL --local-only \
  --directory ./new-project-goal --role project-author \
  --timeout 3600 --worker-timeout 120 --reported-tokens 200000 \
  --goal-turns 128 --save-results \
  --source ./checkout --file src/main.rs --edit --permission-mode workspace \
  -- "Review src/main.rs, propose the fix, then verify the result"
```

Use only an explicitly verified local/nonbillable inference route. This command
is off by default and does not authorize a cloud call. The source-file scope and
private-copy proposal/review rules are unchanged: nothing applies to the original
checkout or merges automatically. Goal mode currently uses approved private copies,
not the separate Git-worktree backend.

Without `--goal`, existing single-turn fan-out behavior stays unchanged. Goal mode
owns one session (`--workers 1`, `--parallel 1`) and defaults its role to project
orchestrator. It starts the native process once, verifies its mode/model/offline
policy, then continues prompting without resetting conversation, workspace, output
or usage counters. Earlier assistant/tool messages remain context for later turns.
Retry and automatic compaction stay disabled in this bounded local profile.

## Goal state and budgets

A normal reply ends a turn, not the goal. The role contract requests one complete
JSON declaration after each prompt: continue with a summary, DONE with a summary
and nonempty evidence, or blocked with a specific reason. Invalid/unstructured
replies consume the same budget and get repair feedback before another turn; a
quoted DONE, missing evidence or extra authority fields never declares completion.
The original goal text remains unchanged in continuation prompts.

DONE and its evidence are agent-reported claims, not a proof of arbitrary project
correctness or permission to apply/merge output. The runtime also verifies its own
policy, accounting, owned process cleanup and private workspace collection. A
refused workspace review changes the result to blocked/policy failure.

The visible lifecycle is active, pausing, paused, DONE, blocked, budget exhausted
or canceled. Budget limits span the whole goal:

- `--timeout`: cumulative wall seconds (default 3600 in goal mode), including pauses
  and suspend, measured with the kernel boot clock; it never resets between turns.
- `--reported-tokens`: cumulative observed usage, including native tool-loop and
  steering requests, not a provider-side request cap or a hard monetary ceiling.
- `--goal-turns`: prompt-turn attempts, default 128, bounded to 1..10000.
- `--response-bytes`: cumulative accepted native RPC stdout/stderr in goal mode.
- `--worker-timeout`: an additional per-prompt-turn time cap (default 30 seconds).

An interrupted turn with unavailable usage remains unknown. A token-bounded goal
blocks rather than refunding uncertain usage or continuing with a fake zero.
Time-only library goals can retain that unknown total while continuing under their
original time limit. Expiry, invalid runtime output, policy changes or lost native
work never becomes successful DONE. Terminal elapsed time stops growing once the
runtime has settled.

## Control and events for the shared-session work

`goal_session::pursue` takes immutable native settings/worker grants, a goal budget,
a receiver of pause/resume/steer/cancel commands, an independent cancel
flag and a trusted nonblocking event consumer. Events carry current goal state,
revision, budget, usage and turn replies. Native steering is admitted while an
inference is running. Pause requests abort and stays visibly pausing/in-flight
until settlement; it does not claim that a request stopped merely because abort
was sent. Between settled turns pause/resume spends no new inference and keeps the
same native process/context. Controls use revisions, not an exclusive writer
identity, as the foundation for TUI and external clients sharing one owner.

A live control owner can hold the goal paused or blocked for operator input. With
no remaining control sender, the command returns its blocked/paused report and
stops/reaps its process. The CLI currently exposes status and Ctrl-C cancellation;
the opt-in [local socket API](SHARED_SESSION.md) now connects external clients to
this owner. The existing interactive TUI also has a default-off attachment to its
actual core session; see that guide for activation and limits. Background-task
wake-ups and goal-driven
worker-lane events follow that API. Context compaction/cloud reliability remains
separate work with the existing approval and main-lane coordination gates.

INT/TERM cancels owned work, joins/kills/reaps its process before output and keeps
exit 130/143. Status on stderr is nonblocking/best effort. The final terminal-safe
JSON report includes goal state, cumulative accounting, last reply, stop evidence
and optional private workspace review. Exit 0 requires DONE. Raw report persistence
still needs `--save-results`; otherwise it goes to stdout only. A private bounded
hash-only `goal.jsonl` audits turns and controls without raw goal/role/reply/steer
text. It is not yet a crash-resume journal or input for `orchestrate-inspect`.
Goal reports are native goal results, not the older A2A fan-in report schema.

The Rust goal ledger's `Goal::events(after, limit)` uses an inclusive cursor and
returns at most 64 events. Continue with `next_sequence`, immediately after the
last returned event. `latest_sequence` is the exclusive ledger high-water mark;
it can be ahead of a partially read page. `gap` means earlier events were evicted
from the 128-event history. A zero-sized or empty page leaves the cursor unchanged.
This ledger API is separate from the shared-session socket's `events` response,
whose existing `next` cursor uses the wire protocol described in SHARED_SESSION.md.

## Model-free verification

Real RPC fixtures read, edit and verify an approved private file over three goal
turns/six model requests, pause/resume between steps, assert later requests retain
earlier assistant context, verify only one process/policy initialization, preserve
cumulative usage, collect a valid proposal and leave the original file untouched.
Other fixtures pause active inference and resume the SAME process with uncertain
usage kept unknown, admit live steering, stop on cumulative time/token/turn caps,
report explicit blocking and refuse plain-text DONE as completion. The CLI smoke
case repairs an ordinary reply then completes in the same process and saves its
final report only under explicit consent. All inference is owned loopback fake
traffic; no installed model, provider key, production service or other machine is
used. Full-evening cloud orchestration acceptance is still outstanding.

## Observed token thresholds (coordinator decision)

Token budgets are observed thresholds. A request admitted below the threshold
may finish above it; another inference request is refused once the running
observed total is at or above the allowance. A valid final DONE at exactly the
threshold remains possible, but that does not permit another request. Actual
usage is retained without clamping it to the budget. This policy covers chat,
compaction and zero-output prefill, whose input still consumes tokens.

The TUI's in-process meter already gates each SDK admission. Native workers now
also enforce their immutable observed threshold inside their SDK adapter, rather
than depending on parent polling to notice an overrun before a tool loop starts
another request. Settlement and admission share a short lock; inference runs
outside it. Usage is published before a threshold refusal, so the parent can
attribute the refusal as budget exhaustion with the settled observation.

Goal snapshots and native worker reports expose `token_overrun`: zero for known
usage within the token allowance, a positive excess after a settled overrun, and
absent/null when cumulative usage is unknown or a goal has no token threshold.
An unsettled request is not converted to known usage. Fully settled overrun
usage remains known even when the runtime stops with `token_budget`.

Strict pre-request token ceilings wait for a trusted prompt bound; cloud
`count_prompt` is unavailable. Requested output alone cannot bound input plus
output. That follow-up is [issue 445](https://github.com/oxstack/ox-llm/issues/445).
The gateway's configured daily budget remains the hard monetary limit; token
thresholds are not a substitute for it. This decision does not authorize another
paid run or changes to the shared gateway.
