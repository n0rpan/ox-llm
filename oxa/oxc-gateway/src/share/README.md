# Guest sharing (opt-in foundations)

The `share` feature is off by default. It supplies the private key registry and
owner key commands. Importing it creates no files, sockets or threads. Listener
and real gateway integration are separate slices.

`keys::Store::open` binds an existing mode-0700 directory outside repositories.
Every path component is opened without following links. Registry and lock files
must be regular, single-link, owner-owned mode-0600 files; unsafe existing modes
are refused rather than repaired. Updates hold a process-shared lock, write a
new private file, sync it, rename it and sync the directory. The store retains a
directory descriptor, not a bearer or cached registry.

`create(name, limits)` returns metadata and a new random 256-bit bearer, which
the owner command presents once. Only a domain-separated HMAC-SHA256
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

## Owner key commands

Build the CLI with its opt-in `share` feature. The default build has no gateway
key-store dependency enabled and refuses `share` without creating storage.

```sh
oxa share key add Tablet --daily-requests 100 --daily-tokens 100000
oxa share key list
oxa share key revoke <id>
```

Commands emit JSON. `add` prints the new `api_key` once, after durable storage;
copy it privately into the other device's client. Listing and revocation expose
metadata only. If stdout fails after creation, list the id and revoke it before
creating a replacement; the bearer cannot be recovered. Both limits are required
positive integers. Storage is `~/.oxa/share`, independent of project settings and
`OXA_AGENT_DIR`. Missing directories are created privately through anchored
no-follow descriptors, with parent syncs on creation and retry. Existing unsafe
permissions, symlinks and repository paths are refused without repair. No model,
provider credentials or listener are opened by these commands. Serving still
requires the separately coordinated native gateway/owner-priority integration.
