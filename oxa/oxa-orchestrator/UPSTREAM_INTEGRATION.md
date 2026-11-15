# Integration preview on current upstream

This document records draft #159's main-based integration. The cloud dependency
validation branch applies that final patch to a different pinned base; see
[CLOUD_GATEWAY_DEMO.md](CLOUD_GATEWAY_DEMO.md) for its scope and evidence.

This branch replays the draft A2A/orchestrator stack onto upstream `b568f52`.
Published draft branches remain intact; nothing is merged. The replayed baseline
is `5e95e94`. The integration changes after that baseline are the focused review
range for API compatibility and #60 background-command support. The complete
branch also includes the earlier, still-unreviewed orchestration milestones.

## Compatibility changes

- Preserve upstream's TUI cache reset while adding worker footer rows; this was
  the only textual replay conflict.
- Adapt test ChatRequests to `sampling` and the reopen fixture to the new current
  model/thinking argument. Engine decorators continue forwarding requests intact.
- Read the first complete worker/background wake JSON envelope. Core now appends
  its environment note to user wire messages; requiring JSON at the end left a
  delivered result unacknowledged and its parent waiting until time exhaustion.
  The shared-TUI suite failed that workflow before the parser adaptation and
  passes afterward. Actual process cleanup remains required for acknowledgment.

## GitHub command capability

Upstream #60 makes the GitHub capability private to online sandboxed commands.
The old background adapter prepared argv directly and bypassed that credential
transport and output redaction. A synthetic actual-bubblewrap regression produced
`missing missing` before the change instead of `[redacted] github.com`.

The new `oxa-owned-command` crate carries opaque prepared command resources. The
feature-gated Bwrap adapter separates secret environment data from public argv and
passes it through a bounded private argument descriptor. NetSandbox applies its
existing clamp and registered GitHub capability, then attaches redaction. The
existing orchestrator runner still owns spawn, process-group identity, boot-clock
deadlines, output caps, cancellation and reaping. It filters stdout/stderr before
retaining capped bytes. No credential is placed in model/tool arguments, task
status, worker profiles or argv; unrelated execs cannot inherit the descriptor.

Tests cover enabled/disabled/offline/no-network cases, stdout chunk splits with
stderr interleaving, cross-pipe fragments joining at EOF, filtering before
truncation, descriptor lifetime and
close-on-exec isolation, and oversize refusal. They use a synthetic capability;
no actual credential, keyring or GitHub API is used. This integrates the capability
path, not a live authenticated GitHub acceptance run.

## Existing-crate hooks (all optional/off by default)

The complete branch's hooks relative to upstream are:

- Workspace manifests/lockfile: new crates and optional dependencies.
- `oxa-cli`: `orchestrator` feature, opt-in command dispatch and dependency augmentation.
- `oxa-interactive`: `session-control` feature; bind/admit/observe/idle-drive/status
  attachment hooks and idle Esc forwarding.
- `oxa-proto`: `owned-background` feature, prepared-capability alias and an
  unsupported-by-default sandbox preparation method.
- `oxa-sandbox`: `owned-background` feature; Bwrap refusal/preparation and
  NetSandbox clamp/private-capability/redaction adapters. Default run paths remain
  upstream's implementation.
- The network-surface scanner retains the earlier explicit A2A literal-loopback
  path entries; its detection rules and planted-violation self-tests are unchanged.

Protocol, supervision, control, budgeting, events, worker lanes and command
resource/filter logic live in the new crates. No Core, config, controller,
workflow-setting, production-service or installed-binary changes are made.

## Verification

- Full feature orchestrator suite, including shared TUI and actual native workers.
- New prepared-command crate tests; proto/sandbox tests with `owned-background`.
- Strict all-target Clippy, formatting, default-feature CLI check and network
  surface scanner/self-tests.
- Real feature-built CLI in tmux: shared steering, visible idle Esc cancellation,
  a second worker completing and waking the same parent through a Core environment
  note, session rebinding, main compaction and exact usage. The compacted goal's
  ledger was 180,452 tokens over three chats, two summaries and two zero-output
  prefills; Ctrl-D exited 0. See `tools/live_tui_demo.py`.
- The isolated lab proxy target: 25 tests passed. #151's older public run failed
  in its asynchronous transcript-row assertion; upstream already changes that
  test to wait for both completed rows, and this integration inherits that fix
  without editing lab files. Only the failed old CI job was rerun diagnostically;
  attempt 2 passed at the unchanged #151 head. #155 also has green public CI.

This needs **T3 review** for the optional credential/descriptor path, alongside
coordinator and T2/T3 review of the existing stack. Public CI, actual cloud
continuation recovery (#64), monetary settlement (#47/#48/#71), live GitHub auth
and the working-evening cloud acceptance remain separate gates. No paid model or
real provider was contacted; the brief still requires Oscar's go for a cloud run.
