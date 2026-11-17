# ox-llm threat model

Used by automated security review (Codex) and by human/agent reviewers. Keep it short and current.

## What we protect
- **Credentials:** provider API keys, the ChatGPT sign-in token record (access + refresh token), OpenCode keys, GitHub App keys/tokens, share/guest keys. They live in user-only files (mode 0600) and the protected key store.
- **The user's machine:** files outside the workspace, the user's shell, other processes, the network.
- **Money and quotas:** metered provider spend, daily budget caps, subscription allowances (ChatGPT, OpenCode Go).
- **Private data:** prompts, sessions, transcripts, usage ledgers, local model state.

## Who may be hostile
- **Model output** (any provider, any model): may try to read secrets, escape the sandbox, call tools it should not, or smuggle secrets into tool arguments.
- **Repository content and project settings** (`.oxa/`, project settings files, AGENTS.md, skills, MCP config in a cloned repo): untrusted unless the user trusts the project.
- **Upstream providers and the network:** malformed or hostile streams, oversized responses, redirects, wrong usage/billing fields.
- **LAN share guests** and **outside contributors** (fork PRs on this public repo).

## Trust boundaries (must hold)
1. **Secrets never leave their store:** never printed, logged, put in prompts, tool output, session files, telemetry, test fixtures, CI logs or error messages; redaction covers streamed text and decoded tool arguments.
2. **User-only settings:** a project file can never change engine transport, native binary/provider selection, `allowRemote`, sharing, login, feature flags, or budgets.
3. **Sandbox:** model tools run under bubblewrap with private paths denied (credential files, `~/.oxa` private dirs, ledgers); no new escape routes (bind mounts, env passthrough, `/proc`, sockets, setuid helpers).
4. **Offline mode** makes no network calls at all.
5. **Network guard:** the engine endpoint is loopback unless the user enables remote in user settings; no credential is sent across a redirect or to a host it was not issued for.
6. **Budgets and ledgers:** every metered request is reserved before sending and settled from validated provider usage; unknown usage stays reserved; caps refuse the next request; ledger rows are append-only and validated.
7. **Guests and outside PRs:** LAN-share guests never reach cloud providers or owner credentials; fork PRs never get secrets, tokens or token-spending reviews (`pull_request_target` is forbidden).
8. **Native engine (oxe):** the HTTP server listens on loopback only; `unsafe`, FFI (kernels, NPU), process spawning and file permission changes need explicit review.
9. **No Python in the product path** (v0.2.1+).

## Most security-relevant paths
`oxa/oxc-gateway/src/` (providers, auth, ledger, redaction, share), `oxa/oxa-cli/src/{login_cmd,in_process,dispatch}.rs`, `oxa/oxa-sandbox/`, `oxa/oxa-perm/`, `oxa/oxa-proto/src/guard.rs`, `oxa/oxa-tools/src/bash.rs`, `engine/crates/{server,kernels-sys,image-npu,oxn}/`, `.github/workflows/`, `packaging/`.
