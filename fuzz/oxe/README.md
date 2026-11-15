# Fuzzing oxe (Task 61, part A)

Targets: the HTTP access/API-key layer, the OpenAI request parameter parser, chat input
normalisation and template rendering, and the Qwen3-Coder output parser (including incremental
vs reference streaming mode). No model weights, tokenizer or network are needed; the template is
the committed fixture and the engine's pure normalisers are called directly.

## Short smoke (normal CI, stable Rust)

```sh
cargo test --locked --manifest-path fuzz/oxe/Cargo.toml --test smoke
```

Runs every target over `corpus/` and deterministic mutations.

## Long runs (local session)

```sh
cd fuzz/oxe
RUSTUP_TOOLCHAIN=nightly cargo fuzz run chat_parser -- -max_total_time=300
RUSTUP_TOOLCHAIN=nightly cargo fuzz run chat_request -- -max_total_time=300
```

The libFuzzer crate is `fuzz/oxe/fuzz/` (its own workspace); seeds are under
`fuzz/oxe/fuzz/corpus/<target>/seed-*`. Crash inputs are written to `fuzz/oxe/fuzz/artifacts/`.
