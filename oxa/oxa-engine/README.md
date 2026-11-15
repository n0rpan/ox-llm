# oxa-engine

HTTP + SSE client for the oxe engine and the pure request builder. Depends on `oxa-proto`. Used by `oxa-core` and `oxa-cli`; HTTP couples oxa to the local engine (O-135).

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | re-exports; `Engine`, `ChatRequest`, `Hints`, `EngineInfo` come from `oxa-proto` |
| `http.rs`, `sse.rs` | A1 | `Url`, `http_request`, `SseParser`, `post_sse` |
| `chat.rs`, `info.rs`, `hints.rs` | A2 | `build_body`, `ReplyAssembler`, `HttpEngine`, `build_headers`, `new_idem_key`, `fetch_info`, `count_tokens` |
| `embedded.rs` | #289 | `EmbeddedEngine`, the injected `EmbeddedGateway` interface |

Contracts
- `build_body` / `build_headers` are pure and are the only request builders. Body layout is fixed in the doc comment of `build_body`; W6 fields are always present.
- Liveness: comments count; one silence limit; no wall-clock limit.
- Retry only before the first data byte, same idempotency key. A cut reply is an `Err` and never reaches the wire (W2).
- Engine contract table (M1-M3, H1-H6): plan sec. 3. `oxa-fake` implements the same table.
- Comments (F2, S2 / F10): `: prefill <done>/<total> [<s>s]` -> `StreamEvent::Prefill` (the engine's seconds when given); `: queued [pos=<n>] wait_s=<s>` (key=value, unknown keys ignored) and the legacy `queued <n>` (position) / `queued <n>s` (seconds) -> `StreamEvent::Queued`.
- Errors (F3): a non-2xx reply and an in-stream error object (the controller's `event: error` after early headers, the engine's plain `data: {"error":..}`) share one mapping: `exceed_context_size_error` -> `context_overflow`; 503 (also type `local_backend_busy`) -> `engine_busy`, message `HTTP 503` plus `retry after <n> s` when a `Retry-After` header came (`SseEnd::retry_after`); other statuses -> `engine_http` naming `HTTP <status>`; an in-stream error without a status -> `stream_broken`. An error object after reply data is a cut reply (`stream_broken`). `HttpEngine::chat` never retries `engine_busy` itself (the core waits).
- Info (F4, F14): capabilities from `/_local/status` `model_limits[limits_for].capabilities` (before the model loads), else `/props` `oxe_hints`; a `/props` 503 is no error; a controller in `state: loading` without a context is `engine_busy`.
- `HttpEngine::clear_hint` (EG5): `POST /_local/cache/clear {"scope":"hint","hint":h}`, 404 -> `POST /state-cache/clear`; 409 -> `busy`; other non-2xx -> `engine_http` / `engine_busy`.
- Contract tests against the fake: `tests/fit_engine_contract.rs`.

`EmbeddedEngine::new(Arc<dyn EmbeddedGateway>)` explicitly opts into in-process
cloud chat. It shares the request builder, discovery parser and reply assembler
with HTTP. The binary supplies a gateway implementation that uses the same
admission, key, budget and ledger pipeline as the daemon. Neither the adapter
nor its caller owns provider credentials or implements those policies again.
No default engine selection changes in this slice.

Gateway completion includes ledger settlement; a Done event alone cannot make
an assistant turn succeed. Offline and pre-cancelled requests stop before any
gateway call. Mid-stream failures cannot produce a committed turn, and the
adapter never retries. Exact cloud tokenization is unsupported and prompt counts
remain client estimates. `tests/embedded_gateway.rs` exercises the socket-free
interface.

The default-off Cargo feature `in-process-gateway` adds
`EmbeddedEngine::from_gateway(Gateway) -> Result<EmbeddedEngine, OxaError>`.
Construct `GatewayRuntime::new(config, key_path, ledger_dir, GatewayOptions)`
from trusted user configuration, then pass it to `Gateway::new`. These types
are re-exported from `oxa-engine` for the binary's composition. The bridge owns
a private Tokio driver for the synchronous Engine API; callbacks run on the
calling thread, and cancellation also interrupts a provider waiting for bytes.
Async consumers use `Gateway::chat` directly. The default HTTP engine and
runtime selection remain unchanged; the CLI owns the separate user opt-in.
