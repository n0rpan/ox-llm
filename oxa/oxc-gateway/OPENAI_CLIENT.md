# OpenAI Responses client

`openai/client.rs` implements the transport part of the second-provider contract
on #331. It does not activate a route. Rust-cloud owns credential-path selection,
runtime/gateway integration, admission, billing, settlement and durable replay
scope. No runtime.rs, gateway.rs, accounting.rs or oxa-proto edits belong here.
The only shared-helper change is the narrow nonblocking token-lock entry approved
by rust-cloud on #331; no sign-in or file-format change.

## Contract

`Client::stream(Chat, Auth, Cancellation, callback)` returns the final assistant,
ordered private replay items, optional `accounting::Usage` and finish reason.
Callbacks receive `Sending` before generation forwarding and normalized `Chunk`
values. There is no transport Done: the owner settles and writes its ledger
before signaling success. `stream_with_usage` preserves private terminal usage
before a finish/public-usage callback can cancel delivery.

`Chat` borrows canonical JSON and explicit per-assistant-turn replay items; it
uses #663's translator API. Persist the returned ordered items with assistant
history under the owner's provider/model/account/prefix scope. Supply them to a
new client/process on continuation. There is no previous-response ID or hidden
continuation cache. Missing replay items retain the typed request refusal before
credential access. Error tool results retain their content and matching call IDs.

`Auth::ApiKey` borrows a freshly supplied `secrets::Key`.
`Auth::Subscription` uses only the explicitly supplied token path and refresh
configuration through the merged token-file helper. It does not discover ambient
credentials or sign in. Subscription request adaptation and protocol headers come
from the pure helper. Refresh rotates returned access/refresh tokens, retains the
selected account, and preserves the prior refresh token when omitted. Invalid
expiry/records fail without replacement. Lock acquisition and the complete refresh
HTTP exchange share a caller deadline capped at 30 seconds; the HTTP timer runs under the file helper's lock. The
coordinator-approved additive `try_credentials` entry retries only WouldBlock
without leaving a blocking-lock task behind. The original blocking API and all
owner/mode/no-follow/record/atomic-replacement checks remain intact.
Cancellation or dropping the request stops its owned refresh HTTP operation.

The reusable HTTP pool follows Anthropic's hardened transport policy: no proxies
or redirects, HTTPS except owned loopback HTTP, a 30-second connect timeout and
30-minute read timeout for long thinking pauses. SSE handles CR/LF/CRLF, comments,
multiline data and arbitrary UTF-8 boundaries. Request/stream bodies are capped
at 64 MiB; error/refresh bodies at 64 KiB. Non-SSE success, malformed framing,
failed/incomplete transport and missing terminal events fail closed. A valid
provider incomplete response retains its length outcome and known usage.

API and subscription HTTP refusals use the existing typed classifiers; upstream
body text is never retained in errors. Authorization/account headers are marked
sensitive. Decoded fields redact current and freshly retired credentials across
chunk boundaries before callbacks and final assistant exposure. Overlapping rotated
credentials use leftmost-longest matching, holding an ambiguous prefix until it
can be disambiguated; a failing-first regression covers this case. Auth, refresh
and completed-response Debug omit credentials/private content; HTTP error Debug
also hides provider-owned plan text rather than trusting it as a safe string. Provider replay
items are private input to the caller's scoped persistence, not a public event.

Usage keeps total prompt including cache reads and total output including
reasoning. Cache writes are zero; missing/invalid final usage remains unknown.
The client makes no USD estimate and does not decide refusal charging or caps.

## Validation and integration state

Tests use owned Rust loopback mocks and generated temporary synthetic records;
no real keys, sign-ins, credential-file discovery or provider calls. Coverage
includes both modes, three tools with done-only/partial arguments, empty completed
output, streamed/completed call agreement, error results, real serialized replay
continued byte-identically by a NEW OS process, missing-state refusal, refresh,
malformed refresh, timeout/lock release, dropped futures, cancellation, redirects,
body bounds, private usage opt-out and decoded credential redaction.

The client is registered only with the existing `upstream-openai`,
`anthropic-client` and `ledger` feature combination. It adds no new dependency,
daemon, default-on behavior or route. Default features remain empty; enabled
binary-size delta is not measured. Runtime/provider wiring stays with rust-cloud.

The client branch includes normal merges of the actual #660 stream revision and
#673 refusal classifier; validation uses their public modules, not scratch copies.
The #660 takeover fixes reasoning/answer separation, ordered opaque replay with
redacted Debug, invalid usage remaining unknown and inconsistent argument-prefix
rewrites. All 21 stream regressions pass, including failing-first cases. The
native incomplete/invalid-usage client case now passes as well.

Full gateway tests pass in the normal parallel mode with private HOME, a private
`/tmp` mount and network namespace. This includes 102 library tests, 17 actual
client integration tests, eight token-file tests and four subscription classifier
tests, plus the remaining gateway suites. Strict all-target/all-feature Clippy,
formatting and default-off checks pass. Persisted history is serialized to disk
and continued by a newly spawned OS process in both modes; captured upstream
request bytes equal the same-process continuation, including error tool results.

The suite requires `strace` for its existing durability/fault-injection test.
Local validation supplies a checksum-verified package only inside the test
namespace, with no system install or test changes. Shared `/tmp/.git` artifacts
correctly trigger the key-file repository guard; isolation uses a private `/tmp`
mount rather than bypassing that guard. An earlier parallel discovery-fixture
name collision is tracked with rust-cloud for its separate S7 fix; this passing
parallel run does not claim to fix it. No scanner or acceptance requirement is
weakened.
