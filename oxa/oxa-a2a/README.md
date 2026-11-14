# oxa-a2a

Work in progress for milestone 2. This inert library implements an original,
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
client and an owned mock agent. File parts and unknown
required extensions fail closed. Cards cannot change the registered endpoint.
The client provides send, get, stream, cancel and resubscribe. Streams bind
task/context IDs and reject disconnects before a final record. Tests exercise
absolute deadlines, output limits, bad auth, malformed HTTP and protocol errors.

The mock uses random ephemeral capability tokens, loopback ephemeral ports,
bounded requests and task history, and joins its server thread on drop. It
supports completed echo tasks or paused tasks that can be canceled or steered.
It is a development fixture, with no models or tools. Unsupported send
configuration fails explicitly, including push-notification configuration.

Limits: HTTP responses must carry Content-Length; chunked transfer encoding is
rejected. No production TLS/remote adapter exists yet. Literal loopback mocks
are the only eligible routes; a loopback oxc proxy is not thereby proven local
or offline. No worker processes are launched. Permission envelopes, production
credentials, artifact accumulation and worker budget enforcement belong to
later orchestration work and are not supplied by these wire types.

Run `cargo test --locked -p oxa-a2a` from `oxa/`. The workspace registration
adds this crate to ordinary hosted checks without linking it into `oxa`.
It reuses serde/serde_json; no new external dependency is introduced. Manifest
registration is explicitly subject to architect/coordinator review.
