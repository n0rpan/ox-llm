# Guest sharing (opt-in foundations)

The `share` feature is off by default. This first slice supplies a key registry,
not a listener or a CLI command. Importing it creates no files, sockets or threads.
The CLI and real gateway integration follow after the native controller seams.

`keys::Store::open` binds an existing mode-0700 directory outside repositories.
Every path component is opened without following links. Registry and lock files
must be regular, single-link, owner-owned mode-0600 files; unsafe existing modes
are refused rather than repaired. Updates hold a process-shared lock, write a
new private file, sync it, rename it and sync the directory. The store retains a
directory descriptor, not a bearer or cached registry.

`create(name, limits)` returns metadata and a new random 256-bit bearer, which
the eventual owner command presents once. Only a domain-separated HMAC-SHA256
digest is persisted; the domain is public, and security depends on the random
bearer's entropy. Authentication verifies tags with HMAC's constant-time
primitive and scans the complete bounded registry. `list` exposes metadata only;
`revoke` durably disables the id, retaining its name and limits. Every
authentication reads fresh storage, so an already-open handle observes revocation.
No automatic repair or recovery of malformed registries enables a key.

Names are bounded printable ASCII, ids are random, and both daily request and
token limits must be positive. At most 64 records (including revoked records)
fit in a registry of at most 64 KiB. This module stores limits; the later
listener must reserve and enforce them before forwarding. It does not authorize
cloud models, subscriptions, tools, sessions or file access.

Tests use only generated synthetic bearers and private throwaway directories,
including concurrent child processes. No real credential store is consulted.

## Linux tailnet identity adapter

`tailnet::LocalApi::new(socket, deadline)` accepts an owner-selected absolute
Unix socket path, normally `/var/run/tailscale/tailscaled.sock`. Construction
performs no I/O and starts no task. It does not inspect installed credentials,
invoke `tailscale`, use a TCP fallback, or read socket paths from guest requests
or project settings. The caller must choose a trusted local daemon socket.

`addresses()` obtains assigned addresses from a fresh `status?peers=false`
response. Only Running state with a real TUN interface is usable; missing,
malformed, wildcard, multicast or loopback addresses fail closed. The adapter
returns assigned addresses, never another interface as a fallback. Listener
address selection remains the separate listener's responsibility.

`same_owner(peer)` takes the actual transport IP, queries fresh status and
whois, and checks positive matching owner/profile ids plus an exact host prefix
for that peer. Tagged or shared nodes are refused. Unknown/malformed identity
and daemon errors never authorize access. This is an optional second factor;
it does not replace the listener's API-key authentication. Userspace networking
without a TUN interface is deliberately unsupported for direct binding.

All work uses the caller's Tokio runtime. At most eight checks are active;
excess calls fail immediately. One explicit deadline (positive, at most 30s)
covers connect/write/read and both requests in a peer check. Cancellation or
timeout drops the Unix stream and releases its slot; no worker remains. HTTP/1.0
with connection close avoids pooling/chunk decoding, while enforcing 8-KiB/64
header and 64-KiB JSON-body bounds. Redirects, transfer encodings and ambiguous
lengths are refused. Daemon bodies and socket paths are absent from diagnostics.
No listener, native gateway or CLI serve command is activated by this module.

Contract references: Tailscale's [LocalAPI client](https://github.com/tailscale/tailscale/blob/main/client/local/local.go),
[status fields](https://github.com/tailscale/tailscale/blob/main/ipn/ipnstate/ipnstate.go),
[whois response](https://github.com/tailscale/tailscale/blob/main/client/tailscale/apitype/apitype.go),
and [node ownership/address fields](https://github.com/tailscale/tailscale/blob/main/tailcfg/tailcfg.go).
Tests use synthetic Unix-socket servers only; they never contact the installed
daemon. The thin implementation of the listener's Tailnet trait follows when
that listener slice is on main.
