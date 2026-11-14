# fixtures/pi/rpc

`documented.json` holds one entry per P row of the RPC area (`RPC-0nn`): the example or rule from pi 1.5.2's
`packages/coding-agent/docs/rpc.md`, `rpc-commands.md`, `rpc-extension-ui.md` and `json.md` (commit 47e9d74). These are the
documented outputs, transcribed, not outputs recorded by running pi. A key with a `?` prefix is optional. A `null` value
in an example means "any value of any JSON type". `oxa-parity/tests/rpc.rs` compares oxa's real output against them.
