# Fake-script format (W0-FROZEN)

A script drives `oxa-fake` (`--script <file>`) and `oxa_fake::ScriptedEngine`. It is JSON Lines: one turn object per line, consumed in order by chat requests with `max_tokens > 0`. Blank lines and lines starting with `#` are skipped. A `max_tokens: 0` request (prefill, H6) consumes no turn.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `text` | string | absent | `content`, streamed exactly as written (W2: never trimmed) |
| `reasoning` | string | absent | `reasoning_content`, streamed before `text` |
| `tool_calls` | array of `{"id","name","args"}` | `[]` | `args` is a STRING sent verbatim as the arguments (it may be invalid JSON on purpose) |
| `finish` | `"stop"` \| `"tool_calls"` \| `"length"` | `"tool_calls"` if there are tool calls, else `"stop"` | `finish_reason` |
| `completion_tokens` | number | canonical reply bytes + 1 | reported `usage.completion_tokens` |
| `chunk` | number | 0 | delta size in bytes (0 = one delta per field); never splits a UTF-8 sequence |
| `fault` | object | absent | one of the faults below |

Faults (`"fault": {"kind": ..}`):

| `kind` | Other keys | Effect |
|---|---|---|
| `http` | `status`, `body` | answer with that status and body, no stream |
| `overflow` | | 400 `exceed_context_size_error` |
| `drop_at` | `bytes` | close the connection after that many SSE body bytes |
| `refuse` | | refuse this connection |
| `hold_headers` | `ms` | wait before sending headers, then reply normally |
| `comments_only` | `ms` | send `: queued wait_s=<s>` once per second for that long, then reply normally |
| `early_close` | | close the socket right after `[DONE]` |
| `queued` | `count` (1), `pos` (true) | send `count` queued comments (10 ms apart) before the reply, in the unified format (S2 / F10): `: queued pos=<n> wait_s=<i>` with positions counting down to 1 (the controller) when `pos`, else `: queued wait_s=<i>` (the engine). The scripted engine emits the same `StreamEvent::Queued`s |
| `stream_error` | `status` (required), `queued` (1), `type` (per status), `event` (true) | F3: `200` + SSE head, `queued` controller comments, then the error object `{"message","type","code":<status>}` and the end of the chunked body without `[DONE]`. `event: true` = the controller's form (`event: error` + `data:`), `false` = the engine's plain `data: {"error":..}`. `type` defaults to the OpenAI type of the status (`invalid_request_error` 400, `unavailable_error` 503, `server_error` 500, ...); `exceed_context_size_error` makes it an in-stream overflow |
| `busy` | `retry_after` (30; `null` = no header) | F3 / S2: the controller's queue-full reply: HTTP 503, type `local_backend_busy`, code `queue_full`, `Retry-After: <s>` |
| `snap_null` | `reason` (`no_boundary_in_last_message`) | H4: the reply carries `timings.snap_pos: null` and `snap_reason` (only when `X-Oxe-Snapshot: keep` was sent) |
| `stall` | `bytes`, `ms` (both required) | server only: send `bytes` bytes of the SSE body, then nothing at all (no data, no comment) for `ms`, then the rest of the reply; a client idle limit below `ms` sees a silent stream (SET-051) |
| `cold` | | serve cold (both engines): the engine holds nothing for the session when this request arrives (a server without a prefix cache, an evicted or restarted state): `cached_tokens: 0`; afterwards it holds this prompt + reply as usual |

Example:

```
# turn 1: read a file
{"reasoning":"look first","tool_calls":[{"id":"call_1","name":"read","args":"{\"path\": \"src/main.rs\"}"}]}
# turn 2: answer
{"text":"Done.\n"}
# turn 3: the stream drops after 40 bytes
{"text":"this reply is cut and must never reach the wire","fault":{"kind":"drop_at","bytes":40}}
```

Token model: one token = one byte of `oxa_fake::cache_bytes(system, tools_json, messages)`; `cached_tokens` = common prefix with what the fake holds for that session: the last prompt plus its reply minus the reply's last token (F1: the engine never feeds the last generated token), so a perfect continuation reports `prompt + completion - 1`; after an H6 prefill the fake holds exactly the prompt. Prefix cache: the anchor (system text, `\0`, tools JSON, `\0`) is shared across sessions, so a request whose session holds nothing (a fork, a new session) still reports at least the anchor when any session holds the same anchor; the `cold` fault and `restart()` drop that too.

Engine parity (both fakes):

- Overflow (F6): 400 `exceed_context_size_error` only when `prompt >= n_ctx`. Otherwise the reply is bounded by `max_tokens` and by the room left (`n_ctx - prompt`); a reply that does not fit is cut there (reasoning, then text, one byte per token; tool calls are dropped) with `finish_reason: "length"` and `completion_tokens` = the bound.
- H6 (`max_tokens: 0`, F7): `: prefill N/N 0s`, the role chunk, the finish chunk with `"length"`, a separate usage chunk (`choices: []`) with `timings`, `[DONE]`; no content delta.
- Comments (F2 / F10): `: prefill <done>/<total> <s>s` and `: queued [pos=<n>] wait_s=<s>`, the engine's and the controller's exact forms.
- H4: `timings.snap_pos` only when `X-Oxe-Snapshot: keep` was sent: the end of the last message (the prompt end; the fake renders no generation opener), or `null` + `snap_reason` with `snap_null`.
- F9 (both fakes): a generating request whose last message is an assistant message is history; a NEW turn is generated (it consumes a script turn). Only an explicit in-place continuation (`"add_generation_prompt": false` with `max_tokens > 0`, HTTP fake) is a 400 (`oxa_fake::CONTINUATION_400`). H6 is unchanged.
- Models (HTTP fake): `GET /v1/models` lists every `model_limits` row (`FakeOpts::models`, or `model` alone); a request is bounded by the `cap` of the row it names (else `n_ctx`).
- `POST /apply-template` (HTTP fake): the chat body rendered by the fake's template (`cache_bytes`) as `prompt`; with `"return_tokens": true` also `tokens` (one per byte, so its length equals the `prompt_tokens` the same chat request reports, framing included) and `generation_prompt` (`""`).
- Timing (HTTP fake): `FakeServer::max_in_flight()`, `in_flight()` and `timings()` (one `ChatTiming` per chat request: seq, `max_tokens`, start/end ms, `client_closed`) observe overlap and cancels (ENG-001 / ENG-002). `--hold-prefill <ms>` (`FakeOpts::hold_prefill_ms`) holds every H6 request's headers that long; a client that closes meanwhile ends it at once.
- Today's engine after a rewrite (HTTP fake, OXE-POSTCOMPACT-MISS): `--rewrite-resume-block <n>` (`FakeOpts::rewrite_resume_block`): a request that rewrites what the session holds resumes at the deepest `n`-token block of the shared prefix, and so does the session's NEXT request even when it is a pure append. Off by default (the correct engine continues the live state).
- F13 (HTTP fake): a request that re-sends an assistant tool call whose `arguments` is not valid JSON is a 400 (`Failed to parse tool call arguments as JSON`), as on the engine; `FakeOpts::strict_tool_args = false` turns it off. A script may still produce such a call (the reply is streamed verbatim); it is the NEXT request carrying it that the engine refuses.
