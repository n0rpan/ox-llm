# Findings: fuzzing oxc (Task 61, part A)

All findings were reproduced on `origin/main` before the fix commit. The fix is a separate
commit (`fix(oxc): ...`) so the tests and the fix can be reviewed independently. The tests never
use a real API key or a real cloud endpoint; the SSE corpus is synthetic.

## F-1 — CRLF and lone-CR SSE streams are never translated (medium)

**Reproducer:** `corpus/response-crlf.sse` and
`test_openai_sse.ResponseStreamFraming.test_crlf_stream_is_delivered_before_eof` (and the
`/v1/completions` twin). Feeding a valid CRLF `data:` stream returns no events; `finish()` also
returns nothing.

**Root cause:** both `openai_responses.ResponseStream.feed` and
`openai_api.CompletionStream.feed` split the buffer only on `b"\n\n"`. The SSE grammar allows
CRLF, LF and lone CR line endings, and a CRLF stream contains `b"\r\n\r\n"`, so no frame is ever
recognised. A real OpenAI-compatible upstream (or a proxy that normalises line endings) can
therefore make `/v1/responses` and `/v1/completions` return an empty body/stream until the
connection closes, and lose every event.

**Fix:** a shared `openai_api.next_sse_frame()` finds the blank line for all three line-ending
forms and returns the frame as soon as it is complete; both translators use it. The
`event: error` / multi-line `data:` scan also normalises CR before splitting lines.

## F-2 — A malformed stream chunk raises out of the translator (medium)

**Reproducer:** `test_openai_sse.MalformedChunks.test_non_list_choices_are_ignored` and
`test_unhashable_tool_call_index_is_treated_as_missing`.

**Root cause:** `ResponseStream._chunk` iterates `value.get("choices") or ()` without checking
that it is a list, and `_tool_delta` uses the JSON `index` directly as a dictionary key. A
chunk such as `{"choices": 1}` (`TypeError: 'int' object is not iterable`) or a tool call with
`"index": []` (`TypeError: unhashable type: 'list'`) escapes `feed()` and unwinds the controller's
proxy thread. The engine is local, but it is still an untrusted byte source: a bug, a proxy, or a
malformed upstream must be a stream error, not an exception.

**Fix:** non-list `choices` and non-list `tool_calls` are ignored; a missing, non-integer or
negative tool index falls back to the next slot. The response stream still completes the items
that were already announced.

## F-3 — An unterminated SSE frame grows without bound (low, denial of service)

**Reproducer:** `test_unterminated_giant_frame_is_bounded`; it feeds a `data:` line one byte
larger than the limit and checks that the translator closes instead of retaining the buffer.

**Root cause:** neither translator caps its partial-frame buffer. An upstream that sends bytes
without a blank line (or a CRLF stream, see F-1) grows the controller's memory for as long as
the connection stays open.

**Fix:** `MAX_SSE_FRAME` (16 MiB) bounds the partial frame. `/v1/responses` emits an `error` +
`response.failed` event with code `stream_too_large`; `/v1/completions` emits one error frame and
`[DONE]`. A normal event is far below this limit.

## F-4 — A malformed `profiles` value crashes the model table (low)

**Reproducer:** `test_ledger_config.ConfigAliases.test_malformed_profiles_are_an_empty_table_not_a_crash`
(`"profiles": null`, `[]`, `3` or `"x"`). Before the fix `alias_table()` and `resolve_model()`
raise `TypeError`, and the request path's `Manager.config()` catches only `OSError`/`ValueError`.

**Root cause:** runtime.json is user-editable and the readers assume `profiles` is a dict.
The control routes happen to catch `TypeError` (they answer 503), but request admission calls
`Manager.config()` through a path that does not, so a malformed config can end the request with
an unhandled exception instead of a clear 503.

**Fix:** the pure alias table and `resolve_model()` treat a non-dict `profiles` as empty;
`Manager.config()` normalises it and converts the remaining type errors to `Unavailable`
("setup is still being prepared"), which the request path already handles.

## Not found in this pass

The hypothesis properties also exercise arbitrary JSON chunks and arbitrary byte streams. They
found no panic/hang outside the cases above in the time budget. The ledger reader skips torn
lines, JSON scalars and non-finite numbers; it produced no exception in the corpus test.
