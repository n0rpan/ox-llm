# Security

## Reporting
Please report vulnerabilities privately through GitHub Security Advisories ("Report a vulnerability" on this repository). Do not open public issues for security problems.

## Review guidelines (used by automated security review)
Read `docs/threat-model.md` first. Report a finding only if this change weakens one of its trust boundaries; explain the trigger and the impact, and cite the changed lines.

Treat as **high** severity:
- a secret (API key, ChatGPT token, OpenCode/GitHub token, share key) printed, logged, stored outside its 0600 store, sent to the wrong host, or reachable by model tools;
- a project file or model output able to change user-only settings, enable a feature flag, or bypass the sandbox or network guard;
- metered spend that bypasses reservation or settlement, is not recorded in the ledger, or exceeds a **configured** daily cap (no configured cap means no cap check; that is owner policy, not a finding);
- fork-PR or guest access to secrets, cloud providers or owner credentials;
- an unattended run (worker, subagent, `oxa work`, headless goal) that runs without the OS sandbox while `oxa.sandbox.unattended` is not `false`;
- a dangerous command that runs without approval outside yolo;
- a classifier bypass: a command that matches a dangerous-command rule but is not caught after parsing;
- a project file or model output able to change `oxa.approval.*` or `oxa.sandbox.*`, or to answer its own approval request;
- new `unsafe`/FFI/process/network code in the engine without bounds checks.

Treat as **medium**: missing bounds on untrusted input (sizes, lengths, recursion), error messages that echo provider text, weakened validation of provider usage/billing fields.

Do not report: style, pre-existing issues outside the diff, test-only code that never runs in production, or hypothetical risks without a concrete path.

Intentional removals must be stated in the PR body with a `Removes:` line; flag silent deletion of security checks as high.
