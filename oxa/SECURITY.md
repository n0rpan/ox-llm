# Security policy

This file covers `oxa`, the coding agent of ox-llm. Report problems in the engine (`oxe`) or the controller (`oxc`) through the same repository.

## Reporting a vulnerability

Report vulnerabilities through the private vulnerability reporting of the ox-llm repository. Do not open a public issue, and do not post details in a public discussion, until a fix is released.

Include the oxa version (`oxa --version`), the permission mode and the steps that reproduce the problem. Do not include API keys, tokens or session files; describe them instead.

## Supported versions

The current release is v0.2.0, part of ox-llm.

## Threat model

oxa is a local agent: a language model reads your project, runs commands and writes files on your machine. The model's input includes text it did not write: files in the project, web results from `web_search`, and output from MCP servers. Any of it can try to steer the model.

What oxa defends against:

- **Instructions carried in content.** The permission modes gate what each tool call may do. In `workspace` mode, writes outside the project and extension commands that ask for network are asked for, bash runs in the sandbox, and `plan` mode is read-only.
- **Credentials.** The bash sandbox masks SSH, cloud, git, package-registry, password-store and browser-profile paths, and oxa's own state directory. The file tools refuse them too. A command starts with an empty environment and gets only a short list of variables; a toolchain variable whose name looks like a secret is never passed. The Brave Search key is never logged or written to a session.
- **Hostile project configuration.** A project's hooks, MCP servers, extensions, skills, prompts and system prompt files load only after you trust the project. Once trusted, oxa records a hash of each executable file, and asks again when one changes. Project files cannot widen the sandbox or enable a remote engine. Git's hooks and config, including a repository's `core.hooksPath`, stay read-only to bash; a symlinked `.git` or a `.git` file that names a writable directory is refused.
- **Unwanted network use.** The harness connects only to the local engine by default and sends no telemetry. `--offline` (or `OXA_OFFLINE=1`) refuses every other connection that oxa itself makes, and removes network from bash. A proxy that is not loopback is refused while offline unless the target is on its `NO_PROXY` list (then the connection is direct), and a redirect is not followed. Bash cannot reach the host's user session bus, systemd-resolved or container control sockets: they are Unix sockets, so the empty network namespace does not contain them, and the sandbox masks them.
- **Prompts sent to a remote host.** A non-loopback engine is refused unless the user settings allow it.
- **Crashes in one component.** A panic in a tool, extension, MCP connection or hook is contained to that call.

What oxa does not defend against:

- Commands you run yourself. `!cmd` runs with your rights and outside the bash sandbox.
- Stdio MCP servers and extensions you install. A stdio MCP server runs with your rights, outside the bash sandbox. Extensions have no ambient file, network or process access of their own. Exec hooks run in the sandbox, without network.
- Network use by the model's tools. With the default `oxa.sandbox.net: "on"`, a command the model runs in bash can send what bash can read to any host, and `web_search` sends its query to Brave. Offline mode, `oxa.sandbox.net: "off"` or `ask` mode limit this; none of them stops a steered model from reading what it can reach.
- Files the deny-list does not cover, such as the `.env` file of a project. Bash and the file tools can read them.
- Actions you allow: an "Allow for this session" grant applies until the run ends, and `--approve` trusts a project for one run.
- Content the model reads and then writes into a session, an export or a report. Scrubbing in `oxa report-issue` is pattern-based; read a draft before you post it.
- Writable package caches. Files the model writes there are used by your next build.
- The engine, the model weights, the operating system and the `bubblewrap` binary. A flaw in those is outside oxa, though oxa's sandbox relies on them.
- Side channels such as timing, and anything reachable by a user who already has access to your account.

The details of each control, and the list of what the sandbox exposes, are in [docs/pages/security.md](docs/pages/security.md).
