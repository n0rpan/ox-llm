# Experimental native controller: CLI control requests

This slice adds seven commands to the opt-in `oxc-native` binary: `load`,
`unload`, `clear-cache`, `flash-cache`, `harness-prepare`, `harness-forget` and
`storage-delete`. They use the existing loopback control API and user-service
names. Other command execution still refuses with exit 2. Nothing installs the
binary or changes existing Python entry points or services.

The `native-controller` Cargo feature remains off by default. Dependencies are
optional; default builds produce no native executable. The safe Rust client is
separate from argument parsing, engine/daemon code, status projection, desktop
launching, file readers and installers. No Python runtime fallback is present.

The client retains the 64 KiB reply bound, per-command timeouts and the reference
notification/failure behavior. The timeout measures a stalled read, including
responses progressing beyond 15 seconds total. Legacy Python JSON constants in
otherwise valid replies do not discard usable error fields; non-finite values
are excluded from the client's numeric projections.

```sh
cargo build --locked --release --manifest-path oxc-rs/Cargo.toml \
  --features native-controller --target-dir /tmp/oxc-native-target
OXC_COMMAND=/tmp/oxc-native-target/release/oxc-native \
  python3 -m unittest discover -s conformance -p 'test_cli*.py' -v
python3 conformance/check_native_scaffold.py /tmp/oxc-native-target/release/oxc-native
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml \
  --features native-controller --all-targets -- -D warnings
```

The 140 parser scenarios and 33 control scenarios compare with the unmodified
Python reference. Every control scenario runs in its own network/HOME namespace
against owned HTTP and system-command peers. No engine/model or real service,
notification, credentials or cloud request is used. The trickled-response case
observes the unchanged 15-second stalled-read limit. Unsupported commands have
separate capability-refusal checks.

Stacked on the parser slice #177; #28 remains the integration/proof branch.
Cloud-only is the first complete runtime target, following #24 and its stack on
main. Keep all migration PRs draft until v0.2.0 ships; continue local work/main
follow-up without waiting for reviews or requesting full CI. Post-release
reviews are through the orchestrator (Fable) and oxa.
