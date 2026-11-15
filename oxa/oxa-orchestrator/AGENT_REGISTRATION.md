# Explicit private registration and host ownership

With default-off `core`, `registration::AgentHost::start` owns a foreground
loopback A2A server backed by the operator's fixed native RPC policy. It creates
a new private host directory and atomically publishes a new mode-0600
`registration.json`. A fresh random transport capability and its SHA-256
fingerprint identify the host instance. No inference is launched merely by
hosting, loading registration or creating a client.

Registration is an A2A bearer capability, never a cloud/provider credential or
a tool grant. `Registration::load` reads only an explicitly selected private,
operator-owned, bounded plain single-link file through a protected private
parent inode. It refuses links/special files, unsafe parents, changed captures,
invalid versions/addresses, mismatched fingerprints and unknown fields. Only
literal loopback addresses with nonzero ports are accepted. Debug redacts the
capability; parsing failures do not echo untrusted credential values. Do not log,
commit or forward the private registration file as ordinary result metadata.

Loading metadata does not authenticate liveness or establish that inference
cannot forward to paid models. The caller must verify the original instance and
make the explicit local-only assertion when constructing a `LocalAgent`.
The host fixes executable paths, role/model, permissions/tools and per-task
budgets; incoming messages carry task/steering data, not a policy override.
There is no implicit discovery, installation or production service.

A submitting controller may exit while the independently owned host remains
alive. Reconnecting with the same initial message identity/data uses the retained
task instead of launching another native worker. A fresh host uses a different
capability and rejects the previous one. A retained registration after shutdown
is stale diagnostic data, not proof that a host is live or a task may be replayed.

Drop closes handlers and invokes backend cancellation/join. For a success result,
use `AgentHost::shutdown`: it first closes the server and joins native task
threads, then requires known cleanup evidence for every retained task. Unknown
cleanup remains an error, including infrastructure failures without a trustworthy
report. Clean shutdown confirms ownership cleanup, not task success or provider
billing. Retained native journals preserve the actual outcomes and uncertainty.
CLI signal/lifetime handling and TUI integration remain later slices.

Local acceptance: 139 ordinary crate tests pass, including five registration
cases. Tests cover private/redacted bounded loading, malformed routes and values,
an actual submitting process exiting while its native RPC worker stays owned by
the separate host, reconnect without a second spawn, drop/explicit shutdown with
stopped-child journal evidence, unknown-cleanup refusal, and fresh-instance
capability rejection. Native workers are isolated trusted mock RPC processes;
no real model, provider, shared gateway or paid request is involved.
