# Opt-in Rust cloud gateway

Default features are empty; nothing is installed or starts a listener. Python is
the behavior reference. This #28 / #289 slice adds `pricing` and `accounting` to
the existing `policy` feature.

`Price` validates flat/tiered catalog prices and computes decimal actual charges,
separating uncached input, cache reads, cache writes and output. Direct Anthropic
reads default to 0.1x input; absent write rates conservatively reserve 2x. Other
providers retain conservative defaults unless the catalog supplies cache rates.
Reservations cover the full input window and maximum tier/cache rates, with the
reference's 25% OpenRouter headroom. Output must already be validated and bounded
by the gateway; zero is refused here too.
- `cloud-only`: catalog contracts.
- `policy`: policy contracts.
- `secrets`: secrets contracts.
- `upstream-anthropic-request`: the pure request translator shared with oxc.

Share one `Budget` per daily ledger path across request tasks. `reserve` locks
spend plus outstanding holds atomically. Mark the hold forwarded before sending
any potentially paid bytes. `finish` writes fixed remote settlement fields via a
callback while retaining the reservation, then releases it. The callback must add
the existing private request metadata (origin, IDs, HMACs, timings); never add
prompts or replies, and never re-enter this budget from the callback. Redact any
provider-supplied strings before observing usage. `append_private` writes a 0600
record in a dedicated 0700 ledger directory. Failed writes or dropped forwarded
holds block further spending in that budget instance. Missing/failed usage costs
the full reservation; cancelled streams use final usage or a bounded byte/time
estimate that includes hidden reasoning.
Build all examples with `cargo build --locked --manifest-path
oxa/oxc-gateway/Cargo.toml --all-features --examples`. Run the source contracts with
`/usr/bin/python3 -m unittest discover -s oxa/oxc-gateway/conformance -v`; native
checks set each present `OXC_*_CONTRACT` to its absolute example executable.

OpenRouter's reported cost wins even when below catalog cost, intentionally
fixing #283 rather than copying Python's remaining maximum-of-the-two defect.
The catalog estimate remains a separate field. `SpendReader` matches main's
incremental #296 reader: skip only an unfinished last line, reject malformed
complete rows, and retain a running total/cursor. Replacement, truncation or a
changed 64-byte cursor tail resets it. Unchanged files require bounded work.

```sh
cargo check --locked --manifest-path oxa/oxc-gateway/Cargo.toml --no-default-features
cargo test --locked --manifest-path oxa/oxc-gateway/Cargo.toml --all-features
cargo build --locked --manifest-path oxa/oxc-gateway/Cargo.toml --all-features --examples
/usr/bin/python3 -m unittest discover -s oxa/oxc-gateway/conformance -v
OXC_POLICY_CONTRACT="$PWD/oxa/oxc-gateway/target/debug/examples/policy_contract" OXC_PRICING_CONTRACT="$PWD/oxa/oxc-gateway/target/debug/examples/pricing_contract" /usr/bin/python3 -m unittest discover -s oxa/oxc-gateway/conformance -v
```

Forty-seven price/settlement cases are captured from unmodified Python in private
HOME/network namespaces using `/usr/bin/python3`; thirteen include remote row
fields and cancellation. Ten native regressions cover concurrent admission,
partial appends, rewrite/reset, permissions, cache pricing, reported cost and
write-failure handling. Source goldens use one case per line for review. These
contracts use synthetic data only; no real keys or API calls. Existing policy
contracts still run with explicit native drivers. Hosted x86/ARM gates run these
checks when ready; drafts retain cheap checks.

Still missing: HTTP composition and private request metadata integration,
provider forwarding/cancel and installer. Companion discovery/secrets slices stay
independent. Preserve all present modules/features/examples when merging them;
this library alone is not a complete cloud-only daemon.

Main’s `secrets` module and its explicit native contracts are preserved. The
symlink-loop case validates the two complete interpreter-specific safe refusals
from Python 3.12/3.13; all other secret errors remain exact. Credentials are read
from owner-only files and are never logged or included in errors.

Cancellation without final usage uses the maximum applicable cache rate, fixing
the reference’s remaining plain-input estimate for cache writes. The three
estimate fixtures retain both the unchanged Python row and an explicit native
contract derived with Python’s existing conservative cost oracle; final usage
stays exact. Two costs differ; the reservation-capped case agrees.

Cancellation estimates are capped at the admission hold, but a known actual
charge above that hold is always retained (#283).
The secrets loop contract explicitly supports Python 3.12's generic refusal and
3.13's symbolic-link refusal: it validates one of those two complete messages
before normalizing only that wording. Native uses the symbolic-link message.
The secrets subset passes both Python versions and native; every other error is
compared exactly. Credential values and paths must never appear in errors.

`secrets::KeyFile::prepare` validates the configured location once per config
generation. Keep its Result and surface the static refusal after policy checks;
repository/source-tree paths now have their own safe reason (#331/#266). Parent
links bind to the approved canonical directory, so later retargeting cannot
select a repository key. Reprepare when configuration/location changes. Every
read still opens with no-follow and verifies regular-file type, owner, mode,
size and fresh bytes. Key material is never cached; Debug omits the path.
The parent directory must exist when preparing the location. Create it before
constructing/reloading the configuration; a missing file within it is allowed.
Three filesystem regressions cover parent retargeting, missing parents,
rotation, mode changes and final symlinks. The 34 source contracts stay unchanged; three native repo refusals
intentionally use the requested specific diagnostic rather than Python's generic
message. All other errors remain exact, except the already validated Python
3.12/3.13 symlink-loop wording.

The `ledger` feature composes private request metadata with accounting for the
in-process Anthropic skeleton (#331/#289). Open one `Ledger` for the gateway
lifetime and clone it. Each `today()`/`day()` handle reuses exactly one Budget
for that day's path; an admitted request keeps its day across midnight. The
install HMAC salt is stable, owner-only, and published atomically so concurrent
starts cannot observe a partial salt. Invalid salts fail closed without rotation.

```rust,no_run
# fn example(directory: &std::path::Path, payload: &serde_json::Value, model: &oxc_gateway::accounting::Model) -> Result<(), Box<dyn std::error::Error>> {
use oxc_gateway::{ledger::{Context, Ledger}, accounting::{Outcome, Usage}};
let ledger = Ledger::open(directory)?; // retain this instance across calls
let daily = ledger.today()?;
let mut entry = daily.entry("configured-alias", payload, Context::default())?;
let mut hold = daily.budget().reserve(&serde_json::json!(1.0), true, model, 100)
    .map_err(|_| std::io::ErrorKind::PermissionDenied)?;
entry.admitted();
hold.mark_forwarded(); // immediately before potentially paid provider I/O
// The native client translates and redacts its stream; observe its final usage.
let usage = Usage::default(); // missing usage retains the conservative hold
hold.finish(&usage, Outcome::Complete, |fields| {
    entry.append(fields, 200, None, None).map(|_| ())
}).map_err(|_| std::io::ErrorKind::Other)?;
# Ok(()) }
```

Pass the provider key to `Entry::append` whenever one is available, including
failures. Metadata includes a random request ID, wall/first-data timings,
normalized origin, keyed agent/session/idempotency identifiers and numeric
message/tool sizes. No prompt, reply, tool contents or raw identifiers are
retained. Only typed accounting fields are copied from settlement. Seven new
filesystem/concurrency tests cover metadata privacy, shared admission, write
failure, midnight, salt initialization/validation and key redaction. Full history
classification, pruning and HTTP/CLI presentation remain outside this slice.
`Catalog::from_config` takes a trusted normalized public-price snapshot keyed
by upstream ID. This keeps file IO outside metadata; valid/no configured router
prices receive the overlay, while invalid configured prices stay invalid. Source
contracts seed an owner-only public cache and make no public network calls.

`runtime` is the Anthropic-only in-process admission/settlement layer for #331.
`Runtime::new(config, key_path, ledger_dir, Options)` uses trusted, already-resolved
paths; Options supplies offline and ledger-enabled policy. Keep one Runtime for
all sessions. Construction/discovery reads no credential bytes and opens no
ledger or network connection. Discovery advertises only implemented Anthropic
models. The native client's async entrypoint composes these methods:

1. Validate the pure provider request, then call `begin(payload, Context)`.
2. Use the guard's immutable `model()`, `payload()` and `key()`; its max_tokens
   is bounded by the same catalog limit used for reservation.
3. Call `mark_forwarded()` immediately before potentially paid I/O. The client
   translates/redacts streaming fields and passes canonical usage to `observe`.
4. Call `finish(Outcome, status, static_error)` and await successful settlement
   before reporting success to oxa. A dropped forwarded guard records the
   conservative interrupted charge; an unforwarded guard records zero cost.

Nine new native integration tests exercise this orchestration without network
or an engine. This is not yet the complete async client: the rust-upstreams lane
provides Anthropic HTTP/stream I/O and the Engine bridge, and the runner lane
drives that actual async entrypoint against its shared owned mock. Refusal-row
recording before admission, replay/history, retention and other providers remain
incomplete; the existing policy contracts still run. No Python feature changes.
This library lives inside oxa's product tree, so oxa can consume it in-process
without a path dependency leaving that tree. It has its own workspace and lock,
preserving independent gateway builds and feature checks. The native oxc
controller can depend on the same source from its separate workspace. Both entry
points retain the same policy, key and settlement implementation; transport
adapters supply trusted caller metadata. Default features remain empty, and
moving the library does not enable a listener or change the default engine.
