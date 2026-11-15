# Experimental native controller

The opt-in `oxc-native ledger` command reads the existing local JSONL ledger
without modifying it. It follows `OXC_LEDGER_DIR` or the default state directory,
reads at most 512 KiB from each of today's and yesterday's files, and returns up
to 200 newest rows. Only public numeric fields and allowlisted labels are shown;
prompts, replies and arbitrary labels are excluded. Symlinks and non-regular
files are refused; opening a FIFO never waits for a writer.

The parser and ledger CLI are independent of the pending control-command slice.
Other command execution still refuses with exit 2. The native binary requires
`native-controller`, off by default; no installation or Python entry point changes.
Optional dependencies provide dates, safe file flags and public JSON formatting.

```sh
cargo build --locked --release --manifest-path oxc-rs/Cargo.toml --features native-controller
OXC_COMMAND="$PWD/oxc-rs/target/release/oxc-native" python3 -m unittest discover -s conformance -p 'test_cli*.py' -v
python3 conformance/check_native_scaffold.py "$PWD/oxc-rs/target/release/oxc-native"
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features
cargo test --locked --manifest-path oxc-rs/Cargo.toml --all-features
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-features --all-targets -- -D warnings
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
```

Use Python 3.14.8 for the frozen parser contracts (CI pins it); other interpreters
can differ in argparse messages. The 140 parser and 17 ledger cases compare exact
stdout, stderr and exit codes with Python under a private HOME and a cleared
environment. Ledger cases cover private fields, corrupt rows, bounded tails,
symlinks, directory override, number formatting, non-finite numeric literals and Unicode/large integer limits.
The FIFO regression uses a synthetic pipe and cleans up even when the reader fails.

The pure upstream translator remains a separate default-off library feature;
see [UPSTREAMS.md](UPSTREAMS.md) for its API and fixture checks. Combining Cargo
features preserves ledger JSON order while translator tests canonicalize object
keys explicitly. Arrays and all text bytes remain exact. v0.2.0 is released;
locally green slices proceed to ready status and automated review.
