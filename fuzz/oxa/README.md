# Fuzzing oxa (Task 61, part A)

Targets: SSE parsing and split invariance, tool-call stream reassembly/comment parsing, the
malformed-tool-call repair classifier, Markdown/ANSI rendering of untrusted text, session
header/entry decoding, MCP SSE message decoding, RPC command parsing and lexical path
resolution.

## Short smoke (normal CI, stable Rust)

```sh
cargo test --locked --manifest-path fuzz/oxa/Cargo.toml --test smoke
```

This runs every target over `corpus/`, then 2,000 deterministic mutations per text/stream
target. No nightly toolchain, sanitizer or network.

## Long runs (local session)

```sh
cd fuzz/oxa
RUSTUP_TOOLCHAIN=nightly cargo fuzz run sse -- -max_total_time=300
RUSTUP_TOOLCHAIN=nightly cargo fuzz run markdown -- -max_total_time=300
```

The libFuzzer crate is `fuzz/oxa/fuzz/` (its own workspace); it depends only on the harness
library here. Seeds are committed under `fuzz/oxa/fuzz/corpus/<target>/seed-*`; generated
corpus entries and crashes are ignored. A crash is saved in `fuzz/oxa/fuzz/artifacts/<target>/`.

`FINDINGS.md` records confirmed bugs, the failing test and the proposed fix.
