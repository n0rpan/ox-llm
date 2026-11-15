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

OpenRouter's reported cost wins even when below catalog cost, intentionally
fixing #283 rather than copying Python's remaining maximum-of-the-two defect.
The catalog estimate remains a separate field. `SpendReader` matches main's
incremental #296 reader: skip only an unfinished last line, reject malformed
complete rows, and retain a running total/cursor. Replacement, truncation or a
changed 64-byte cursor tail resets it. Unchanged files require bounded work.

```sh
cargo check --locked --manifest-path oxc-gateway/Cargo.toml --no-default-features
cargo test --locked --manifest-path oxc-gateway/Cargo.toml --all-features
cargo build --locked --manifest-path oxc-gateway/Cargo.toml --all-features --examples
/usr/bin/python3 -m unittest discover -s oxc-gateway/conformance -v
OXC_POLICY_CONTRACT="$PWD/oxc-gateway/target/debug/examples/policy_contract" OXC_PRICING_CONTRACT="$PWD/oxc-gateway/target/debug/examples/pricing_contract" /usr/bin/python3 -m unittest discover -s oxc-gateway/conformance -v
```

Forty-six price/settlement cases are captured from unmodified Python in private
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
