# In-process Rust gateway

This library belongs to the oxa workspace and shares its lockfile and build
profiles. Default features are empty. Workspace membership starts no listener,
engine or runtime, and does not install another binary.

The controller is built for small footprint, local use and large tasks. Its
tests protect requests/streams, admission budgets, private accounting, secret
handling and clean refusals. Python wording and CLI compatibility are not goals.
The existing `policy` and `secrets` features are independently selectable.
The `pricing` and `accounting` features supply cache-aware charges and atomic
cross-process reservations. Admission and settlement take an exclusive `flock`
on `<ledger>.lock`; holds are persisted in `<ledger>.holds/` with an owner lock.
Dead unforwarded holds are released; forwarded holds remain reserved if their
owner dies before recording settlement. Share a daily Budget across requests and
retain each hold until its writer callback commits the private settlement row.
Mark holds forwarded before sending any potentially paid bytes. Failed writes
block further spending. Missing/failed usage costs the full reservation;
cancellation uses final usage or a bounded byte/time estimate including hidden
reasoning and cache-write premiums. Known actual charges are never capped at
the reservation; OpenRouter's reported cost is authoritative.

```sh
cargo check --locked --manifest-path oxa/Cargo.toml -p oxc-gateway --no-default-features
cargo test --locked --manifest-path oxa/Cargo.toml -p oxc-gateway --all-features --profile ci
```

Tests use synthetic credential files and run in the required CI job with private
HOME, cleared environment and an isolated network. No real credentials or cloud
calls are used. The shared workspace's gateway job also runs formatting and
strict Clippy; its result is included in `gate`.

CI creates a synthetic file with a different owner before dropping capabilities
and passes its path as `OXC_SECRET_OWNER_PATH` to the ownership test. Root test
runs must supply such a fixture too. Unprivileged Linux runs can use the foreign
owner of public kernel metadata; the guard refuses it before reading contents.

The `oxa-cli` example `gateway_footprint` measures the actual oxa executable with
the existing owned HTTP fixture in a separate process. Run it inside the test
sandbox, passing an absolute binary path:

```sh
cargo run --locked --offline --manifest-path oxa/Cargo.toml -p oxa-cli \
  --example gateway_footprint --profile ci -- /absolute/path/to/oxa
```

It emits five samples of idle/streaming RSS and threads, startup/request time to
the first text, and the overhead against a direct request with the same body.
Results contain numbers only. This initial slice does not link the gateway into
the production binary; consumer slices must measure their own footprint.
