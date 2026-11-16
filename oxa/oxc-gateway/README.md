# In-process Rust gateway

This library belongs to the oxa workspace and shares its lockfile and build
profiles. Default features are empty. Workspace membership starts no listener,
engine or runtime, and does not install another binary.

The controller is built for small footprint, local use and large tasks. Its
tests protect requests/streams, admission budgets, private accounting, secret
handling and clean refusals. Python wording and CLI compatibility are not goals.
The existing `policy` and `secrets` features are independently selectable.
The `pricing` and `accounting` features supply cache-aware charges and atomic
process-shared reservations. Retain each hold until its writer callback commits
and syncs the private settlement row. A short file lock covers spend, persisted
holds and admission; it is released before any upstream work. Independent Budget
instances and processes use the same private daily files. Failed writes or an
abandoned potentially paid hold block further spending across processes.

The ledger directory must already be owned by this user with mode 0700. Daily
`.budget-lock` and `.holds` files are owner-only and contain only random hold IDs,
reserved amounts and a failure flag. Bounded state reads reject malformed,
interrupted, oversized, non-owner or non-private files. Rows are locked and synced
before their holds are removed. No runtime, thread or resident file descriptor is
added to an idle gateway. A crashed process retains its unknown reservations;
there is deliberately no automatic timeout or PID-based release of possibly paid
work. The next calendar day has a separate reservation file.

```sh
cargo check --locked --manifest-path oxa/Cargo.toml -p oxc-gateway --no-default-features
cargo test --locked --manifest-path oxa/Cargo.toml -p oxc-gateway --all-features --profile ci
```

Tests use synthetic credential files and run in the required CI job with private
HOME, cleared environment and an isolated network. No real credentials or cloud
calls are used. The shared workspace's gateway job also runs formatting and
strict Clippy; its result is included in `gate`.

Linux accounting tests require `strace` for actual syscall ordering and injected
sync failures. CI installs it for the isolated gateway suite.

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

The independent `upstream-anthropic-request` and `upstream-anthropic-stream`
features supply pure native Messages translation. They start no runtime or I/O.
Request translation bounds output, translates tools/results, places cache
breakpoints and preserves opaque thinking blocks. Disabled reasoning does not
activate adaptive thinking. `translate_with_blocks` accepts private restored
blocks only for the exact conversation prefix; the caller must scope storage by
model, endpoint and credential.

The stream translator takes decoded provider events and a caller timestamp.
`Stream::with_redactor` accepts independent hold-back state for text, thinking
and each tool's arguments, plus atomic redaction of tool names/IDs. Public chunks
and the assembled assistant use those same safe fragments. Signature deltas are
concatenated privately for replay and never emitted. `Stream::new` uses identity
redaction and is only appropriate without sensitive data.

`finish` rejects truncated/error streams. Only terminal output usage followed by
`message_stop` is trusted, with input, cache reads and cache writes kept separate.
Trusted usage remains available when public usage is disabled. Native tests
cover these contracts without Python peers or byte-exact goldens. Transport and
the production caller that supplies the redactor follow in S5/S6.

`anthropic-gateway` composes typed catalog configuration, private admission,
native transport and consuming settlement. `Runtime::new` takes validated model
routes with optional prices and explicit private paths; its default options
refuse cloud use. Discovery borrows public metadata and reads no key or ledger.
`Gateway::chat` takes the same borrowed `oxa_proto::ChatRequest` used by the core,
without a request-byte conversion at the engine boundary. Decoded chunks pass
straight to reply assembly. Only the frozen tools schema needs decoding.

Known refusals write private zero-spend rows. Documented Anthropic HTTP refusals
(400, 401, 403, 404, 413, 429) settle at zero as `refused`; timeout, server and
transport failures retain the conservative charge. Paid calls retain process-shared
reservations until durable settlement; interrupted guards conservatively record
unknown paid work. Trusted terminal cache usage settles even if public usage is
omitted or final delivery cancels. Unknown paid cancellations retain the full
reserved charge. Done follows a successful append; later cancellation cannot
retract an already settled reply. Failed appends block subsequent admissions.
Opaque thinking replay is bounded and scoped to history, tools, model, endpoint
and credential; it never appears in public chunks or ledger diagnostics.

The optional `oxa-engine/in-process-gateway` bridge delivers callbacks on the
calling thread. Construction/discovery starts no worker. The first chat lazily
creates one parked I/O worker for pooled connections and concurrent calls.
Shared `CancelToken` notifications wake network waits directly, with no timer
or relay token. Async consumers use `Gateway::chat` directly. Production setting
and factory selection are the subsequent default-off S7 slice.
