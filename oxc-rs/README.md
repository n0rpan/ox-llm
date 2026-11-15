# Experimental native controller: CLI grammar

This first Rust migration slice implements help, version and argument parsing
only. Every valid command refuses execution with exit code 2. It never reads
config/credentials, contacts a service, starts an engine or changes installation
state. The crate uses only the Rust standard library and forbids unsafe code.

The `oxc-native` binary requires an explicit `native-controller` feature;
default builds produce no native executable. Nothing installs it or changes the
existing Python entry points or services. The integration/proof branch remains
in draft PR #28; later behavior lands in separate reviewable PRs. Cloud-only
mode is the first complete runtime target, following #24 and its cloud-only
stack on main. This parser slice does not provide that mode.

```sh
cargo build --locked --release --manifest-path oxc-rs/Cargo.toml \
  --features native-controller --target-dir /tmp/oxc-native-target
OXC_COMMAND=/tmp/oxc-native-target/release/oxc-native \
  python3 -m unittest discover -s conformance -p test_cli.py -v
python3 conformance/check_native_scaffold.py /tmp/oxc-native-target/release/oxc-native
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml \
  --features native-controller --all-targets -- -D warnings
```

The 65 black-box parser scenarios use an isolated HOME and cleared environment
and compare exit status, stdout and stderr to the unchanged Python reference.
They include all action help/error paths, version precedence/abbreviations and
retired-command behavior. Config and runtime semantics are outside this slice.
