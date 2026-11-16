# Lead session integration

The lead is an ordinary oxa session with the built-in `lead` skill. The skill is
available now through `/skill:lead`; it does not enable runner access by itself.
`oxa lead`, its restricted session preset, the `work` tool and the commands below
are pending integration with the runner. Do not use this document as a list of
commands supported by the current binary.

The preset must work with both the TUI and print driver, using the same session
store, engine selection, turn loop and result contract as ordinary oxa. It adds
`work` and permits only `read`, `grep`, `find` and `ls` alongside it. Restrict the
effective tool registry, including tools supplied by hooks, extensions, MCP and
codemode; merely setting a default tool list is insufficient. Keep the lead
instructions in the system prefix on continuation and reopen. The skill text is
guidance, not the enforcement mechanism.

## Runner boundary

The runner owns task state and action validation. The lead adapter calls its
typed library interface; it must not execute a model-provided shell string,
scrape CLI output, invent a second state schema, or infer facts from worker
prose. Until the runner lands, this boundary remains unwired. In particular,
runner `done` does not by itself establish `merged` or `validated`.

The adapter needs bounded reads for status and asks, typed owner entries, goal
document writes, and runner actions for ordering work, holds and decisions.
Writes return committed state or an explicit uncertain/error result. Owner
entries preserve the original words and distinguish goals, constraints,
questions and decisions. Holds and decisions carry scope and the current
standing/ask revision. Only the runner may decide whether the action is allowed.

The four commands must dispatch directly to the same adapter in both drivers,
without a model turn for routing:

| Command | Runner operation | Response |
| --- | --- | --- |
| `/status` | Fresh bounded state read | Fact card of at most 12 physical lines, source included |
| `/asks` | Bounded open-ask read | One item with options, recommendation, meanwhile; remaining count |
| `/hold` | Revisioned hold write | Confirmed scope/reason and resulting state, or explicit failure |
| `/decide` | Revisioned decision write | Recorded option and resulting state, or stale-ask response |

Ambiguous scope or missing choice produces a clarification without mutation.
Stale revisions require a fresh read; they must not silently overwrite newer
owner intent. Escape newlines in individual fact fields before applying the
12-line cap, and summarize overflow rather than dropping the source reference.
Unreadable or stale facts remain unknown.

## Event integration after L1

Consume the runner's sequenced events and bounded situation file after L1 is on
main. Persist the last processed cursor with the lead session; coalesce events
arriving during a turn and reread the situation at the next safe turn boundary.
Reconnect using the runner's supported cursor/recovery contract. Do not add a
timer that repeatedly asks the model for status, and do not tail logs to recover
facts. A missing event interface must remain explicitly unavailable.

## Acceptance still required

Use a fake engine and temporary runner state to verify restricted tools across
open/reopen, typed owner entries, action rejection under holds, stale decisions,
missing/corrupt state, and multiline/overflow fact cards. Drive the real TUI in
tmux and the print driver against the same runner fixture. After L1, verify
event cursor recovery and coalescing without polling. Final acceptance is an
owner evening on the subscription model with one helper machine and the queue,
without asking the owner to perform a manual status check. This requires the
owner's environment and is not established by fixture tests.
