# Native Bash lifecycle finding and release guard

The post-release lifecycle bridge is tracked in
[issue 258](https://github.com/oxstack/ox-llm/issues/258), labelled `post-0.2`.
Coordinator decision: until v0.2.0 ships, A2A sessions use only
`background_command` for detached work. Do not change the native JobRegistry on
the release path.

## Current mitigation

Build the feature-enabled CLI and fake, then run from `oxa/`:

```sh
cargo build -p oxa-cli --features orchestrator -p oxa-fake
python3 -I oxa-orchestrator/tools/native_jobs_policy_demo.py
```

The real TUI receives native Bash detached-start and job-read calls, both of
which must be refused. A foreground Bash command must still complete normally,
and the same goal reaches DONE without creating an unmanaged job. The test uses
owned fakes, a private HOME and a cleared environment. It fails without the A2A
policy adapter and passes with it. Existing background_command integration tests
cover the supported detached path, result wakes, cancellation and reaping.

## Historical reproduction

With the CLI built from pre-policy integration `fc551b2`, run:

```sh
python3 -I oxa-orchestrator/tools/native_jobs_demo.py
```

The original diagnostic starts a bounded native Bash job that writes a heartbeat.
The model declares DONE while the heartbeat keeps growing (observed 10 to 35
bytes after DONE). The native registry lacks the goal ownership, completion/list
and terminal-result acknowledgement hooks needed by the A2A owner. Its output
progress counter is not terminal-result delivery acknowledgement.

The current mitigation prevents that diagnostic from starting its job; that is
a refusal policy, not a completed lifecycle bridge. The issue specifies the
post-release ownership, cancellation, completion/wake and unread-result design
and its required race/cleanup tests. No paid provider or production service is
used by either diagnostic.
