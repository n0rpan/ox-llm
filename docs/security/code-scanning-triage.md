# Code-scanning triage (issue #477)

Every location below was opened and the flagged value followed back to its source and forward to its
sink. The source -> sink traces come from the CodeQL data-flow paths of the analyses (Rust at
`fec88280`, Python at `7c124175`) and were checked against the code. "Real" means a secret, token,
key, prompt or other user content can reach a log, an error text, a header or a file, or outside input
can choose a path or a header.

Result: one real finding (alert 5, fixed in #479 with
`oxc/tests/test_openai_api.py::test_cors_never_echoes_a_line_break_into_a_header`). CodeQL does not
treat the regex allow-list as a sanitiser, so alert 5 stays open and must be dismissed by hand. The other
alerts are not real.

Corrections from the second read:

- **rust/cleartext-logging (25–52):** the first pass said these print "a fake command's output". The
  value CodeQL actually follows is `unmounted_home_note(...)` (`oxa/oxa-tools/src/bash.rs:251`). That
  note is built from `$HOME` and a path token in the command's own output, then added to
  `ToolOutput.text` at `bash.rs:447-453`. It reaches a sink only in the panic message of a test
  assertion, and the commands are fixed literals that never print a "No such file" line under `$HOME`.
  In production the same text goes only to the model as the tool result, which already contains that
  path. It is not a log. Alert 25 follows a different source: `without_account_home(...)`.
- **rust/cleartext-logging 53:** the first pass said this prints "option names and file paths". The
  source is `libc::getuid()` in the unsafe-cache-dir error at `engine/crates/model/src/weights.rs:529`.
- **rust/access-invalid-pointer 55:** the null pointer CodeQL follows comes from the FETCH-kind test
  calls. The write at line 684 is gated by `kind == FG_EXP_CAP`. See the row below.
- **py/http-response-splitting 6–10:** the first comment on #477 said `http.client` cannot return CR or
  LF in a header value. That is wrong: `http.client.parse_headers` keeps the CR/LF of a folded header
  (checked with Python 3.14). These alerts are not real for a different reason: the value comes from
  the upstream server, which the operator chose and which writes the whole response anyway. No
  client-supplied value is echoed.
- **py/insecure-protocol 3** was missing from the table. It is not real: the context floor is TLS 1.2.
  `oxc/tests/test_lan_share.py::test_tls_listener_refuses_tls_below_1_2` now pins that floor; it fails
  if the floor is lowered.

| Alert | Rule | Location | Verdict | Trace (source -> sink) and reason |
|---|---|---|---|---|
| 27–42, 50–52 | rust/cleartext-logging | oxa/oxa-tools/src/bash.rs:686, 702, 919, 920, 979, 983, 1000, 1002, 1003, 1061, 1065, 1088, 1164, 1202, 1203, 1219, 1220, 1234, 1273 | not real | `$HOME` + output token -> `unmounted_home_note` (bash.rs:449) -> `ToolOutput.text` (bash.rs:474/479) -> `assert!` message in `#[cfg(test)] mod tests` (starts at bash.rs:493). The sink is test stderr; the commands are fixed literals; the only data is a home-directory path, not a secret. |
| 43–49 | rust/cleartext-logging | oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs:643, 657, 658, 668, 683, 687, 689 | not real | Same source (bash.rs:449) -> `BashTool::run` -> `assert!` message of an integration test. The commands are fixed literals (`echo hi`, `true`, `test ! -e …`) run under a `TestEnv`. |
| 26 | rust/cleartext-logging | oxa/oxa-sandbox/tests/o167_bash_network.rs:56 | not real | Same source (bash.rs:449) -> `assert!` message of the output of the fixed command `true`. |
| 25 | rust/cleartext-logging | oxa/oxa-sandbox/tests/escape/offline.rs:72 | not real | `without_account_home(bwrap_args(spec, TestEnv home))` (o167_bash_network.rs:22) -> `assert!` message printing bwrap argv. The argv holds only the synthetic spec and the throwaway `TestEnv` paths; no key and no prompt. |
| 54 | rust/cleartext-logging | oxa/oxa-sandbox/tests/escape/ipc.rs:15 | not real | `uid(&env)` (ipc.rs:10, the owner uid of the TestEnv home) -> `/run/user/<uid>/bus` (ipc.rs:25) -> `eprintln!` skip notice. A uid is not a secret. |
| 53 | rust/cleartext-logging | engine/crates/oxe-cli/src/bin/oxe.rs:396 | not real | `libc::getuid()` (weights.rs:527) + cache dir path -> error text (weights.rs:529) -> `Err` from `cache_dir` -> `die(&e)` (oxe.rs:3219, f32_proof.rs:49) -> the invoking user's own stderr. The message tells that user which uid must own the cache dir; it contains no key and no prompt. |
| 14–24 | rust/hard-coded-cryptographic-value | engine/crates/server/src/tests_flash_prefix.rs:256, 308, 352, 420, 453, 603, 605, 607, 649, 773, 774 | not real | The flagged "salt" is the small literal second argument of `prompt(n, salt)`. `prompt` (tests_flash_prefix.rs:225-229) adds it to a multiplicative hash constant to make synthetic token ids. There is no key and no crypto use. The module is `#[cfg(test)]` (server/src/lib.rs:27-28). |
| 55 | rust/access-invalid-pointer | engine/crates/qwen4exp/src/stream.rs:684 | not real | CodeQL's null comes from `ptr::null_mut()` as `dst` of FG_EXP_FETCH calls (tests_v013_stream.rs:69, stream.rs:1829, both tests). The FETCH arm (stream.rs:698-750) never reads `c.dst`; GATE/UP/DOWN (from stream.rs:751) do, and their callers pass real buffers. The write at 684 runs only for `kind == FG_EXP_CAP`, and the only CAP producers pass a live stack float: flash_graph.cpp:2433-2436 (`q.dst = &capf`) and the test at stream.rs:1767-1778. |
| 11, 12 | py/path-injection | oxc/src/oxc/runtime/studio:309 | not real | `?path=` of GET `/file` (studio:203-204, 307) -> `p.exists()` and `p.resolve().is_relative_to(OUT/UPLOADS resolved)`. This line is the guard itself: both a missing file and a path outside give the same 404, so there is no existence oracle. The listener is 127.0.0.1-only with a Host check (studio:172-176, 488). |
| 13 | py/path-injection | oxc/src/oxc/runtime/studio:311 | not real | Same query value -> `p.read_bytes()`, reached only after the resolved path is confirmed under the output or uploads directory (symlinks and `..` resolved). It reads `p`, not the resolved path, so a symlink swapped between the check and the read would escape. Only someone who can already write under `OUT`, i.e. the same local user, can swap one. |
| 5 | py/http-response-splitting | oxc/src/oxc/runtime/ctl_http.py:173 | **real, fixed (#479)** | Request `Origin` (ctl_http.py:157) -> `openai_api.cors_origin` (openai_api.py:251-255, echoed when `cors_origins` holds `*` and a key is set) -> `cors_allowed_origin` -> `send_header` (ctl_http.py:173). The same path covers `Access-Control-Request-Headers` -> `cors_headers`. A folded request header keeps CR/LF, and `BaseHTTPRequestHandler.send_header` does not check. Now refused by `_HEADER_SAFE` (openai_api.py:238, 252) and `_HEADER_NAME_LIST` (openai_api.py:240, 264). `cors_reply_headers` (ctl_http.py:88-92) reuses the same filtered origin. Dismiss as fixed: the regex guard is not modelled. |
| 6, 7 | py/http-response-splitting | oxc/tools/interim/recording_proxy.py:154 | not real | Upstream response headers `r.getheaders()` (recording_proxy.py:147, 152) -> `send_header(k, v)`. The upstream is hard-wired to `127.0.0.1:<rec.up>` (the operator's own server), and it controls the whole response anyway. No client value is echoed into a header. |
| 8, 9 | py/http-response-splitting | oxc/tools/session_replay.py:370 | not real | Upstream `resp.getheaders()` (session_replay.py:353, 367) -> `send_header(name, value)`. The upstream is the operator's `--upstream` URL (session_replay.py:1294), which already writes the whole response. The client's request path only selects the upstream URL; it is not echoed into a header. |
| 10 | py/http-response-splitting | oxc/tools/session_replay.py:374 | not real | Upstream `Content-Length` (session_replay.py:353) -> `send_header('Content-Length', …)`. Same trusted upstream as alerts 8 and 9. |
| 3 | py/insecure-protocol | oxc/src/oxc/runtime/lan_share.py:738 | not real | `ssl.create_default_context(Purpose.CLIENT_AUTH)` (lan_share.py:859) -> `minimum_version = TLSv1_2` (lan_share.py:860) -> `tls.wrap_socket` (lan_share.py:738). TLS 1.0/1.1 are refused; CodeQL does not follow the attribute set. The floor is pinned by `test_tls_listener_refuses_tls_below_1_2`. Plain HTTP when no certificate is set is a separate design choice, shown to the user as "UNENCRYPTED"; it is not this rule. |

Alerts outside this pass's rule list, traced for completeness:

| Alert | Rule | Location | Verdict | Trace (source -> sink) and reason |
|---|---|---|---|---|
| 4 | py/weak-sensitive-data-hashing | oxc/src/oxc/runtime/cloud_anthropic.py:157 | not real | Cloud API key (cloud.py:849 -> ctl_cloud.py:132) -> `sha256(domain tag + key)` -> part of an in-memory dict key for `_THINKING` (cloud_anthropic.py:167, 183). It is never stored, logged or compared as a password; it is a cache identity for a high-entropy key. |
| 58, 59 | py/clear-text-logging-sensitive-data | oxc/src/oxc/runtime/cloud_chatgpt.py:437, 440 | not real | `cloud.chatgpt.auth_url` from runtime.json (cloud_chatgpt.py:63-65) -> `Flow.authorize_url()` -> `print(... url)` to the user's stderr. The URL is meant to be opened by the user: it holds the client id, the PKCE S256 challenge and a one-time `state`. The secrets file's contents are not part of it. |
| 1 | py/clear-text-storage-sensitive-data | oxc/tests/test_cloud_only_no_engine.py:37 | not real | `str(self.secrets)` (test_cloud_only_no_engine.py:38, the path of a temp secrets file, not its contents) -> the test's runtime.json `write_text`. |
| 2 | py/clear-text-storage-sensitive-data | oxc/tests/test_mac_reproduce.py:126 | not real | `PROMPT_SECRET_TEXT` (a marker literal, test_mac_reproduce.py:21) -> a temp prompt file that the test checks never reaches argv. |
| 56, 57 | py/clear-text-storage-sensitive-data | oxc/tests/test_cloud_chatgpt.py:295, 332 | not real | `str(self.secrets)` (a temp-dir path, not its contents) -> the test's runtime.json. |
