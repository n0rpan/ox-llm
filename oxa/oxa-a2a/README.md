# oxa-a2a

This library implements an original,
text/data-only wire profile for [A2A 0.3.0](https://a2a-protocol.org/v0.3.0/specification/).
The upstream specification/schema is
[Apache-2.0 licensed](https://github.com/a2aproject/A2A/blob/v0.3.0/LICENSE).
No upstream implementation code or schema is copied. Selected specification
examples are retained under Apache-2.0 in `tests/fixtures`, with their license
and attribution. Tests document required fields missing in the published
examples and explicitly correct them in memory against the normative schema.
This is selected profile coverage, not full upstream-suite conformance.

Current scope: typed messages/tasks/events/cards, JSON-RPC envelope validation,
task identity checks, a bounded SSE data reader, authenticated loopback HTTP
client and an owned mock agent. It also provides bounded
collection and a server for owned backends. File parts and unknown
required extensions fail closed. Cards cannot change the registered endpoint.
The client provides send, get, stream, cancel and resubscribe. Streams bind
task/context IDs and reject disconnects before a final record. Send and stream
continuations retain each request-specified task/context constraint, including
the first reply. Bound Message replies must carry the requested identity fields;
unbound new-task/direct-reply requests remain supported. Tests exercise
absolute deadlines, output limits, bad auth, malformed HTTP and protocol errors.

`ResponseBudget` bounds aggregate incoming wire bytes across requests, including
HTTP headers and optional fields that decoding ignores. Shared readers reserve
before reading. Unused observation allowance can fund cleanup without increasing
the aggregate allowance. `collect::Collector` binds task/context identity,
assembles append/lastChunk artifacts, handles direct messages and paused
streams, and bounds retained results. After a disconnect it cannot promote an
incomplete artifact to a complete result using a snapshot that omits that artifact.

The mock uses random ephemeral capability tokens, loopback ephemeral ports,
bounded requests and task history, and joins its server thread on drop. It
supports completed echo tasks or paused tasks that can be canceled or steered.
It is a development fixture, with no models or tools. Unsupported send
configuration fails explicitly, including push-notification configuration.

`server::Server` explicitly starts an authenticated loopback-only listener for
trusted `Backend` implementations. It serves send/get/cancel and live streaming
or resubscription through bounded task snapshots. It limits simultaneous
handlers, leaves one slot available for non-streaming control requests, caps
headers/bodies/events, and interrupts owned sockets and joins handlers on drop.
Cancellation must produce a terminal task state; an unverified acknowledgment
is an error. The backend must supply bounded methods and join its owned work
on shutdown. No models, tools or arbitrary plugins implement this trait.

Limits: responses require Content-Length by default. SSE
also supports explicit Connection: close framing for live updates. Chunked transfer encoding remains
unsupported. No production TLS/remote adapter exists yet. Only explicitly
registered loopback development agents are eligible; a loopback oxc proxy is
not thereby proven local or offline. This crate itself launches no workers.
Permission envelopes, production credentials and worker
budget enforcement belong to orchestration and are not supplied by wire types.

The crate is linked into `oxa` (Linux) and compiled into the release binary. Nothing opens a
socket until the caller explicitly constructs a client or server; this crate itself starts no
listener and launches no workers. The user-only setting `oxa.a2a.enabled` (default true) gates
the run-time use. The 34 wire/collector/loopback/host cases run in the ordinary workspace test
inventory.

Run `cargo test --locked -p oxa-a2a` from `oxa/`. It reuses serde/serde_json; no new external dependency is introduced. Manifest
registration is explicitly subject to architect/coordinator review.
