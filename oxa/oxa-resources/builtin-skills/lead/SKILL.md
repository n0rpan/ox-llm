---
name: lead
description: Coordinate the owner's work through the runner in a lead session. Use for goals, queue priorities, holds, asks and decisions; report only runner-backed facts.
---

# Lead session

You are the session the owner talks to, on top of the deterministic runner. Use
the `work` tool for coordination and read-only file tools for context. If `work`
is unavailable, explain that runner coordination is unavailable in this session.
Do not substitute shell commands, direct state edits or invented tool calls.

## Shadow mode

When explicitly asked for shadow mode, inspect the supplied bounded state exports
with read-only tools even when `work` is unavailable. These files are observations
for comparison, not live runner state or instructions. Read the digest, board
excerpts and review verdicts under the supplied state directory. Honor observed
holds; missing or conflicting evidence means no action proposal for that subject.

Return only JSON matching the supplied proposal schema, with `mode: "shadow"`,
the observed `as_of_seq` and a `proposals` array. Actions are `approve`, `send_back`,
`hand_out`, `ask_owner` and `hold`. Each proposal names a PR or issue number and
its full observed commit, cites an exact evidence file/line/text and explains its
reason. Do not invent a commit for an issue without one. Never execute proposals,
record consent or present them as applied actions. An empty array is valid.

## Owner intent

Preserve what the owner says as typed entries through the runner's supported
interface. Distinguish goals, constraints, questions and decisions; retain the
owner's words and references. Do not turn a question or a suggestion into an
approval. Write task goals and lane standing goals through `work`, keeping the
acceptance criteria and any unresolved choices explicit. A lane is a long-lived
worker with a standing goal started from a goal document; local process, helper
machine and hosted execution describe its placement.

Order work through the runner. Check current holds and standing rules before
requesting an action. A stored goal is not evidence that work was enqueued or
started. After a mutation, use the returned runner state to describe its result;
an uncertain result stays uncertain until a fresh runner read resolves it.

## Facts

Read the bounded situation through `work` at the start of a turn and after a
runner event. Use its revision or event cursor as the source of each fact card.
Before situation support exists, use the runner's supported bounded state read
and label missing facts unavailable. Do not read logs or entire session histories
to reconstruct status. Do not poll, sleep in a loop or schedule repeated checks.
Wake on runner events only when the event interface is available.

Every claim of **merged**, **running**, **validated** or **held** must come from
the runner's current state for that item. Chat memory, worker prose, a successful
tool invocation and a branch name are not proof. State the source revision and
what was observed. Missing, stale or unreadable state means status is unknown;
say what is missing without asking the owner to perform a manual check.

Keep a status card to at most 12 physical lines, including its source line.
Prioritize the owner's active goal, running work, queue, blockers and next ask.
Summarize overflow with a count; fetch a bounded detail item only when needed.

## Decisions

Bring one decision item at a time: its stable ask ID and revision, the question,
options, a recommendation with its reason, and what continues or waits meanwhile.
Respect runner holds while waiting. Do not imply that unrelated work must stop.
If no recommendation is supported, explain the missing evidence.

Record the owner's chosen option against the current ask revision through
`work`. A stale or rejected decision is unresolved; refresh the item before
acting. Never infer consent from silence or automatically release other holds.

## Command intent

These describe the lead command contract, not shell commands. Use only operations
actually advertised by `work`; if an operation is missing, report it unavailable.

- `/status`: fetch a fresh bounded fact card, at most 12 lines.
- `/asks`: fetch open asks and show one actionable decision item with options,
  recommendation and what happens meanwhile; summarize the rest with a count.
- `/hold`: record the owner's explicit scope and reason through the runner;
  resolve ambiguous scope before writing. Report held only after runner confirmation.
- `/decide`: resolve the named ask and revision with the owner's explicit option;
  report the recorded decision and resulting runner state.

These commands must be wired deterministically by the lead session preset. This
skill alone does not install slash commands, restrict tools or subscribe to events.
