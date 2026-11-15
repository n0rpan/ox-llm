# Findings: fuzzing oxe (Task 61, part A)

No confirmed crash or invariant violation was found in this pass. The targets and the short CI
smoke exist so the next change is exercised automatically:

| Target | Property |
|---|---|
| `chat_request` | arbitrary JSON request bodies either normalise/template-render or return `BadParam`; no panic |
| `chat_parser` | arbitrary model text parses in partial and final mode; malformed output is `ParseError`, not a panic |
| `chat_session` | incremental parsing returns the same message and error state as the reference full parse at every chunk |
| `api_params` | `gen_params` / `check_sampling` / `logit_bias` accept or reject arbitrary JSON without panicking; limits stay enforced |
| `http_access` | arbitrary Host/Origin/auth header bytes and key files are accepted or refused; never a panic |

Coverage: 5,000 libFuzzer runs per target over the committed seeds plus mutation; the normal-CI
smoke runs the same entry points on stable Rust. The run does not need model weights, a tokenizer,
NPU/Hexagon SDK content, a network socket or an API key.

If a later run finds a bug, add the minimised input to `corpus/` (or
`fuzz/oxe/fuzz/corpus/<target>/seed-*`), a failing smoke case, and the root-cause/fix section
here — with the fix in its own commit, as the other Task 61 PRs do.
