# Opt-in Rust cloud gateway

First extraction for #28 and #289: engine-free discovery metadata from the
Python `oxc` controller. Default features are empty. No binary is installed;
the example is only a JSON contract driver and opens no listener.

`Catalog::from_config` selects only `cloud_only: true`, respects cloud-off and
offline policy, and supplies model rows, chooser rows, model limits, sleeping
health and the startup line. Its status fragment is composed by a future HTTP
adapter. It reads no files, keys, environment, hardware or inference leases.

Reference: main `d7f70bb05ed7ef65ea377e8e6cdcee0dab4415aa`. Eight cases use the
unmodified Python HTTP controller, an owned mock upstream, a private network
namespace and HOME, `/usr/bin/python3`, and a foreign lease held through stop.
Only discovery/status fields owned by this slice are compared. HTTP gates,
forwarding, secrets, budget/ledger, cancellation and installation remain future
slices; this is not a complete cloud-only daemon.

```sh
cargo build --locked --manifest-path oxc-gateway/Cargo.toml --features cloud-only --example catalog_contract
/usr/bin/python3 oxc-gateway/conformance/test_catalog.py
OXC_CATALOG_CONTRACT="$PWD/oxc-gateway/target/debug/examples/catalog_contract" /usr/bin/python3 oxc-gateway/conformance/test_catalog.py
```

Run Clippy with `--all-targets --all-features -- -D warnings`, and `cargo check
--no-default-features`. Bubblewrap is required for source capture. Goldens are
recorded only with `--record-python`; test/native runs never regenerate them.

Catalog regressions cover fractional limits with strict JSON-type equality,
truthy non-string upstream IDs and falsy provider/upstream fallback. All three
failed against the initial native head; the unchanged source goldens still pass.
