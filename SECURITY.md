# Security

## Reporting
Please report vulnerabilities privately through GitHub Security Advisories ("Report a vulnerability" on this repository). Do not open public issues for security problems.

## Review guidelines (used by automated security review)
Read `docs/threat-model.md` first. Report a finding only if this change weakens one of its trust boundaries; explain the trigger and the impact, and cite the changed lines.

Treat as **high** severity:
- a secret (API key, ChatGPT token, OpenCode/GitHub token, share key) printed, logged, stored outside its 0600 store, sent to the wrong host, or reachable by model tools;
- a project file or model output able to change user-only settings, enable a feature flag, or bypass the sandbox or network guard;
- metered API requests that bypass required reservation or settlement, are missing from the ledger, or bypass a **configured** daily cap. With no configured cap there is no cap check for the owner or authenticated allowlisted share guests. Guest authority, tokens and request quotas still apply;
- OpenCode Go requests that bypass subscription token accounting, or provider-reported paid charges that are missing from the ledger or shared daily spend. Go admission reserves subscription tokens only. The owner's local metered API cap does not apply to Go admission, including provider-side fallback to a paid balance;
- fork-PR or guest access to secrets, cloud providers or owner credentials;
- new `unsafe`/FFI/process/network code in the engine without bounds checks.

Treat as **medium**: missing bounds on untrusted input (sizes, lengths, recursion), error messages that echo provider text, weakened validation of provider usage/billing fields.

Do not report: style, pre-existing issues outside the diff, test-only code that never runs in production, or hypothetical risks without a concrete path.

The repeat guard warns and continues by owner decision (#1448): the third identical tool result gets a notice, a call that still repeats gets one persisted warning, and the run is not stopped. A finding that asks for the old stopping behaviour is not a finding; `oxa.maxTurns` is the configured cap.

Dangerous commands are a floor prompt in the interactive run, and an unattended run (a worker, a subagent, a headless goal: no one to ask) never waits for an answer (#1512). `oxa.approval.unattended` is the owner setting, user settings only: `auto` (the default) refuses the command under the OS sandbox and files an approval request without one, `refuse` always refuses, `request` always files a request. Requests live in `<agent dir>/approvals/` (directory 0700, one record per request at 0600, written atomically under a private temporary name, never following a symlink or an agent directory owned by another user) and are answered only by the owner through `oxa send <session> --approve <id> once|session|always|deny` or the lead web page, never by inbox text or model output. A `once` grant is consumed by the run that retries the command; `session` and `always` grants persist, and `always` is recorded in the user-only `oxa.approval.always` list. A finding that asks for a default-off switch, for an unattended run to wait indefinitely, or for the desk to take an answer from a model or a project file is answered here.

Intentional removals must be stated in the PR body with a `Removes:` line; flag silent deletion of security checks as high.
