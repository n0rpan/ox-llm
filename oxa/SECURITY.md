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

- **Command effects.** The dangerous-command rules read one parsed `Effects` value per shell command. `credential-read` (credential stores, token-printing commands, `/proc/*/environ`) and `git-hook-write` (git hooks, `git config` program keys, gitdir pointers) are approval prompts in standard mode, not a sandbox; yolo does not reach them and `oxa.approval.dangerous` can remove them. A `wait` probe that runs `gh` is network access: plan denies it, ask asks once per session.
- **Grant binding and root actions.** A desk grant of a pushing command is bound to the resolved source ref ids and a digest of origin's URLs, re-read before the run; unresolvable facts are refused for unattended replay. `oxa.approval.rootActions` is owner configuration (user settings only, interactive owner session only): it skips oxa's own `sudo` question for exactly the named command, while sudo decides whether a password is needed. The password path is `SUDO_ASKPASS` -> the broker's `oxa-askpass` -> an oxa masked dialog; it is never answered by model text, a typed chat message or yolo, and never reaches tool output, the transcript or logs.
- **Instructions carried in content.** The permission modes gate what each tool call may do. In attended standard/auto runs, dangerous commands ask but model bash uses the user's host rights by default; `--sandbox` or `oxa.sandbox.interactive` enables bubblewrap. Unattended runs keep bubblewrap by default. Plan mode remains confined and read-only.
- **Credentials.** When bubblewrap is enabled it masks SSH, cloud, git, package-registry, password-store and browser-profile paths, and oxa's own state directory. The file tools refuse them in every mode. Every model command starts with an empty environment and gets only the explicit command environment; a toolchain variable whose name looks like a secret is never passed. The Brave Search key is never logged or written to a session.
- **Publishing secrets.** Before `oxa work --publish` or a lane push, the commits to be pushed are scanned for private keys, known token formats (also after a word character), opaque `password`/`token` and `_KEY`/`credentials` assignments and quoted literals, `Bearer`/`Basic` headers, URL passwords, secret-store file content and the values of your secret-named environment variables and settings. A finding refuses the push and names the file and line, never the secret. A secret removed by a later commit still blocks, so rewrite the unpublished commits. A file is not refused by its name alone: an empty secret-store file and `.env.example` publish unless their content holds a secret. A lane push also needs your own confirmation through the session broker (or the terminal/`OXA_WORK_OWNER_CONFIRMED=1` compatibility path outside an agent), and refuses any commit it would add that changes GitHub workflows or `tools/ci/`. See [docs/work.md](docs/work.md).
- **Hostile project configuration.** A project's hooks, MCP servers, extensions, skills, prompts and system prompt files load only after you trust the project. Once trusted, oxa records a hash of each executable file, and asks again when one changes. Project files cannot widen the sandbox or enable a remote engine. Git's hooks and config, including a repository's `core.hooksPath`, stay read-only to bash; a symlinked `.git` or a `.git` file that names a writable directory is refused.
- **Unwanted network use.** The harness connects only to the local engine by default and sends no telemetry. `--offline` (or `OXA_OFFLINE=1`) refuses every other connection that oxa itself makes, and removes network from bash. A proxy that is not loopback is refused while offline unless the target is on its `NO_PROXY` list (then the connection is direct), and a redirect is not followed. Bash cannot reach the host's user session bus, systemd-resolved or container control sockets: they are Unix sockets, so the empty network namespace does not contain them, and the sandbox masks them.
- **Prompts sent to a remote host.** A non-loopback engine is refused unless the user settings allow it.
- **Crashes in one component.** A panic in a tool, extension, MCP connection or hook is contained to that call.

What oxa does not defend against:

- Commands you run yourself. `!cmd` runs with your rights and outside the bash sandbox.
- Stdio MCP servers and extensions you install. A stdio MCP server runs with your rights, outside the bash sandbox. Extensions have no ambient file, network or process access of their own. Exec hooks run in the sandbox, without network, except under yolo (also a yolo-started unattended worker), where they run with your rights like bash.
- Network use by the model's tools. With the default `oxa.sandbox.net: "on"`, a command the model runs in bash can send what bash can read to any host, and `web_search` sends its query to Brave. Offline mode, `oxa.sandbox.net: "off"` or `ask` mode limit this; none of them stops a steered model from reading what it can reach.
- Files the deny-list does not cover, such as the `.env` file of a project. Bash and the file tools can read them.
- Actions you allow: an "Allow for this session" grant applies until the run ends, and `--approve` trusts a project for one run.
- Content the model reads and then writes into a session, an export or a report. Scrubbing in `oxa report-issue` is pattern-based; read a draft before you post it. Outside yolo, publication needs the owner's own approval of the exact text shown: a key press in the approval dialog, or a `yes` the owner typed into the interactive input; text from tools, files, the web or other sessions never counts. In yolo it posts without asking and shows the posted text, but a draft the scrubber would change (a secret) is never posted.
- Writable package caches. Files the model writes there are used by your next build.
- The engine, the model weights, the operating system and the `bubblewrap` binary. A flaw in those is outside oxa, though oxa's sandbox relies on them.
- Side channels such as timing, and anything reachable by a user who already has access to your account.

The details of each control, and the list of what the sandbox exposes, are in [docs/pages/security.md](docs/pages/security.md).
