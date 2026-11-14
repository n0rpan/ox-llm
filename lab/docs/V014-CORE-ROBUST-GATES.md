# v0.1.4 core-robust: gates G-Q (queue) and G-R (replay)

Pre-registered before any hardware run. Each feature ships default off; it turns on by default only
when its gate passes. A killed gate ships that feature off (v0.1.3 queue, no replay) and blocks
nothing else. Budget: 12 minutes of hardware each. Rehearse each adapter on ci-x64 against the stub
engine first, including the negative control named below.

Flags (controller environment, a systemd drop-in, restart the controller):
`LOCAL_AI_QUEUE=1` (ctl_queue), `LOCAL_AI_REPLAY=1` (ctl_replay) (both on by default since v0.2.0, A5;
`=0` turns each off, which the negative controls below use), `LOCAL_AI_TURN_DIAG=1` (ctl_diag,
labels only; on for both gates so their rows carry `rewrite_class`, `history_rewrite`, `fold_suspect`).

Common setup: the daily model on the 32K rung, fast mode, `stream: true`, `temperature: 0`, the
client-free driver (`tools/closed_loop_session.py`, `--compaction off`) with a fixed `--seed`, an
explicit `--session-id` per session, and the controller's request ledger read after the run. The
adapter records per request: the send time, the time of the response head, the arrival time of
every SSE comment line (`: queued pos=<n> wait_s=<s>`), the first data byte, the raw body bytes (kept in RAM,
hashed with SHA-256 for the receipt), the status and any retry. Receipts hold hashes, sizes,
times and enum labels only, never text.

## G-Q: queue with early headers (12 min)

| Step | What | Min |
|---|---|---|
| 1 | Control: session A alone, 4 turns grown to the 32K rung (`--session-id gq-a-ctl`) | 4 |
| 2 | Restart the controller (cold KV for A as in step 1). Session A again (`gq-a-run`, same seed), and session B fresh (`gq-b-run`, other seed, ~3K, 4 turns) started while A's turn 2 decodes, each next B turn sent while A runs | 6 |
| 3 | Read the ledger; compare A's per-turn body hashes with step 1 | 2 |

Pass (all must hold):
- every request 200; no 503, no 429, no client retry;
- every request that found the slot busy: response head <= 1 s after send; `: queued pos=<n> wait_s=<s>` gaps
  <= 20 s; the first data byte comes after the last comment, and the body after the comments
  equals what the engine sent (a complete stream ending `data: [DONE]`);
- every request that found the slot free: no comment line at all (the v0.1.3 bytes);
- A's turns 2-4 and B's turns 2-4: ledger `miss_class` `append-ok`, `history_rewrite` false;
- A's body hash of every turn equals the control's (byte-identical output with B interleaved);
- queued rows carry `queue_pos`; the first row after a switch of lanes carries `session_switch`.

Kill: any output difference against the control, any miss on an append turn, any 5xx, any 429.

Arm G-Q-EG1 (v0.2.0, background pre-emption, +3 min, same setup): session C (`gq-c-bg`, ~8K new
tokens, `X-Oxe-Priority: background`, `Idempotency-Key`) is sent; 2 s later (C still in prefill)
session A sends one interactive turn. Pass: A's response head <= 1 s and its first data byte within
one prefill chunk of C's cut; C's ledger row carries `preempted: 1`; C's stream holds SSE comments
only before its first data byte and its body hash equals a control run of C alone; the engine log
shows C's first request ended as a client cancel and exactly one completed C request. A second probe
sends A after C's first data byte: no `preempted`, C completes first.

Negative control (ci-x64, stub engine): the same adapter with `LOCAL_AI_QUEUE=0` must report the
busy request's response head only at admission (the head-time check must fail), and with the cap
lowered to 1 waiter a third request must be refused with 503 and `Retry-After: 30`.

## G-R: replay after a cut (12 min)

| Step | What | Min |
|---|---|---|
| 1 | Session `gr-1` (fresh seed) grown to the 32K rung; send turn N, close the socket 1 s after send (during prefill, before any data byte); within 5 s send the identical raw body again with the same session id and read it to the end | 4 |
| 2 | Turn N+1 of `gr-1` as an ordinary append (the C3 check) | 1 |
| 3 | Session `gr-2`: the same at turn N, but the cut comes after 20 data events (mid-decode); retry within 5 s; then turn N+1 | 4 |
| 4 | Control: replay off (`LOCAL_AI_REPLAY=0`), a cut and retry as in step 3 on `gr-3`; cross-session probe: during a cut's grace send its body with another session id | 3 |

Pass (all must hold):
- steps 1 and 3: the retry's ledger row says `replay: attached` and the producer's row
  `replay: cut-served`; the engine shows one prefill for turn N (one engine request, its
  `prompt_n` once; no second engine request for the retry);
- the bytes the cut client got are a prefix of the retry's body; the retry's body is a complete
  stream ending `data: [DONE]`; the retry's body equals the engine's one stream (the bytes the
  controller relayed, SHA-256 of the producer's recorded body = SHA-256 of the retry's body);
- turn N+1 after each: `miss_class` `append-ok`;
- the control (step 4) does re-prefill (two engine requests): it shows the adapter can tell the
  difference; the cross-session probe is an ordinary request (no `attached` row, its own engine
  request).

Kill: any byte difference between the cut prefix, the replay and the engine's stream; any attach
across sessions or principals; more than one prefill on an attached turn.

Negative control (ci-x64, stub engine): a retry with one changed body byte (and, separately, the
same `Idempotency-Key` with a different body) must not attach.

Limits that are part of the design, not gate findings: replay only for local, streaming,
temperature-0 requests with a session key; 15 s grace from the cut; at most 8 MiB per entry,
4 entries, RAM only; a request of the same session that is not identical cancels the cut entry.
