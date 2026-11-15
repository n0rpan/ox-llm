# Native Bash job lifecycle diagnostic

Build the actual feature-enabled CLI and owned fake, then run from `oxa/`:

```sh
cargo build -p oxa-cli --features orchestrator -p oxa-fake
python3 -I oxa-orchestrator/tools/native_jobs_demo.py
```

The tool uses a private HOME, a cleared TUI environment, an owned loopback engine,
and the real `bash` tool with `background: true`. A bounded command writes a
heartbeat. The model then declares the goal done. The diagnostic records the
shared-session status, terminal frame and two heartbeat sizes. It exits nonzero
if the goal reaches DONE while the heartbeat continues; a refused command cannot
pass the check. Cleanup closes the isolated TUI and fake server. The heartbeat
also expires independently after about eight seconds.

At integration `b9d8574`, this diagnostic fails: the native per-session job
registry introduced by main is separate from the A2A `background_command` manager.
A2A's pending-result guard, cancellation and completion wakes currently cover its
own background commands and workers. They do not yet establish native Bash job
ownership. A running native job can therefore coexist with a DONE goal. Do not
interpret the passing A2A background suite as evidence for native-job lifecycle
integration.

The coordinator decision is pending: add a minimal default-off native-registry
lifecycle bridge, or constrain the A2A tool policy until after release. This
standalone diagnostic is deliberately outside automatic test discovery while
that product decision is pending. Its failure must remain visible; there is no
expected-failure switch. It uses no paid provider, credentials or production
service, and proves no real billing behavior.
