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

The early draft remains unregistered pending the remaining helper modules (#660 and #673); subscription adaptation #657 and
request replay #663 are merged. Local development compiles against scratch copies of those helper
interfaces; that is not a completed merged-main gate. Once they land, add the
client module behind the existing upstream-openai/anthropic-client/ledger feature
combination and run the normal gateway fmt/strict all-target Clippy/full tests.
No new dependency, daemon or default-on behavior is introduced. Until module
registration, this client adds no linked production code; default features remain
empty. Runtime/provider wiring stays with rust-cloud.

A full parallel gateway run also exposed an existing fixture collision outside
this lane: Anthropic discovery and OpenRouter discovery use the same temporary
`gateway-discovery-{pid}-{counter}` names with independent counters. Serial
library tests pass; that is diagnostic evidence, not a replacement parallel gate.
Shared `/tmp/.git` artifacts correctly trigger the existing key-file repository
guard. Local isolation uses a private `/tmp` mount rather than bypassing it.

A native incomplete-response test also exposed a contract-item-5 mismatch in
stream helper #660 at its current head: invalid final usage fails the response
instead of leaving usage unknown. Missing/null and valid usage controls pass.
The regression stays in the client suite; the helper owner has been notified.
This draft is not qualified or READY until that helper contract passes too.
