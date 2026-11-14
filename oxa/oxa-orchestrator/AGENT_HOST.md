# Explicit foreground agent host

The default-off orchestration feature adds an independently owned local agent:

```sh
./target/debug/oxa orchestrate-agent \
  --engine 127.0.0.1:PORT --model MODEL --local-only \
  --directory ./new-agent-host --parallel 2 --capacity 16 --lifetime 300
```

Use an explicitly verified local inference endpoint/model; loopback alone is not
proof that an inference gateway cannot forward to a paid provider. This command
always runs offline, in plan mode, with no filesystem tools or other grants. It
neither starts at application launch nor installs/reconfigures an OS service.
Run it in a separate terminal/process from a controller that may exit or restart.
Help and invalid options are inert and do not probe inference.

The host writes `<directory>/registration.json` in its new private 0700 directory.
That 0600 file contains a version, literal loopback socket address, random 256-bit
A2A capability and capability-derived instance hash. Publication uses a bounded
pending file, sync, no-replace rename and parent sync. No existing directory/file
is reused or overwritten. The capability is local-agent authentication, not an
inference-provider credential, model input, tool grant or output artifact. It is
never printed; stderr readiness is best effort/nonblocking and stdout stays empty.
Keep the registration file private. Its instance hash is an identifier, not a
replacement credential or a proof of inference policy.

## Explicit registration and reconnection

Library controllers use `registration::Registration::load(selected_path)` and
`registration.client(limits)`. Loading performs no network request. It requires a
protected output namespace and an operator-owned private plain single-link file
of at most 8 KiB, refuses symlinks/devices/FIFOs/oversize inputs, pins the inode and
checks for observed mutation. Parsing rejects unsupported schemas, non-loopback
addresses, invalid capabilities and mismatched instance binding. Errors and Debug
output do not expose authentication; the public handle has no serialization or
Display implementation. Nothing reads registration from the environment, an agent
card, a model result or a directory scan.

The operator must still assert the original agent and local/nonbillable route
when wrapping that client with `LocalAgent::register_verified_local`. The file does
not automatically confer that trust. The process hosting the original agent must
still be alive. A controller can disconnect/exit, reload that explicitly selected
registration and query/resubscribe to a known task through the existing protocol.
A2A task/context validation and recovery's no-replay rules remain in force.
`Registration::load` itself never starts or resends a task.

The library `registration::AgentHost::start(RpcAgentConfig)` offers the same
separately owned host with explicit immutable operator configuration, including
existing approved workspace grants if the caller supplies them. Dropping the host
stops handlers and joins owned tasks. Existing `Server::start` continues generating
its own ephemeral capability. The new protocol `start_with_capability` accepts
explicit operator-owned authentication for such hosts; binding remains loopback
on an ephemeral port and cards/results cannot select the capability.

## Lifetime, retained tasks and shutdown

`--parallel` caps concurrent tasks (1..16, default 4). `--capacity` caps all retained
tasks, including terminal ones (1..128, default 32, at least parallelism). A full
host refuses new work rather than evicting task identities and silently relaunching.
Per-task wall time is 1..600 seconds (default 30), accepted output is capped at
1 MiB, and reported-token cap defaults to 200000. Usage is observed, not a hard
monetary ceiling. Routes, models, executables, permission mode, tools and budgets
are fixed by operator configuration, never by remote messages.

`--lifetime` bounds the host lifetime to 1..86400 seconds, default 300, using the
boot clock so suspend consumes it. SIGINT/SIGTERM requests shutdown; the host
closes its listener/handlers, cancels active tasks and joins supervisors/children
before returning 130/143. Normal lifetime expiry also stops owned tasks. Fatal
controller-process death still has the existing bubblewrap parent-death cleanup.
No shutdown action targets a historical PID. Task artifacts may contain results;
the audit journals remain hash-only. This command does not enable raw scheduler
result checkpoints or widen any persistence consent.

Registration is stable only for that host instance. Restarting uses a new private
directory, random capability, backend task prefix and ephemeral endpoint. An old
registration file remains private for explicit retention/cleanup, but cannot
restore a dead host's in-memory tasks or authorize replay. Reused ports do not
confer the original capability. The command does not persist backend state across
host death, provide distributed identity or offer CLI controller takeover yet.
The existing integrated `orchestrate` command still owns its ephemeral internal
server; this separate command is opt-in.

## Model-free validation

Owned fake-engine tests launch the actual host command, verify no startup inference,
private/atomic registration and wrong-capability refusal, submit a real sandboxed
RPC task from an owned controller process that exits, reconnect through the same
registration and
collect its original terminal result exactly once. They then terminate the owned
host during a second task and verify exit 143, worker reaping, listener closure,
unchanged registration bytes and no capability in stdout/stderr/audit output. A second host expires normally with a
fresh instance identity and no new inference.
Unit tests cover redaction, malformed input, unsafe permissions/parents, links,
FIFOs, size limits and non-loopback routes. The rebuilt feature-enabled CLI smoke
suite runs the same host scenario. No installed model, provider key, production
service, other machine or daemon/service setting is used.
