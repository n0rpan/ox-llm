# RECEIPT — #2255 test-only evidence branch (assembled)

Durable receipt, committed as the branch tip. It maps the exact head to the hosted
checks the lead must run and report. No product change, no speculative fix, no
changelog claim of a bug fix, no real prompts, credentials or session data.

## Branch and exact heads

- Branch: `agent/assemble-2255-evidence` (local; the lead publishes).
- Base: `e62da190f9c85c6aa02f3dac0e519b67ca8ab5bc` (`origin/main` at assembly, fetched).
- Code head (all test changes): `5a95e758091e956d3be571b09f9db342c8b9c8fa`.
- Exact head for hosted CI: the branch tip, the receipt-only commit that adds this
  file as a child of the code head (`git rev-parse agent/assemble-2255-evidence`).
  It changes no code; CI at the tip covers the code head byte-for-byte.

Commits on top of the base, in order:

1. `34ece0854ddcf74bacb253213c5f8c697f995333` — cherry-pick of
   `e3f77e694a5508a8ce5cf28717dd9fea90a2b131` (Responses gateway fixture,
   probe-2255-prefix). Changed file byte-identical to the source commit.
2. `5a95e758091e956d3be571b09f9db342c8b9c8fa` — cherry-pick of
   `d75cbcca01b537e69907edb69f8d1d54c298bfd6` (local stub engine fixture,
   probe-2255-local). Changed files byte-identical to the source commit.
3. The receipt commit (this file only).

Both cherry-picks applied cleanly with no conflict and no edit.

## Scope (base...code head)

- `oxa/oxc-gateway/tests/openai_client.rs` (+240): two `#[tokio::test]` fixtures,
  each run in the API-key and ChatGPT-subscription routes against the existing
  loopback Responses mock. The file is gated on the `upstream-openai`,
  `anthropic-client` and `ledger` features, as before.
- `engine/crates/server/src/tests_echo.rs` (+262/−2): two
  `#[tokio::test(flavor = "multi_thread")]` continuity fixtures on the stub engine.
  The module is `#[cfg(test)]` (engine/crates/server/src/lib.rs). The −2 is the
  existing `st()` helper extracted into `st_slots()`; `st()` still passes 4 slots,
  so every existing caller is unchanged.
- `changelog.d/prompt-cache-continuity-fixture.md` (+3): describes the fixture only.
  It claims no bug fix. Name and content satisfy `tools/ci/changelog.py`
  (lowercase-hyphen name, non-empty), checked statically.

No runtime code, engine numerics, defaults, thresholds, workflows or CI runner
files are touched. All prompts, replies, keys and tokens in the fixtures are
synthetic; assertion messages print indices, field names or synthetic stub JSON,
never real transcripts.

## Fixture assumptions checked (static, against the base)

Gateway fixture:

- The six source files it builds on (`tests/openai_client.rs`,
  `tests/common/openai.rs`, `src/openai/request.rs`, `src/openai/client.rs`,
  `src/openai/chatgpt_request.rs`, `src/gateway/openai.rs`, all under
  `oxa/oxc-gateway/`) are byte-identical between the original base `f4b0034c` and
  the new base `e62da190`, so the rebase changes nothing under the fixture.
- Helpers exist with matching signatures: `MockOpenAi::start(Vec<Reply>)`,
  `Reply::events`, `wait_for_requests`, `assert_healthy`, `Request { body, target }`,
  `Fixture::new/token_path/key`, `chat(base, payload, replay)`, `subscription_auth`,
  `request::ReplayItems::new`, `completed.assistant/replay_items`.
- `tool_turn()` yields 4 replay items (1 reasoning + 3 function_call) with
  `encrypted_content: "opaque-synthetic-reasoning"`; a message-only
  `response.completed` (the `answer()` shape the new `foreground_answer` mirrors)
  yields 1 replay item, as existing tests assert. The subscription base URL
  construction matches the existing both-modes tests.

Local fixture:

- `handle_hints(st, method, path, query, body, session, turn_kind, ephemeral, hints)`
  and `Hints { snapshot_keep }` exist in `service.rs` with the exact argument order
  used. `body`, `kv`, `SYS`, `TOOLS`, `synth_tokenizer` exist in `tests_echo.rs`;
  `tokenizer::Token` is `u32`; the template is the existing
  `qwen38-flash-next.jinja` fixture; `Worker::new(engine, max_snapshots)` matches.
- Reported fields exist: `timings.cache_n/prompt_n/snap_pos` (api.rs, generate.rs),
  `timings.echo_n/echo_fixed/roundtrip_miss/echo_miss_class` (echo.rs,
  `MISS_CONTENT = "content-mismatch"`), and
  `usage.prompt_tokens_details.cached_tokens = timings.cache_n` (api.rs).
- The `multi_thread` tokio test flavor and the `Reply::Json(c, j)` match are
  already used in this file. The ubatch grid constant 16 matches `w.ubatch = 16`.

## Checks run on this lab host (fmt and diff only)

- `cargo fmt -p oxc-gateway --check` (from `oxa/`) — clean.
- `cargo fmt -p server --check` (from `engine/`) — clean.
- `git diff --check e62da190..HEAD` — clean.
- No compiler, build or test run on this host (lab-host rule). The four new tests
  have NOT been executed locally; hosted CI is the first execution.

## Hosted checks at the exact head (lead runs and reports)

The changelog fragment lives in `changelog.d/`, which `tools/ci/paths.py` does not
classify as documentation or a known component, so a ready PR selects every
component (engine, oxa, gateway). The `checks` workflow at the branch tip is the
authoritative evidence; the jobs that execute the new tests are:

- `gateway` ("cloud gateway", ubuntu x86_64 and ARM):
  `cargo test --locked --offline -p oxc-gateway --all-features -p oxa-engine`
  in the isolated bwrap environment runs
  `plain_foreground_turns_keep_final_provider_input_append_only_both_modes` and
  `tool_continuation_appends_only_replay_and_results_both_modes` (each in both
  routes). The same job runs the oxa-wide `cargo fmt --all -- --check` and
  `cargo clippy -p oxc-gateway --all-targets --all-features -p oxa-engine -- -D warnings`.
- `x86-64-stub` ("oxe-server --engine stub on x86_64"):
  `cargo test --release --locked -p server --lib` runs
  `continuity_four_turns_reuse_the_previous_prompt` and
  `continuity_tool_turn_keeps_the_previous_prompt` (filter `continuity`).
  Model-free: stub engine, synthetic tokenizer, no weights, network or sandbox.
- `clippy (engine, oxa; aarch64)`: `cargo clippy --workspace --exclude qwen4exp
  --all-targets` in `engine/` covers the new test code.
- `rustfmt`, `scans` (includes the `changelog.py` fragment lint) and the `gate`
  aggregate must succeed at the exact head.

A branch push without a PR starts `lane` instead: it selects `oxc-gateway` plus
its local reverse dependencies in the oxa workspace and `server` in the engine
workspace (x86_64 only; ARM engine is excluded from lane). Lane is a fast signal,
not a replacement for the PR `checks` run; native engine qualification stays on
the Oryon route and is not needed for these model-free tests. Once the PR is open,
wait on the `checks` run at the exact head, never on an earlier head's result.

Expected outcome: all four tests green. The gateway fixture is a stability guard
(the probe receipt's static review expects append-only to hold on the plain path);
a red result there would be the first reproduced W1 divergence — read the failed
job tail and locate the earliest differing item before any fix. The local fixture
asserts adjacent reuse in the stub's own accounting. Neither fixture reproduces
the packet's cloud usage pattern, and neither proves upstream cache routing or
real KV reuse; owner-derived live evidence (PLAN-2255.md, via lead) is still
needed. No fix is included or implied by this branch.

### Hosted CI result (lead fills in at publication)

- Exact head tested: `<branch tip sha>`
- `checks` run id / result: `<pending>`
- Failed job tail, if any: `<pending>`

## Truncation note

No output truncation affected this work. Both fixture diffs were read in full
(269 and 306 lines including commit headers); helper and field verification read
the relevant source sections directly. The probe RECEIPT.md files were read in
full from the probe-2255-prefix and probe-2255-local worktrees.
