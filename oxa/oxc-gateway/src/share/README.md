# Guest sharing (opt-in foundations)

The `share` feature is off by default. It supplies private keys, durable guest
admission and an explicit HTTP listener. There is no CLI command or real gateway
adapter yet. Importing it creates no files, sockets or threads. The CLI and real
gateway integration follow after the native controller seams.

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
listener reserves and enforces them before forwarding.

`Store::admit` verifies fresh credentials under the same lock as quota updates.
It reserves one request and the possible input/output tokens durably in a private
per-UTC-day file. Its single-use guard settles only trusted usage within the
reservation; unknown outcomes and process exit retain the conservative charge.
In-flight settlement stays on its admission day across midnight, even after key
revocation. There are at most 128 unresolved reservations per day; exhaustion or
malformed/unsafe state refuses work. Request admission uses nonblocking locks and
returns Busy rather than parking an I/O thread behind another process.

`listener::Server` uses its caller's Tokio runtime. A fakeable `Tailnet` adapter
supplies assigned addresses and optional peer ownership checks; absent/ambiguous
or unspecified discovery never falls back to wildcard. `Binding::Explicit` is
the owner's deliberate override, including loopback for `tailscale serve` TLS
termination. This module does not read certificates or call a real Tailscale API.
Forwarded headers do not provide peer identity.

Only authenticated GET `/v1/models` and streaming text POST
`/v1/chat/completions` are served. The `Answer` adapter receives only validated
guest history and sampling/output limits. Tools, tool messages, file/image
attachments, session fields and unknown execution fields are refused. Local
models are visible by default; metered remote aliases require a separate owner
allowlist. Subscription routes are always excluded. No owner prompt, tool set,
session store or provider credential enters a guest request.

Headers are capped at 16 KiB/64 fields, bodies at 1 MiB, live connections at the
configured bound (default eight), peer/key rate tables at 256/64 entries, and
stream buffering at four 64-KiB text fragments. Read/write deadlines and the
request deadline are explicit. The backend must honor cancellation and retain
ownership until its work stops: disconnect, shutdown and deadline request
cancellation and join cleanup instead of abandoning generation. Streaming
connections accept one request and treat client EOF/pipelining as cancellation.
Cancel and await `serve` for orderly shutdown; do not detach it or its backend.

Real gateway routing, owner-priority local leases, native request-ledger rows,
CLI key commands, actual tailnet detection/LocalAPI, and a status adapter are
subsequent wiring. The counters here are quota authority, not a second copy of
cloud billing or a complete request ledger. TLS can terminate at `tailscale
serve`; direct certificate support is not implemented in this slice.

Tests use only generated synthetic bearers and private throwaway directories,
including concurrent child processes. No real credential store is consulted.
