# Prepared owned commands (experimental)

An opaque command capability for the default-off orchestrator background runner.
Sandbox policy remains in `oxa-sandbox`; process ownership, deadlines, output
bounds and group cleanup remain in `oxa-orchestrator`. This crate supplies only
prepared arguments, private descriptor leases and per-pipe output filtering.

`GH_TOKEN` is supplied by the existing online `NetSandbox` capability. The
prepared command keeps it out of argv using bubblewrap's private `--args` input.
The parent descriptor is close-on-exec; only the prepared child's pre-exec path
clears that flag. The returned command owns its descriptor lease through spawn,
including when the original preparation object is dropped. Unrelated native
workers cannot inherit the descriptor through exec. Private argument data is
bounded to one atomic pipe write; invalid or oversized values are refused before
spawn. The type deliberately has no Debug or serialization implementation.

Each output pipe has independent exact-match redaction state, retained across
read boundaries and flushed at EOF. A second filter covers the retained merge,
including incomplete pieces from separate pipes joining into a complete value.
The runner filters before applying its output
cap, so truncation cannot cut a raw full credential into a retained prefix.
These are the same exact-match rules as the upstream `NetSandbox` redactor;
encoded/transformed output is outside that rule. Network permissions, offline
clamping, approval and workspace restrictions remain authoritative.

All tests use synthetic data. They check chunk splits and interleaved pipes,
private descriptor transport, non-inheritance by another process, lease lifetime,
parent close-on-exec flags, oversized input and actual sandboxed background
execution. No real GitHub token, keyring or remote API is read or called.
