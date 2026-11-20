# Security

oxa is a local agent. The model reads your project, runs commands and writes files on your machine, and it can be steered by any text it reads, including files in the project, web pages and the output of MCP servers. The controls below limit what a model can do and reach. They do not make a model safe to run on untrusted input without supervision, and they do not limit what you do yourself.

The short threat model is in [SECURITY.md](../../SECURITY.md) at the repository root.

Execution context matters independently of the permission mode. Interactive CLI sessions do not use an OS sandbox by default; dangerous commands ask for your approval. Unattended runs without yolo keep the Bubblewrap sandbox by default. Yolo never uses an OS sandbox, including for unattended work it starts. In every context, the in-process guards remain: secret masking, private-path denials, user-only settings, and offline mode.

## Permission modes

Every tool call has an access class: `Read`, `Write`, `Exec` (a command), `Net` (a host) or `Mcp` (an MCP tool, read or write). The mode decides what happens to each class.

| Access | `plan` | `workspace` (default) | `ask` |
|---|---|---|---|
| read inside the project | allow | allow | allow |
| read outside the project | ask | ask | ask |
| write inside the project | deny | allow | ask |
| write outside the project | deny | ask | ask |
| bash, read-only command, no network | allow | allow | ask |
| bash, other command, no network | deny | allow (context determines OS sandboxing) | ask |
| extension `exec` with `net: true` | deny | ask | ask |
| `web_search` | deny | allow, no prompt | ask |
| MCP tool that its server marks read-only | allow | allow | ask |
| MCP tool, other | deny | ask | ask |

In `workspace`, bash runs without a routine permission question. Interactive sessions still ask before dangerous commands and run bash without an OS sandbox; unattended non-yolo runs use the sandbox described below;an interactive run can opt into it with `--sandbox` or `oxa.sandbox.interactive`. Bash has network access in `workspace` and `ask` modes by default (see [Network](#network)). An extension `exec` that asks for network is asked for in `workspace` and `ask`, and refused in `plan`.

- When oxa asks, it offers "Allow once", "Allow for this session", and "Deny". A session grant covers the same path directory, command, host or tool for the rest of the run. Grants are kept in memory and never written to disk. A grant never overrides a deny.
- Plan mode is for reading and planning. It refuses writes, the `subagent` tool, `web_search`, and every command that is not read-only.

Paths are resolved before the check: `..` is followed and a symbolic link that points outside the project counts as outside.

Capped tool results are an exception to the outside-project read question: the file tools can read the running session's full-output spill directory without asking, in every mode, including `ask` and unattended runs. This recovers output the session already received, not a new permission to run the tool or read its original inputs. Forks and clones also read their source sessions' spills without asking, following the saved session ancestry so inherited output paths keep working after a resume. Those inherited directories are read-only to the file tools; unrelated sessions and the rest of the agent state remain denied except in full-access `yolo`. The bash sandbox is unchanged. When another action needs approval in an unattended run, the refusal says nobody is available to approve it, rather than claiming a user declined.

Pasted and dropped images saved as `tmp/paste-<hash>.<image extension>` remain readable with the ordinary outside-project approval (or an existing session grant / auto), not the no-prompt spill exception. This read-only exception covers generated image names directly under the temporary directory, not unrelated sessions, arbitrary temporary files or symlinks into denied state.

### standard, auto and yolo

[modes.md](modes.md) is the short guide to these modes. On top of the table, the permission mode decides who answers the questions:

| Mode | Prompts | Bash | File tools |
|---|---|---|---|
| `standard` | the table above asks you; dangerous commands ask interactively | no OS sandbox interactively; sandboxed when unattended | deny-list and protected paths enforced |
| `auto` (default) | routine table questions allowed and counted; protected configuration and dangerous commands still ask interactively | no OS sandbox interactively; sandboxed when unattended | deny-list and protected paths enforced |
| `yolo` | none | **without the sandbox**, in your own environment | in-process security guards remain |
| `plan` | as in the table | read-only commands | no writes |

- `auto` is the default of a fresh install. It removes routine prompts, but interactive dangerous commands still require approval. Interactive bash has no OS sandbox; unattended non-yolo bash keeps it. The file tools still refuse `~/.ssh`, `~/.gnupg`, cloud and git credentials, `~/.config/oxc`, browser profiles and the agent directory, `.git/hooks` and `.git/config` are never written, and a write to the project's `.oxa` configuration still asks.
- `yolo` removes permission prompts and OS sandboxing. Bash runs as you, with the files, credentials and network available to the process, but the in-process guards listed above remain. It hard-refuses publishing secrets and an OS-breaking wipe; the owner's explicit confirmation passes other dangerous commands. Yolo also applies to every worker, subagent, headless session and unattended run it starts. Use it only on a machine and project you can restore. With `--offline` bash still has no network.
- Set the mode at start with `--standard`, `--auto`, `--yolo` or `--permission-mode <mode>` (one run), or with the `permissions.defaultMode` setting (`standard`, `auto`, `yolo` or `plan`). `/mode <mode> --save` and "make default" in the `/mode` picker write that setting. A project's settings can lower the mode (for example to `standard`) but never raise it.
- In a session: `/mode` opens the picker, `/mode <mode>` switches, `/auto` switches to auto, `/yolo [on|off]` or Ctrl+Alt+O toggles yolo (off returns to the mode before it), and Alt+M cycles standard, auto and yolo. `/mode workspace` and `/mode ask` still set the table mode.
- The status bar always shows the mode. In yolo it shows a flame, `YOLO full access` and the count of auto-approved actions, the frames turn to the theme's hot colours, and the first switch in a session prints a one-line notice. In print, JSON and RPC mode the mode is written to stderr at start.
- Subagents inherit the mode the parent has when they start. Running background jobs also keep their launch-time rights: switching modes affects subsequent commands, not processes already started. Stop an existing job with `bash {"job": "job-N", "kill": true}` or close the session.
- The auto-approved count records questions answered with allow, once each. A denial bypassed by full access is not an approval and does not increment it (#443).
- Plan mode and project trust work the same in every mode.

### Owner yes

An approval comes only from you: your typed message, your answer to an oxa dialog, or yolo chosen for the session. Text the model read (tool output, files, web pages, other sessions) never counts. oxa commands that need your yes (for example publishing) ask the session through a private unix socket (`OXA_CONFIRM_SOCK`, directory 0700, socket 0600). In yolo the session answers yes and records it with a notice. In standard and auto it shows a dialog; while one waits, your next typed message answers it (`yes`, `y`, `ja`, `ok`, `post it` mean yes, anything else no). A typed message answers only a question whose dialog oxa has already shown you. The dialog marks the question as text from the process that asked, not as an oxa message. A command the model starts cannot make its own listener count: an oxa command accepts an answer only from the sandbox's pinned socket or from the session's own oxa process (checked on Linux through the socket's peer credentials). Yolo is unchanged: the model already has your rights. With no way to ask you, the answer is "none" and nothing is confirmed. The socket is reachable by any command the model runs, so what one request can hold in the oxa process is bounded: its size (default 8 MiB), the time to send it (default 60 seconds) and the requests served at once (default 64; more are answered "none"). You can change each with `OXA_CONFIRM_MAX_REQUEST_BYTES`, `OXA_CONFIRM_READ_TIMEOUT_SECS` and `OXA_CONFIRM_MAX_CONNECTIONS` ([cli.md](cli.md#environment-variables)).

The socket accepts one newline-terminated request up to the size limit (default 8 MiB). A larger request receives `none`.

A shell alias makes yolo quick to start:

```console
$ alias yoxa='oxa --yolo'
```

## The bash sandbox

On Linux, unattended non-yolo bash runs in a `bubblewrap` (`bwrap`) sandbox. Without a usable `bwrap`, the bash tool is disabled for those runs and oxa says so at start-up. Interactive CLI bash runs without an OS sandbox by design and relies on dangerous-command approval plus the in-process guards; a missing or unusable `bwrap` does not disable model bash for an attended interactive session unless the owner requested the OS sandbox. Oxa warns at start-up; plan commands and non-yolo project hooks still require `bwrap`. Yolo never uses `bwrap`, including for workers, subagents, headless sessions and other unattended work it starts. This includes ownerless RPC and headless runs that start or resume a pursuing goal. An interactive run can opt into it with `--sandbox` or `oxa.sandbox.interactive`. Exec hooks keep their dedicated sandbox and no-network policy except under yolo. Host bash uses a cleared environment with toolchain variables and keeps GitHub output redaction. Network-disabled model bash also masks host runtime IPC.

Off Linux (macOS) there is no OS sandbox. Bash and exec hooks run as `bash -c` with your full user rights, every result is flagged as unsandboxed, and oxa says so at start-up and to the model. The guards that live inside oxa stay: the file tools' denials and protected paths, credential masking, permissions and trust, and `--offline` for oxa's own provider and network calls. Bash gets the same cleared base environment as the Linux sandbox (a fixed `PATH`, `HOME`, `LANG`, `TERM`, toolchain variables), so credential variables of the oxa process are not inherited. A refused bash network cannot be enforced there: when `--offline` asks for one, or `oxa.sandbox.net: "off"` in an unattended run, bash is disabled for that run (start-up and the model's environment note say so) instead of running a command with the network the owner turned off. In an interactive session `oxa.sandbox.net: "off"` keeps bash and warns at start-up that the network is not restricted. With the network on, commands run as you and every result is flagged unsandboxed.

What a command sees:

| Part of the machine | Inside the sandbox |
|---|---|
| `/` (system files, toolchains, and any other path your account can read) | read-only |
| your home directory | empty, except the entries below. The exception is a session started in the home directory or above it, see [Starting in your home directory](#starting-in-your-home-directory) |
| `/tmp` | the session's own scratch directory, kept between its commands (see below) |
| the project directory | read-write (read-only in plan mode) |
| your toolchains: `PATH` entries under home, rustup, mise and asdf, nvm and similar, uv, pipx, pyenv and rbenv, git configuration | read-only |
| package caches of pip, uv, go, cargo registry, npm, pnpm, yarn, bun, deno | read-write |
| `oxa.sandbox.readPaths`, `oxa.sandbox.writePaths` (user settings, `~/` expanded) | read-only, read-write |
| `.oxa`, `.agents/skills`, `.git/hooks`, `.git/config` inside a writable project | read-only |
| a project's `.git` file (a worktree), `.git/config.worktree`, and a missing `.git/hooks` or `.git/config` | read-only; a missing one cannot be created (see below) |
| the deny-list below | masked: an empty read-only tmpfs over a directory, `/dev/null` over a file |
| host runtime IPC: `/run/user/<uid>`, `/run/dbus`, `/run/systemd/resolve`, Docker/Podman sockets, name-service sockets | masked, except the sockets you list in `oxa.sandbox.sockets` for a network-enabled command (user settings only, default none; a start notice names each, and the docker socket gives root on the host): they are Unix sockets, not network sockets, so the network namespace does not contain them; online, only the resolver’s regular DNS configuration files remain readable |
| environment variables | none, except the short list below |
| file ownership | only your own uid is mapped; root-owned system files (for example `/usr/bin/python3`) show as uid 65534 (`nobody`), and `id` shows your uid plus group `nobody` |

Tools that insist on "owned by root or the caller" (stat/ownership checks, some wrappers, sudo-like or ssh strict-mode checks) fail only inside the sandbox because of this uid mapping. That is an artifact of the user namespace, not a repo bug, and no flag maps host root without extra privilege; report such failures as environmental.

The same mapping limits what a command can write as metadata. Setting a POSIX ACL (`setfacl`, or `fsetxattr` on `system.posix_acl_access`) that names a user id or group id other than the mapped ones fails with `EINVAL`, because the kernel cannot represent an unmapped id in the namespace; the same goes for `chown` to another id. Named entries for your own uid and gid, and ACLs with only owner, group, mask and other entries, work. A test or script that needs a foreign named uid in an ACL has to run outside the sandbox.

The environment of a command starts empty. It gets `PATH`, `HOME`, `LANG`, `TERM` and `AI_AGENT`, the toolchain variables that are set (for example `CARGO_HOME`, `CARGO_TARGET_DIR`, `RUSTUP_HOME`, `GOPATH`, `NVM_DIR`, `UV_CACHE_DIR`, and the `MISE_*` variables), and the variables oxa sets for the session (`OXA_SESSION_ID`, and `OXA_SESSION_FILE` unless the session is ephemeral). Every other variable is dropped, including your API keys and tokens. A toolchain variable whose name contains `TOKEN`, `SECRET`, `KEY`, `PASSWORD`, `PASSWD`, `CREDENTIAL`, `AUTH`, `COOKIE` or `SESSION` is never passed.

To reuse a Cargo cache across worker worktrees, set `CARGO_TARGET_DIR` in the
environment that starts the workers and add that dedicated directory to
`oxa.sandbox.writePaths` in user settings. The sandbox mounts the directory
through the user setting.

### Starting in your home directory

If oxa starts in your home directory, in `/`, or in a parent of your home directory (for example `/home`), the project directory is not a project. The whole directory is then the bash working directory, and:

- a start in `/` or a parent of your home directory is read-only for bash, whatever the settings;
- a start in your home directory itself is read-only for bash, unless you confirm the interactive question below or the user setting `oxa.sandbox.allowHomeWrite: true` is set. Then your home directory is the workspace: bash can write in its folders and files (for example `~/project`), while the home folder itself (so nothing new appears next to your dotfiles), every dotfile and dot-folder (`.bashrc`, `.profile`, `.zshrc`, `.config`, `.local/bin`, `.ssh`, `.oxa`, `.claude`, `.codex`, ...), symlinks and `~/bin` stay read-only, and the deny-list stays masked. One notice at the start says so ("workspace = your home directory"). Create a new top-level folder with the file tools or outside oxa. The file tools ask before writing the shell startup files, `~/.local/bin`, `~/.config/{systemd,autostart,hypr,omarchy,...}`, `~/bin` and the agents' settings, also in `auto` mode (not in `yolo`). In print, JSON and RPC mode only the setting makes the home writable;
- your home directory is visible to bash, read-only, with the deny-list below still masked. A command can read your shell history, your dot files and your other files there;
- bash has network access by default, so a command can send what it reads to a host (see [Network](#network)).

oxa refuses such a start in print, JSON and RPC modes. In interactive mode it warns and asks "Start oxa here anyway?", and the default answer is No. The user setting `oxa.allowHomeCwd: true`, or the environment variable `OXA_ALLOW_HOME_CWD=1`, allows the start without the question. An explicit `--yolo` or `--auto` on the command line is the owner's opt-in: the start warns once and goes on in every mode, with no question. Start oxa in a project directory instead.

Writable package caches are shared with your own builds. A crate or Go module that the model unpacks there is what your next build reads. Clear a cache after a session you do not trust.

### The deny-list

These paths are masked inside the sandbox, and the file tools refuse them in `standard`, `auto` and `plan`, whatever the grants or settings say. The single deliberate exception is user-selected `yolo` outside plan mode: its bash sandbox and file-tool permission floor are off. `grep`, `find` and `ls` on a folder above them (your home directory, `~/.config`) skip them: their names and contents are not shown. The list is fixed. Each entry is a path under your home directory, except oxa's own agent directory, which is masked wherever `OXA_AGENT_DIR` puts it.

- SSH, GPG, and cloud credentials: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.kube`, `~/.vault-token`, `~/.terraform.d/credentials.tfrc.json`, `~/.s3cfg`, `~/.config/rclone`, `~/.config/hcloud`, `~/.config/doctl`, `~/.config/sops/age`.
- Git and package-registry credentials: `~/.git-credentials`, `~/.config/git/credentials`, `~/.config/gh` (see below), `~/.config/hub`, `~/.netrc`, `~/.npmrc`, `~/.pypirc`, `~/.cargo/credentials`, `~/.cargo/credentials.toml`, `~/.gem/credentials`, `~/.m2/settings.xml`, `~/.gradle/gradle.properties`, `~/.config/github-copilot`.
- Container registry logins: `~/.config/containers/auth.json`, `~/.local/share/containers/auth.json` (and `~/.docker/config.json`, below).
- oxa Cloud keys: `~/.config/oxc/llm/cloud-secrets.json`.
- Password stores and keyrings: `~/.password-store`, `~/.local/share/keyrings`, `~/.config/op`, `~/.config/Bitwarden`, `~/.pki`.
- Browser profiles: Firefox, Thunderbird, Chrome, Chromium, Brave, Edge, Vivaldi and Opera profile directories.
- oxa's own state: the agent directory `~/.oxa/agent` (keys, trust records, sessions), `~/.config/oxc` and `~/.local/state/oxc`.
- The earlier agent's directory, `~/.pi/agent`.
- `~/.docker/config.json`.

The list applies to your `$HOME` and also to the home of your account from the passwd database, so starting oxa with another `HOME` never exposes the real `~/.ssh` or the cloud keys.

Exception for the GitHub CLI: in a sandbox with network access (not `--offline`), `~/.config/gh` is bound read-only so `gh` works (its host list and token file are readable there). With `--offline` there is no network and `gh` fails. A token in the system keyring is not reachable from inside the sandbox, so at start-up, when the network is on and `gh` is installed, oxa runs `gh auth token` once outside the sandbox (3 second limit; any failure is ignored). It passes the result to commands that have the network as `GH_TOKEN`, with `GH_HOST=github.com`. This happens in workspace and full modes with network only, never with `--offline` or `oxa.sandbox.net: off`. The token is not logged and is not in the session file, the transcript or the prompt. An exact copy of it in command output is replaced with `[redacted]` before the model sees it. While a command runs, its live output always lags by one byte less than the token's length, whatever the bytes are, so a partial match is never visible in how much output has appeared. Any command that has the network can use the token, for example to call the GitHub API, so log in with a token that has only the scopes you accept. It is handed to bwrap through a pipe, not on its command line, so other processes of your user cannot read it from the process list. Every other entry stays masked.

GitHub reads without a token in the sandbox: the `wait` tool accepts the host-side read probes `pr_view`, `pr_checks`, `run_log_failed`, `issue_view` and `pr_activity` (new reviews and comments on a PR at a pinned head, one `gh pr view` read per poll, bodies scrubbed and cut). They run `gh` outside the sandbox with your login, read only, in fixed argument vectors with a number from the call and the repository from the `origin` remote (github.com only). `gh` must be found in a trusted PATH directory. A failed-step log is cut to its last 24 KB and scrubbed of credentials before the model sees it. They refuse with `--offline` and in a runner-owned worker session. The token never enters the sandbox.

Other secrets are not on the list. A project's `.env` file, a token in a project configuration file, and files outside your home directory are readable by bash, and by the file tools in the modes that allow reads. Keep such files out of the project when the model should not see them, or use `plan` or `--offline` for the session.

### Git worktrees and shared clones

A git worktree keeps its history in the main repository, and a clone made with `git clone --shared` reads its objects from the repository it came from. For such a project, oxa also binds the git data that lies outside the project folder, so `git status`, `git log` and `git commit` work in bash:

- the worktree's own git directory (`.git/worktrees/<name>` in the main repository): read-write;
- the main repository's git directory (its `.git`): read-write, so commits, branches and the index can be written. Its `hooks` and `config` files are read-only, because git runs them outside the sandbox later;
- the object directories that a shared clone reads from: read-only.

In a worktree, a commit only needs the worktree's own git directory and the objects, refs, logs, packed-refs and index parts of the main repository, and those stay writable. Everything git runs or reads outside the sandbox is read-only, as in a normal checkout: the main repository's `hooks`, `config`, `config.worktree` and `info`, and the worktree git directory's `hooks` and `config.worktree`, so a tool call cannot plant a hook or change the configuration.

Because `info` is shared with the main checkout, `.git/info/exclude` cannot be written from bash in a worktree ("Read-only file system"). Bash explains this failure and suggests a local alternative. To hide an existing, untracked scratch directory without changing tracked files or shared metadata, put an ignore-all file **inside** it: `printf '*\n' > .venv-test/.gitignore`. Git then ignores the directory's contents, including that `.gitignore`; do not overwrite an existing tracked `.gitignore`. This also works for directories of check logs. For individual scratch files, write patterns to a writable local ignore file and use `git -c core.excludesFile=<local-ignore-file> status` (or another Git command). That override applies only to that invocation and replaces the usual `core.excludesFile`, not `.gitignore` or `info/exclude`. Editing shared `info/exclude` still requires a terminal outside oxa.

Git's hooks and config stay as they are even when they do not exist yet. In a git directory that bash can write (the project's own `.git`, or the main repository's `.git` of a worktree), a missing `hooks` directory is covered by an empty read-only directory and a missing `config` file by an empty read-only file, so a command cannot create a hook that git would run later. The same holds for `config.worktree` (in the main repository and in the worktree's own git directory) when the repository's `config` turns on `extensions.worktreeConfig`, and for the directory a repository's `core.hooksPath` names (also when it is set in an included config file; a relative value resolves from the worktree root, not from a nested session directory). To do this, the sandbox has to create the empty directory or file in your repository, and it stays there after the command: an empty `.git/hooks` directory (what `git init` makes anyway, minus the samples), an empty `config` / `config.worktree` file, or an empty `core.hooksPath` directory. Git treats an empty one as no hooks and no settings. Symlinks are never bound over: a `hooks` that is a symlink is left as it is. A `.git` that is itself a symlink, or a `.git` file that names a git directory inside what bash can write, is refused: the sandbox cannot pin a directory it does not own, so no command runs there.

Configured hook directories also pin their writable ancestors, so a command cannot move the read-only mount out of Git's path and replace it. The pins keep ordinary files beside hooks writable. Hooks that link to writable content, or have hard-link aliases, refuse the command; a symlink whose target stays read-only is allowed. These checks include custom hook directories outside the project, since their contents can point back into it.

These paths are found once, when oxa starts, by reading files only. Git is not run, and nothing in the repository is executed. A `.git` file or alternates list that does not point back at real git data is not followed. For example, a `.git` file that names `~/.ssh` binds nothing, and git then fails in bash as it does without this feature. The deny-list still applies: a denied path is never bound, and it stays masked inside the sandbox. In plan mode every one of these paths is read-only. The main checkout's files are not visible to bash.

## Project trust

A project can carry configuration that runs code or loads code: settings with hooks, MCP servers, extensions, skills, prompts, and system prompt files. oxa treats these as untrusted until you trust the project.

- In a project that is not yet trusted, oxa asks once. The options are "Trust this project", "Trust parent directory ... and projects below it" (when offered), "Do not trust (remember)" and "Decide later". `/trust` records a decision later.
- `defaultProjectTrust` sets the answer when no decision is recorded: `ask` (default), `always` or `never`. It is a user setting.
- An untrusted project still contributes its context files (`AGENTS.md`, `CLAUDE.md`), which are text. Its other resources do not load.

Trust covers the content, not just the directory. Once you trust a project, oxa records a hash of each file that runs or loads code: the project's `settings.json` keys that do this, `mcp.json`, `SYSTEM.md`, `APPEND_SYSTEM.md`, and everything under `.oxa/extensions`, `.oxa/skills`, `.oxa/prompts`, and `.agents/skills`. The model can write these files, so if one changes:

- in the interface, oxa asks again and lists each changed file;
- in print, JSON and RPC modes, the changed resources are not loaded and a notice names the files.

`--approve` trusts the project and records the current content for one run. `--no-approve` ignores the project's configuration and resources for one run. `oxa install -l` and `oxa mcp ... -l` check the same record.

In `standard` and `auto`, the model cannot write `.oxa/**` or `.agents/skills/**` without asking, and the file tools refuse writes to `.git/hooks` or `.git/config`. Plan mode denies these writes too. User-selected `yolo` outside plan mode deliberately lifts all of these guards.

## Network

The harness connects to the model you chose: the local engine on loopback, or the provider of the selected cloud model. With nothing configured it connects nowhere. It has no telemetry, no version check and no upload of sessions, exports or reports. Its tools may use the network, as the table shows.

| Source | Network | In `ask` mode |
|---|---|---|
| bash in `workspace` | yes, unless `oxa.sandbox.net` is `off` | every command is asked for |
| bash in `plan` | no | read-only commands run; others are refused |
| `web_search` (only with a key) | Brave Search only; refused in `plan` | asked for `api.search.brave.com` |
| remote MCP servers (HTTP) | the server's host | asked |
| extension `fetch` and exec with network | the host asked for | asked |
| exec hooks | no | not applicable |

### Bash network

User-only settings (such as `oxa.sandbox.net` and `oxa.offline`) count only from the user file. A project settings file that sets one is ignored for that key, with a warning, even when the project is trusted.

Bash has network access by default in `workspace` and `ask` modes. `oxa.sandbox.net: "off"` in user settings removes it from bash in every mode. Where there is no OS sandbox to enforce this (off Linux, or an attended Linux TUI without `bwrap` that did not request the OS sandbox), interactive model bash stays available outside plan mode and warns that the network is not restricted; unattended runs disable bash while the setting is off (see [The bash sandbox](#the-bash-sandbox)).

This matters because the model can be steered by text it reads. A steered model can run a command that sends what bash can read to any host. Bash can read the project, the system files, and the parts of your home directory that the deny-list does not mask. In `ask` mode you approve each command, but the approval does not show where the command sends its output. When the project holds secrets, set `oxa.sandbox.net: "off"`, use `ask` mode, or run with `--offline`.

`oxa work --publish` and its opt-in lane acceptance scan the exact commits that a push would add
to its destination and refuse credential-like content before sending anything. The runner, not
the worker, owns GitHub credentials. A lane candidate also cannot change GitHub workflows or the
lane runner; submit those CI-control changes separately for normal review. Every commit the push would add is checked, not only the diff from the saved base. The push itself needs the owner's confirmation through the session broker; outside a session, the terminal or `OXA_WORK_OWNER_CONFIRMED=1` compatibility paths are ignored while a coding-agent marker (`OXA_CODING_AGENT`, `AI_AGENT`, `OXA_SUBAGENT_DEPTH`) is set. A green lane run counts only when its plan and a lane test job actually succeeded, and only for what the lane tests: changes under `engine/` or `fuzz/` stay on the hosted route.

### web_search

`web_search` sends its query text to Brave Search. The model writes the query, and it can copy text from files it read. In `workspace` mode the tool runs without a prompt. In `plan` mode it is denied, because plan mode has no network. In `ask` mode it asks first. To keep the tool out of a session, use `--no-web-search`, `OXA_WEB_SEARCH=0`, or `--offline`.

### Offline mode

`--offline`, `OXA_OFFLINE=1` (`1`, `true` or `yes`) and `"oxa": { "offline": true }` in user settings are one switch. It applies to everything oxa starts. What it does:

- oxa refuses every connection from its own code to a host other than the loopback address, with one error that says offline mode is on. This covers MCP over HTTP and its OAuth sign-in, extension `fetch`, a remote git package install, and `oxa report-issue submit`. An MCP proxy that is not loopback is refused unless the target is on its `NO_PROXY` list (then the connection is direct), and a redirect is not followed (the offline gate checks the first URL only, so the client never uses a redirect target).
- bash runs without network;
- `web_search` is not offered to the model, and a call that gets through is refused;
- extension `exec` with `net: true` runs without network;
- `!cmd` commands and stdio MCP servers are started through `bubblewrap` with the network unshared. If `bubblewrap` is missing or unusable, the command is refused, and it never runs with network;
- subagent children inherit the switch;
- the local engine keeps working.

Offline mode is about the network only. It does not limit what those programs read or write. A `!cmd` command or a stdio MCP server, when started offline, sees your files as you do: it is not under the deny-list masks, and it can write anywhere your account can write. Exec hooks have no network outside yolo. Programs you start yourself, outside oxa, are not affected by oxa at all.

### Remote engine

oxa refuses to send prompts to an engine on another host. The check is on the engine address (`OXA_ENGINE_URL` or `models.json`). Loopback addresses (`localhost`, `127.0.0.0/8`, `::1`) are always allowed. For any other host, set `oxa.engine.allowRemote: true` in the user settings. A project file cannot set it. oxa prints a warning at start-up while it is on.

## Agent control channel

A headless session (`--mode json` or `--mode rpc`) started by `oxa work` or a lead can accept control requests (steer, pause, resume, cancel) besides plain follow-ups. Control requests are accepted only from the same user. The session owner checks each one against the lease it was handed and the goal revision before acting; a stale or foreign request is refused with a receipt. `oxa.a2a.enabled: false` in the user settings turns the lane off: sessions then use the plain file inbox, and `nativeSteering` is refused.

## Extensions, hooks and MCP

- Extensions run in an embedded JavaScript runtime with no file, network or process access of their own. Each host function they call (`exec`, `fs`, `fetch`, session actions) goes through the same policy as the model's tools.
- A trusted live Lead outside Git can start workers in a primary checkout you have explicitly trusted and named in your user settings (`oxa.workers.repository`). The model and the project cannot choose the checkout. The normal write checks cover the source and the worktree, and the child keeps the normal permission ceiling and sandbox policy.
- Exec hooks (`hooks` setting) run as `bash -c` in the sandbox, with no network. Under yolo (including an unattended worker started with yolo) there is no sandbox anywhere: they run with your rights, and `--offline` still keeps their network off. A hook that exits with status 2 blocks the `tool_call` or `user_bash` event it belongs to. A hook that times out or fails does not block anything. They run on every event, without the model asking, so only put commands you have read in a hook.
- MCP stdio servers and `!cmd` commands run with your rights. Outside offline mode they are not in the bash sandbox, and an MCP stdio server starts after an approval. MCP HTTP servers and their OAuth tokens (`mcp-auth.json`, mode `0600`) are subject to the network rules above.
- An MCP tool's read-only mark (`readOnlyHint`) is the server's own claim. oxa does not check it. For a remote server oxa does not believe the mark unless the server entry says `"trustAnnotations": true` (see [mcp.md](mcp.md#permissions)).

## Panic isolation

A panic inside a tool, an extension, an MCP connection, a hook or a codemode script is caught at that boundary: that call fails with an error, and the session continues.

## Sessions and output

Session files are `0600`. The Brave key is never written to a session, an export or a log. What the model read, however, is in the session: a file that contains a secret puts that secret into the session file and into the requests the engine receives. Exports contain the whole session, including all branches. [sessions.md](sessions.md#what-oxa-stores-and-how-to-remove-it) lists what oxa keeps and how to remove it.

`oxa report-issue draft` scrubs its digest and the text you give it, but scrubbing is pattern-based. Read the draft before you post it; `oxa report-issue submit` posts only after you type `yes` at the terminal. In an interactive session the model publishes with the `report_publish` tool. In yolo it posts at once without asking and shows the exact posted text and URL; the scrubber's findings (secrets, emails, home paths) still refuse it. In auto and standard mode oxa shows the exact title and body (or the exact comment and its destination) and only you approve it: a key press in the approval dialog (Deny is focused) or a `yes` you type yourself. A `yes` that a tool result, file, web page or another session contains never counts; only a line you typed into oxa's own input does. Print, JSON and RPC runs keep the terminal confirmation. `/bug` writes a redacted local bundle and uploads nothing.

### Publishing

`oxa work --publish` and lane pushes scan the commits they would push. The scan refuses:

- private keys and known token formats;
- opaque credential assignments and quoted literals, including names ending in `_key` or
  `credentials`;
- a changed JSON value whose secret key (for example, `"password":`) is on an unchanged line;
- `Bearer` or `Basic` authorization headers and URL passwords; and
- content added to a secret-store file such as `.env`, or the value of a secret-named variable or
  setting.

Files are judged by their content, not their name: an empty secret-store file and `.env.example`
can be published when they contain no secret. A credential deleted in a later commit still blocks
publication, so rewrite the unpublished commits. The detailed rules and examples are in
[../work.md](../work.md#what-the-publication-scan-refuses).

## Checklist

- Keep the mode at `workspace` for daily work and use `plan` for review. Use `ask` on an unfamiliar project.
- Trust only projects you have read. Use `defaultProjectTrust: "ask"`.
- Start oxa in a project directory, not in your home directory.
- Put the Brave key in a `0600` file, or in the environment for one shell.
- Use `--offline` (or `OXA_OFFLINE=1`) when the task needs no network.
- Set `oxa.sandbox.net: "off"` if bash should never reach the network.
- Do not add `oxa.engine.allowRemote` unless the engine is on a host you control.

## Sandbox persistence

oxa shows no startup notice about this (it would fill small terminals). The note is appended to bash results that mention /tmp or fail with a read-only or permission error; this contextual reminder is the only place the model sees it.

Workspace writes persist (except protected paths), as do configured writable binds, package caches and permitted git data. Other paths are read-only or hidden. /tmp is the session's private scratch: it persists between commands of one session and is deleted when the session ends (`TMPDIR` and `XDG_CACHE_HOME` point into it). Do not report scratch files as saved; put deliverables in the workspace.

Each session gets its own scratch directory, `~/.cache/oxa/scratch/<pid>-<n>` on the host (mode 0700), created when the session starts or is reopened. It is deleted when the session ends: at `/new`, `/resume`, fork, clone or any other switch to another session, and when oxa exits. A resumed session therefore starts with an empty /tmp; two sessions never share one. A directory left behind by an oxa that was killed is removed when a later oxa creates its next session directory. Hook commands do not belong to a session and get a new empty /tmp for each command. If the directory cannot be created, every command gets a new empty /tmp instead.

Inside the sandbox `XDG_CACHE_HOME` is always `/tmp/.cache`. Shared caches stay shared: each entry of your cache home (your `XDG_CACHE_HOME` when it is an absolute path, otherwise `~/.cache`) that is bound read-write, for example through `writePaths`, appears as `/tmp/.cache/<name>`, so tools write to the same cache as your own builds. Everything else a tool caches goes to the session's scratch and is deleted with it. With a custom `XDG_CACHE_HOME`, entries of `~/.cache` are not linked.
