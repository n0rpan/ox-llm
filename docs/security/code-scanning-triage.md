# Code-scanning triage (issue #477, second read)

Every location below was opened and the flagged value followed to its source. "Real" means a secret,
token, key, prompt or user content can reach a log, an error text or a header, or attacker-controlled
input reaches a path or header.

Result: one real finding (alert 5, fixed with a test in `oxc/tests/test_openai_api.py`,
`test_cors_never_echoes_a_line_break_into_a_header`); the rest are not real.

| Alert | Rule | Location | Verdict | Reason |
|---|---|---|---|---|
| 27 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:919 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 28 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:920 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 29 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:979 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 30 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:983 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 31 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1000 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 32 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1002 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 33 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1003 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 34 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1061 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 35 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1065 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 36 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1088 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 37 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1202 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 38 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1203 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 39 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1219 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 40 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1220 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 41 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1234 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 42 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1273 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 50 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:686 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 51 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:702 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 52 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:1164 | not real | Assertion message inside `#[cfg(test)] mod tests`; prints a fake/fixture command's tool output (`out.text`), no secret source. |
| 25 | rust/cleartext-logging | oxa/oxa-sandbox/tests/escape/offline.rs:72 | not real | Test assertion printing bwrap argv built from a synthetic spec in a throwaway HOME. |
| 26 | rust/cleartext-logging | oxa/oxa-sandbox/tests/o167_bash_network.rs:56 | not real | Test assertion printing output of the fixed command `true`. |
| 43 | rust/cleartext-logging | oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs:643 | not real | Test assertion printing output of fixed literal test commands (echo/test). |
| 44 | rust/cleartext-logging | oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs:657 | not real | Test assertion printing output of fixed literal test commands (echo/test). |
| 45 | rust/cleartext-logging | oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs:658 | not real | Test assertion printing output of fixed literal test commands (echo/test). |
| 46 | rust/cleartext-logging | oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs:668 | not real | Test assertion printing output of fixed literal test commands (echo/test). |
| 47 | rust/cleartext-logging | oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs:683 | not real | Test assertion printing output of fixed literal test commands (echo/test). |
| 48 | rust/cleartext-logging | oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs:687 | not real | Test assertion printing output of fixed literal test commands (echo/test). |
| 49 | rust/cleartext-logging | oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs:689 | not real | Test assertion printing output of fixed literal test commands (echo/test). |
| 53 | rust/cleartext-logging | engine/crates/oxe-cli/src/bin/oxe.rs:396 | not real | `die()` prints CLI usage/parse/IO errors (option names, file paths, non-numeric token-id fragments of the user's own input file) to the user's own stderr; no key or prompt text. |
| 54 | rust/cleartext-logging | oxa/oxa-sandbox/tests/escape/ipc.rs:15 | not real | Skip notice printing a socket path like /run/user/<uid>/bus; uid is not a secret. |
| 14 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:256 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 15 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:308 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 16 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:352 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 17 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:420 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 18 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:453 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 19 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:603 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 20 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:605 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 21 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:607 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 22 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:649 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 23 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:773 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 24 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:774 | not real | Test-only module; `prompt(n, salt)` builds synthetic token ids from a multiplicative hash constant; no key, no crypto use. |
| 55 | rust/access-invalid-pointer | engine/crates/qwen4exp/src/stream.rs:684 | not real | Write through `c.dst` in an `unsafe fn` whose contract is a valid call record from the ggml graph custom op; the pointer is the op's output tensor, not external input. |
| 11 | py/path-injection | oxc/src/oxc/runtime/studio:309 | not real | `p.exists()` on the query path only gates a 404; content is served only after the resolve check below. |
| 12 | py/path-injection | oxc/src/oxc/runtime/studio:309 | not real | Same line: `p.resolve().is_relative_to(OUT/UPLOADS)` is the guard itself (symlinks and `..` resolved). |
| 13 | py/path-injection | oxc/src/oxc/runtime/studio:311 | not real | `read_bytes()` only after the resolved path is confirmed under the output or uploads directory. |
| 5 | py/http-response-splitting | oxc/src/oxc/runtime/ctl_http.py:173 | **real, fixed** | CORS preflight echoed the client's `Access-Control-Request-Headers` (and, with `*`, `Origin`); a folded request header keeps CR/LF in its value and `http.server.send_header` does not reject it. `openai_api.cors_origin` now refuses non-printable origins and `cors_headers` falls back to the default list unless the value is a plain header-name list. |
| 6 | py/http-response-splitting | oxc/tools/interim/recording_proxy.py:154 | not real | Dev recording proxy relays response headers from the local upstream engine at 127.0.0.1 (trusted), not from the client. |
| 7 | py/http-response-splitting | oxc/tools/interim/recording_proxy.py:154 | not real | Same line, header value from the same trusted upstream. |
| 8 | py/http-response-splitting | oxc/tools/session_replay.py:370 | not real | Dev replay tool relays headers of the trusted upstream response; client headers are not echoed. |
| 9 | py/http-response-splitting | oxc/tools/session_replay.py:370 | not real | Same line, header value from the trusted upstream. |
| 10 | py/http-response-splitting | oxc/tools/session_replay.py:374 | not real | Content-Length copied from the trusted upstream response. |
