# In-process Rust gateway

This library belongs to the oxa workspace and shares its lockfile and build
profiles. Default features are empty. Workspace membership starts no listener,
engine or runtime, and does not install another binary.

The controller is built for small footprint, local use and large tasks. Its
tests protect requests/streams, admission budgets, private accounting, secret
handling and clean refusals. Python wording and CLI compatibility are not goals.
The existing `policy` and `secrets` features are independently selectable.
The `pricing` and `accounting` features supply cache-aware charges and atomic
per-process reservations. Share a daily Budget across requests and retain each
hold until its writer callback commits the private settlement row. Failed writes
block further spending. Cross-process admission is a later series step.

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

The `cloud-only` feature provides a typed, immutable catalog. Construct validated
`catalog::Model` values from trusted user settings, then create a `Catalog` with
explicit `Access`. Discovery borrows `ModelInfo` values with each model's token
limits; private upstream IDs and endpoints are excluded from those values and
Debug output. Disabled/offline catalogs hide and refuse models. An explicit
unknown model is refused rather than silently selecting another provider.
Catalog construction reads no credential or ledger and starts no engine.

Native policy tests assert client status, code, type and parameter, with a
nonempty explanation. They protect error classification without wording parity.

The `ledger` feature supplies private request rows and one shared budget per local
calendar day. Keep one `Ledger` per gateway and clone its handles across calls;
an admitted call retains its original day over midnight. The install salt and
rows are owner-only. Explicit session and idempotency identifiers use separate
keyed HMAC domains. The entry API accepts no request body, and records no prompt,
response, agent fingerprint or inferred session/gap telemetry. Billing fields
are validated and serialized through one writer; failed appends fail admission
closed. Cross-process admission follows the walking-skeleton series.

`KeyFile::prepare` binds an approved parent directory once per configuration;
each read opens the key file afresh without following its final link and checks
owner, mode and size again. No key bytes or key descriptor are cached. Secrets
use UTF-8 JSON (an optional UTF-8 BOM is accepted); UTF-16/32 and nonfinite JSON
extensions are refused. The copied conversion/normalization buffers are gone.
Model endpoints require HTTPS, with HTTP limited to loopback fixtures.
