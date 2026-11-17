# Selected-provider preconnect through the existing gateway pool

Status: qualification only (#881, parent #864, owner decision). No runtime change; merging this note
adds no warm-up. The transport limitation below needs an owner choice.

## Question

Can oxa warm the selected cloud provider's connection after the first editable frame, through the HTTP pool
the gateway already owns, so the first real turn does not pay TCP and TLS setup?

## Verified on this tree (`69a81515`)

- **One pool already exists.** `oxc_gateway::gateway::Gateway` owns one `anthropic_client::Client`
  (`oxa/oxc-gateway/src/gateway.rs`), the OpenAI Responses client is built from the same transport
  (`openai::client::Client::from_messages_transport`), and Go discovery receives that transport
  (`Gateway::discover_opencode`). A warm-up would have to feed this instance; a second client, pool or
  daemon would duplicate the `no_proxy`, redirect-refusal and connect-timeout policy.
- **The pinned transport has no preconnect.** `reqwest` 0.12.28 (`oxa/Cargo.lock`; hyper 1.12,
  hyper-util 0.1.21) exposes no connect, preconnect or pool-insertion method on `ClientBuilder` or
  `Client`: `resolve`/`resolve_to_addrs` pin names, `connector_layer` wraps the connector, and a
  connection becomes an idle pool entry only after a completed HTTP exchange inside hyper-util's private
  pool. A raw TCP/TLS socket would not populate that pool at all.
- **A real request is the only way to warm it, and every real request is excluded here.** The provider
  origins are authenticated: `Runtime::begin` reads the key from the protected store per forwarded
  request, and `discover_go` already does this for Go. A warm-up request would therefore read a
  credential at warm time and send an unsolicited authenticated provider request, which the issue and the
  existing contracts (`Runtime::new`: construction and discovery open no ledger and start no provider)
  forbid.
- **No warm-up exists today.** Gateway construction, model selection and refusals touch no socket. The
  only startup request on the cloud path is Go discovery, when Go is enabled, and it already warms this
  pool for that provider.
- **Warmth would only ever help the first turn.** Once an exchange completes, the pool keeps the
  connection and the next streamed turn reuses it (`tests/v022_provider_preconnect.rs`). So even a
  successful warm-up could only remove setup from the first cloud turn of a process; later turns are
  already warm.

## Conclusion

The existing transport cannot warm the selected provider's pool without an actual request, and any such
request is an unsolicited authenticated provider call. Returning the limitation instead of adding a
fallback: no preconnect is implemented, and nothing is scheduled at the first frame.

## Options for the owner

| Option | Effect | Assessment |
|---|---|---|
| A. No change (recommended) | first turn keeps its connect cost | honest; the only reachable gain is one setup |
| B. Warm with a bounded request to a validated origin | would fill the pool before the first turn | needs a policy decision (credential read at warm time, unsolicited call, budget/rate-limit interaction); rejected for this task |
| C. Wait for an upstream preconnect/pool API in reqwest or hyper-util | same pool, no request | no such API exists today; track upstream |
| D. Finish reading the terminal reply before returning | may keep the connection the client already used | different mechanism from the issue's spec (no first-frame warm-up), changes the reply-completion path, and needs its own decision and measurement |

Option D is worth noting because the client returns as soon as the `message_stop` event is decoded
(`oxa/oxc-gateway/src/anthropic_client.rs`) and drops the remaining body. Whether hyper can then return
that connection to the pool is a race the client does not control, so it is a hypothesis, not a measured
gain; the loopback test below shows reuse only when the whole reply arrives before the client stops.

## Acceptance items returned as not applicable

The issue's acceptance for an implemented warm-up (connection/request counting, zero warm-up when
offline/cloud-off/no provider, cold-vs-warm TTFT under a handshake delay, unchanged proxy/redirect
controls) presumes a warm-up exists. Under this outcome the executable parts are instead the safety
invariants a future warm-up must keep, pinned by `oxa/oxc-gateway/tests/v022_provider_preconnect.rs`:
construction and cloud-off/offline refusals open no connection, and a completed exchange is what leaves a
reusable pool entry.

## Measurement limits

Source-only qualification: nothing was built or benchmarked here, no provider call was made, and no
credential was read. The loopback test counts TCP accepts and requests only. It has no TLS handshake, so
it cannot reproduce the first-turn TLS cost that the 100-300 ms hypothesis depends on; that figure stays
unmeasured and needs a separate immutable receipt on real hardware.
