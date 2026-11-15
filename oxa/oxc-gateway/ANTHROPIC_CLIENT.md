# Native Anthropic transport (#331/#334)

The default-off `anthropic-client` feature exposes `Client`, `Chat`,
`Cancellation`, `Control` and transport `Event`s. Retain one `Client` for its
connection pool, then supply the admitted model, bounded payload and fresh key
for each `stream` call. Admission, offline policy, key-file ownership checks,
budget reservations and ledger settlement belong to the composing gateway.

`Chat` accepts a host root or an API base ending in `/v1`; the client sends
`/v1/messages`, the native version header and a sensitive key header. It forces
streaming, disables proxy discovery and redirects, bounds request/response
bytes, and uses the shared pure translators. Error values contain static
diagnostics and HTTP status codes, without provider bodies or configured URLs.

The client emits `Sending` immediately before a validated network attempt and
then normalized `Chunk` values. It returns a completion only after terminal
usage and `message_stop`. The composing gateway must settle before delivering
its own Done event. Cancellation wakes a provider read, and Stop ends delivery.
Public usage opt-out does not discard the returned final usage. Decoded text,
thinking and each tool's arguments use independent key-redaction state. Opaque
native signed blocks remain private and must never be logged.

The shared runner-owned loopback peer is reused for the client contracts:
native path/headers/body, bounded output, fragmented UTF-8, decoded key-echo
redaction, separate cache categories, status-only provider errors, invalid or
pre-cancelled requests with no I/O, and cancellation while awaiting headers.
Tests use synthetic keys in private owner-only files, a cleared environment
and an isolated network. No SDK or real provider is used.
