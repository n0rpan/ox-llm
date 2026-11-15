# A2A specification examples

The three JSON files are unmodified examples from sections 9.1 (request and
task response) and 9.2 (direct message response) of the
[A2A 0.3.0 specification](https://a2a-protocol.org/v0.3.0/specification/).
Retrieved for this implementation from that versioned specification.
Upstream: [a2aproject/A2A, tag v0.3.0](https://github.com/a2aproject/A2A/tree/v0.3.0).
Licensed under Apache-2.0; see LICENSE-A2A in this directory.
These fixture files retain that license, independently of the MIT Rust code.

The examples have omissions relative to the
[tagged normative schema](https://github.com/a2aproject/A2A/blob/v0.3.0/specification/json/a2a.json):
the request and task history omit Message.kind; the direct response omits
Message.role. Conformance tests first check that these omissions are rejected,
then explicitly insert only the missing required fields in memory and validate
the resulting objects. Fixtures themselves are unchanged. This is selected
wire-profile coverage, not a claim of passing the full upstream conformance suite.
