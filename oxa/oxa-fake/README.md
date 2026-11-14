# oxa-fake (test-only)

Fake engine, scripted in-process engine, W1-W6 checker, golden hashes, test isolation. Depends on `oxa-proto` only; every other crate has it as a dev-dependency.

| Module | Task | Provides |
|---|---|---|
| `lib.rs`, `turn.rs` | architect (frozen) | `ScriptTurn`, `Fault`, `Recorded`, `cache_bytes` |
| `scripted.rs` | architect (implemented in W0) | `ScriptedEngine` |
| `testenv.rs` | architect (implemented in W0) | `TestEnv` |
| `server.rs`, `script.rs`, `record.rs` | A14 | `FakeServer`, `oxa-fake` binary, `parse_script`, `parse_recorded` |
| `check.rs`, `golden.rs` | B15 | `check_all` and per-rule checks, `request_hashes`, `compare_golden` |

Cache model: one token = one byte of `cache_bytes`; see `src/lib.rs`. Script format: `docs/fake-script.md`. The same suite later runs against `oxe-server --engine stub` (plan sec. 6.4).

Engine parity (fit pass, `tests/fit_fake_parity.rs`): the held state drops the reply's last token (F1); H6 ends with `length` and the engine's chunk layout (F7); overflow only when `prompt >= n_ctx`, else the reply is cut with `length` (F6); `: prefill N/N 0s` and the unified `: queued [pos=<n>] wait_s=<s>` (F2 / F10); `snap_pos` only with the H4 hint (`snap_null` fault for a null snapshot); re-sent tool-call arguments must be JSON (F13, `strict_tool_args`).

Controller and test API beyond `requests()`:

| Item | What |
|---|---|
| `FakeOpts::models: Vec<FakeModel>` | rows of `/_local/status` `model_limits` with their `capabilities` (`FakeModel::moe(alias, cap)`, `FakeModel::flash(alias, cap, max_output)`; `FakeCaps::{rust_moe, rust_flash, none}`). Empty = one row for `model` with the Rust MoE block when `hints` (no block without hints) |
| `FakeOpts::props_unloaded`, `FakeServer::set_props_unloaded(bool)` | `/props` answers 503 "no model loaded" |
| `FakeOpts::loading`, `FakeServer::set_loading(bool)` | `/_local/status` is the controller's `state: loading` body (no model, no context; F14) |
| `FakeOpts::controller` (default `true`) | `false` = a plain engine: `/_local/*` answer 404 |
| `FakeOpts::strict_tool_args` (default `true`) | F13 check |
| `POST /_local/cache/clear`, `POST /state-cache/clear` | EG5 `{"scope":"hint","hint":h}`: drops the held state of session `h` only; hint 1-128 of `[A-Za-z0-9._-]` else 400, missing 400, unknown 200 `sessions: 0`; 409 while `set_clear_busy(true)` |
| `FakeServer::clears() -> Vec<String>`, `ScriptedEngine::clears()` | the hints of the accepted clears (`ScriptedEngine` implements `Engine::clear_hint`) |
| `FakeServer::held(session)`, `ScriptedEngine::held(session)` | the cache-model bytes held for a session |
| `Recorded::preserve_thinking: Option<bool>` | `chat_template_kwargs.preserve_thinking` as sent (`None` = absent; F8) |
| `Fault::{Queued, StreamError, Busy, SnapNull}` | script kinds `queued`, `stream_error`, `busy`, `snap_null` (`docs/fake-script.md`) |
