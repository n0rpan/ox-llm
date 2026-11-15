# Security

oxa is a local agent. The model reads your project, runs commands and writes files on your machine, and it can be steered by any text it reads, including files in the project, web pages and the output of MCP servers. The controls below limit what a model can do and reach. They do not make a model safe to run on untrusted input without supervision, and they do not limit what you do yourself.

The short threat model is in [SECURITY.md](../../SECURITY.md) at the repository root.

## Permission modes

Every tool call has an access class: `Read`, `Write`, `Exec` (a command), `Net` (a host) or `Mcp` (an MCP tool, read or write). The mode decides what happens to each class.

| Access | `plan` | `workspace` (default) | `ask` |
|---|---|---|---|
| read inside the project | allow | allow | allow |
| read outside the project | ask | ask | ask |
| write inside the project | deny | allow | ask |
| write outside the project | deny | ask | ask |
| bash, read-only command, no network | allow | allow | ask |
| bash, other command, no network | deny | allow (sandboxed) | ask |
| extension `exec` with `net: true` | deny | ask | ask |
| `web_search` | deny | allow, no prompt | ask |
| MCP tool that its server marks read-only | allow | allow | ask |
| MCP tool, other | deny | ask | ask |

In `workspace`, bash runs without asking, in the sandbox described below. Bash has network access in `workspace` and `ask` modes by default (see [Network](#network)). An extension `exec` that asks for network is asked for in `workspace` and `ask`, and refused in `plan`.

- When oxa asks, it offers "Allow once", "Allow for this session", and "Deny". A session grant covers the same path directory, command, host or tool for the rest of the run. Grants are kept in memory and never written to disk. A grant never overrides a deny.
- Plan mode is for reading and planning. It refuses writes, the `subagent` tool, `web_search`, and every command that is not read-only.

Paths are resolved before the check: `..` is followed and a symbolic link that points outside the project counts as outside.

### standard, auto and yolo

On top of the table, the permission mode decides who answers the questions:

| Mode | Prompts | Bash | File tools |
|---|---|---|---|
| `standard` | the table above asks you | in the sandbox | deny-list and protected paths apply |
| `auto` (default) | none: what would be asked is allowed and counted | in the sandbox | deny-list and protected paths apply |
| `yolo` | none | **without the sandbox**, in your own environment | **not** limited by the deny-list or the protected paths |
| `plan` | as in the table | read-only commands | no writes |

- `auto` is the default of a fresh install. It removes the prompts and nothing else: the bash sandbox and its deny-list stay, the file tools still refuse `~/.ssh`, `~/.gnupg`, cloud and git credentials, `~/.config/oxc`, browser profiles and the agent directory, `.git/hooks` and `.git/config` are never written, and a write to the project's `.oxa` configuration still asks.
- `yolo` is full access. Bash runs as you, with your files, credentials and network; the file tools can read and write anything your account can. Use it only in a session you watch, on a machine and project you can restore. With `--offline` bash still has no network.
- Set the mode at start with `--standard`, `--auto`, `--yolo` or `--permission-mode <mode>` (one run), or with the `permissions.defaultMode` setting (`standard`, `auto`, `yolo` or `plan`). `/mode <mode> --save` and "make default" in the `/mode` picker write that setting. A project's settings can lower the mode (for example to `standard`) but never raise it.
- In a session: `/mode` opens the picker, `/mode <mode>` switches, `/auto` switches to auto, `/yolo [on|off]` or Ctrl+Alt+O toggles yolo (off returns to the mode before it), and Alt+M cycles standard, auto and yolo. `/mode workspace` and `/mode ask` still set the table mode.
- The status bar always shows the mode. In yolo it shows a flame, `YOLO full access` and the count of auto-approved actions, the frames turn to the theme's hot colours, and the first switch in a session prints a one-line notice. In print, JSON and RPC mode the mode is written to stderr at start.
- Subagents inherit the mode the parent has when they start.
- Plan mode and project trust work the same in every mode.

A shell alias makes yolo quick to start:

```console
$ alias yoxa='oxa --yolo'
```

## The bash sandbox

On Linux, bash runs in a `bubblewrap` (`bwrap`) sandbox. Without a usable `bwrap`, the bash tool is disabled and oxa says so at start-up. oxa does not fall back to running commands unsandboxed; only the `yolo` mode runs bash without it.

What a command sees:

| Part of the machine | Inside the sandbox |
|---|---|
| `/` (system files, toolchains, and any other path your account can read) | read-only |
| your home directory | empty, except the entries below. The exception is a session started in the home directory or above it, see [Starting in your home directory](#starting-in-your-home-directory) |
| `/tmp` | a new empty directory for each command |
| the project directory | read-write (read-only in plan mode) |
| your toolchains: `PATH` entries under home, rustup, mise and asdf, nvm and similar, uv, pipx, pyenv and rbenv, git configuration | read-only |
| package caches of pip, uv, go, cargo registry, npm, pnpm, yarn, bun, deno | read-write |
| `oxa.sandbox.readPaths`, `oxa.sandbox.writePaths` (user settings, `~/` expanded) | read-only, read-write |
| `.oxa`, `.agents/skills`, `.git/hooks`, `.git/config` inside a writable project | read-only |
| a project's `.git` file (a worktree), `.git/config.worktree`, and a missing `.git/hooks` or `.git/config` | read-only; a missing one cannot be created (see below) |
| the deny-list below | masked: an empty read-only tmpfs over a directory, `/dev/null` over a file |
| environment variables | none, except the short list below |
| file ownership | only your own uid is mapped; root-owned system files (for example `/usr/bin/python3`) show as uid 65534 (`nobody`), and `id` shows your uid plus group `nobody` |

Tools that insist on "owned by root or the caller" (stat/ownership checks, some wrappers, sudo-like or ssh strict-mode checks) fail only inside the sandbox because of this uid mapping. That is an artifact of the user namespace, not a repo bug, and no flag maps host root without extra privilege; report such failures as environmental.

The environment of a command starts empty. It gets `PATH`, `HOME`, `LANG`, `TERM` and `AI_AGENT`, the toolchain variables that are set (for example `CARGO_HOME`, `RUSTUP_HOME`, `GOPATH`, `NVM_DIR`, `UV_CACHE_DIR`, and the `MISE_*` variables), and the variables oxa sets for the session (`OXA_SESSION_ID`, and `OXA_SESSION_FILE` unless the session is ephemeral). Every other variable is dropped, including your API keys and tokens. A toolchain variable whose name contains `TOKEN`, `SECRET`, `KEY`, `PASSWORD`, `PASSWD`, `CREDENTIAL`, `AUTH`, `COOKIE` or `SESSION` is never passed.

### Starting in your home directory

If oxa starts in your home directory, in `/`, or in a parent of your home directory (for example `/home`), the project directory is not a project. The whole directory is then the bash working directory, and:

- a start in `/` or a parent of your home directory is read-only for bash, whatever the settings;
- a start in your home directory itself is read-only for bash, unless you confirm the interactive question below or the user setting `oxa.sandbox.allowHomeWrite: true` is set. Then your home directory is the workspace: bash can write in its folders and files (for example `~/project`), while the home folder itself (so nothing new appears next to your dotfiles), every dotfile and dot-folder (`.bashrc`, `.profile`, `.zshrc`, `.config`, `.local/bin`, `.ssh`, `.oxa`, `.claude`, `.codex`, ...), symlinks and `~/bin` stay read-only, and the deny-list stays masked. One notice at the start says so ("workspace = your home directory"). Create a new top-level folder with the file tools or outside oxa. The file tools ask before writing the shell startup files, `~/.local/bin`, `~/.config/{systemd,autostart,hypr,omarchy,...}`, `~/bin` and the agents' settings, also in `auto` mode (not in `yolo`). In print, JSON and RPC mode only the setting makes the home writable;
- your home directory is visible to bash, read-only, with the deny-list below still masked. A command can read your shell history, your dot files and your other files there;
- bash has network access by default, so a command can send what it reads to a host (see [Network](#network)).

oxa refuses such a start in print, JSON and RPC modes. In interactive mode it warns and asks "Start oxa here anyway?", and the default answer is No. The user setting `oxa.allowHomeCwd: true`, or the environment variable `OXA_ALLOW_HOME_CWD=1`, allows the start without the question. Start oxa in a project directory instead.

Writable package caches are shared with your own builds. A crate or Go module that the model unpacks there is what your next build reads. Clear a cache after a session you do not trust.

### The deny-list

These paths are masked inside the sandbox, and the file tools refuse them in every mode, whatever the grants or settings say. The list is fixed. Each entry is a path under your home directory, except oxa's own agent directory, which is masked wherever `OXA_AGENT_DIR` puts it.

- SSH, GPG, and cloud credentials: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.kube`, `~/.vault-token`, `~/.terraform.d/credentials.tfrc.json`.
- Git and package-registry credentials: `~/.git-credentials`, `~/.config/git/credentials`, `~/.config/gh` (see below), `~/.config/hub`, `~/.netrc`, `~/.npmrc`, `~/.pypirc`, `~/.cargo/credentials`, `~/.cargo/credentials.toml`.
- oxa Cloud keys: `~/.config/oxc/llm/cloud-secrets.json`.
- Password stores and keyrings: `~/.password-store`, `~/.local/share/keyrings`, `~/.config/op`, `~/.config/Bitwarden`, `~/.pki`.
- Browser profiles: Firefox, Thunderbird, Chrome, Chromium, Brave, Edge, Vivaldi and Opera profile directories.
- oxa's own state: the agent directory `~/.oxa/agent` (keys, trust records, sessions), `~/.config/oxc` and `~/.local/state/oxc`.
- The earlier agent's directory, `~/.pi/agent`.
- `~/.docker/config.json`.

The list applies to your `$HOME` and also to the home of your account from the passwd database, so starting oxa with another `HOME` never exposes the real `~/.ssh` or the cloud keys.

Exception for the GitHub CLI: in a sandbox with network access (not `--offline`), `~/.config/gh` is bound read-only so `gh` works (its host list and token file are readable there). With `--offline` there is no network and `gh` fails. A token in the system keyring is not reachable from inside the sandbox, so at start-up, when the network is on and `gh` is installed, oxa runs `gh auth token` once outside the sandbox (3 second limit; any failure is ignored). It passes the result to commands that have the network as `GH_TOKEN`, with `GH_HOST=github.com`. This happens in workspace and full modes with network only, never with `--offline` or `oxa.sandbox.net: off`. The token is not logged and is not in the session file, the transcript or the prompt. An exact copy of it in command output is replaced with `[redacted]` before the model sees it. Any command that has the network can use the token, for example to call the GitHub API, so log in with a token that has only the scopes you accept. It is handed to bwrap through a pipe, not on its command line, so other processes of your user cannot read it from the process list. Every other entry stays masked.

Other secrets are not on the list. A project's `.env` file, a token in a project configuration file, and files outside your home directory are readable by bash, and by the file tools in the modes that allow reads. Keep such files out of the project when the model should not see them, or use `plan` or `--offline` for the session.

### Git worktrees and shared clones

A git worktree keeps its history in the main repository, and a clone made with `git clone --shared` reads its objects from the repository it came from. For such a project, oxa also binds the git data that lies outside the project folder, so `git status`, `git log` and `git commit` work in bash:

- the worktree's own git directory (`.git/worktrees/<name>` in the main repository): read-write;
- the main repository's git directory (its `.git`): read-write, so commits, branches and the index can be written. Its `hooks` and `config` files are read-only, because git runs them outside the sandbox later;
- the object directories that a shared clone reads from: read-only.

In a worktree, a commit only needs the worktree's own git directory and the objects, refs, logs, packed-refs and index parts of the main repository, and those stay writable. Everything git runs or reads outside the sandbox is read-only, as in a normal checkout: the main repository's `hooks`, `config`, `config.worktree` and `info`, and the worktree git directory's `hooks` and `config.worktree`, so a tool call cannot plant a hook or change the configuration.

Git's hooks and config stay as they are even when they do not exist yet. In a git directory that bash can write (the project's own `.git`, or the main repository's `.git` of a worktree), a missing `hooks` directory is covered by an empty read-only directory and a missing `config` file by an empty read-only file, so a command cannot create a hook that git would run later. The same holds for `config.worktree` (in the main repository and in the worktree's own git directory) when the repository's `config` turns on `extensions.worktreeConfig`. To do this, the sandbox has to create the empty directory or file in your repository, and it stays there after the command: an empty `.git/hooks` directory (what `git init` makes anyway, minus the samples), or an empty `config` / `config.worktree` file. Git treats an empty one as no hooks and no settings. Symlinks are never bound over: a `hooks` that is a symlink is left as it is.

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

The model cannot write `.oxa/**` or `.agents/skills/**` without asking in `workspace` and `ask` modes, and plan mode denies it. The file tools never write `.git/hooks` or `.git/config`, in any mode.

## Network

The harness connects to one place by default: the engine, on loopback (`127.0.0.1:8092`). It has no telemetry, no version check and no upload of sessions, exports or reports. The model talks only to the engine. Its tools may use the network, as the table shows.

| Source | Network | In `ask` mode |
|---|---|---|
| bash in `workspace` | yes, unless `oxa.sandbox.net` is `off` | every command is asked for |
| bash in `plan` | no | read-only commands run; others are refused |
| `web_search` (only with a key) | Brave Search only; refused in `plan` | asked for `api.search.brave.com` |
| remote MCP servers (HTTP) | the server's host | asked |
| extension `fetch` and exec with network | the host asked for | asked |
| exec hooks | no | not applicable |

### Bash network

Bash has network access by default in `workspace` and `ask` modes. `oxa.sandbox.net: "off"` in user settings removes it from bash in every mode.

This matters because the model can be steered by text it reads. A steered model can run a command that sends what bash can read to any host. Bash can read the project, the system files, and the parts of your home directory that the deny-list does not mask. In `ask` mode you approve each command, but the approval does not show where the command sends its output. When the project holds secrets, set `oxa.sandbox.net: "off"`, use `ask` mode, or run with `--offline`.

### web_search

`web_search` sends its query text to Brave Search. The model writes the query, and it can copy text from files it read. In `workspace` mode the tool runs without a prompt. In `plan` mode it is denied, because plan mode has no network. In `ask` mode it asks first. To keep the tool out of a session, use `--no-web-search`, `OXA_WEB_SEARCH=0`, or `--offline`.

### Offline mode

`--offline`, `OXA_OFFLINE=1` (`1`, `true` or `yes`) and `"oxa": { "offline": true }` in user settings are one switch. It applies to everything oxa starts. What it does:

- oxa refuses every connection from its own code to a host other than the loopback address, with one error that says offline mode is on. This covers MCP over HTTP and its OAuth sign-in, extension `fetch`, a remote git package install, and `oxa report-issue submit`.
- bash runs without network;
- `web_search` is not offered to the model, and a call that gets through is refused;
- extension `exec` with `net: true` runs without network;
- `!cmd` commands and stdio MCP servers are started through `bubblewrap` with the network unshared. If `bubblewrap` is missing or unusable, the command is refused, and it never runs with network;
- subagent children inherit the switch;
- the local engine keeps working.

Offline mode is about the network only. It does not limit what those programs read or write. A `!cmd` command or a stdio MCP server, when started offline, sees your files as you do: it is not under the deny-list masks, and it can write anywhere your account can write. Exec hooks have never had network. Programs you start yourself, outside oxa, are not affected by oxa at all.

### Remote engine

oxa refuses to send prompts to an engine on another host. The check is on the engine address (`OXA_ENGINE_URL` or `models.json`). Loopback addresses (`localhost`, `127.0.0.0/8`, `::1`) are always allowed. For any other host, set `oxa.engine.allowRemote: true` in the user settings. A project file cannot set it. oxa prints a warning at start-up while it is on.

## Extensions, hooks and MCP

- Extensions run in an embedded JavaScript runtime with no file, network or process access of their own. Each host function they call (`exec`, `fs`, `fetch`, session actions) goes through the same policy as the model's tools.
- Exec hooks (`hooks` setting) run as `bash -c` in the sandbox, with no network. A hook that exits with status 2 blocks the `tool_call` or `user_bash` event it belongs to. A hook that times out or fails does not block anything. They run on every event, without the model asking, so only put commands you have read in a hook.
- MCP stdio servers and `!cmd` commands run with your rights. Outside offline mode they are not in the bash sandbox, and an MCP stdio server starts after an approval. MCP HTTP servers and their OAuth tokens (`mcp-auth.json`, mode `0600`) are subject to the network rules above.
- An MCP tool's read-only mark (`readOnlyHint`) is the server's own claim. oxa does not check it. For a remote server oxa does not believe the mark unless the server entry says `"trustAnnotations": true` (see [mcp.md](mcp.md#permissions)).

## Panic isolation

A panic inside a tool, an extension, an MCP connection, a hook or a codemode script is caught at that boundary: that call fails with an error, and the session continues.

## Sessions and output

Session files are `0600`. The Brave key is never written to a session, an export or a log. What the model read, however, is in the session: a file that contains a secret puts that secret into the session file and into the requests the engine receives. Exports contain the whole session, including all branches. [sessions.md](sessions.md#what-oxa-stores-and-how-to-remove-it) lists what oxa keeps and how to remove it.

`oxa report-issue draft` scrubs its digest and the text you give it, but scrubbing is pattern-based. Read the draft before you post it; `oxa report-issue submit` posts only after you type `yes` at the terminal. `/bug` writes a redacted local bundle and uploads nothing.

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

Workspace writes persist (except protected paths), as do configured writable binds, package caches and permitted git data. Other paths are read-only or hidden. /tmp is private scratch: writes there are discarded after each command, not saved on the host. Do not report scratch files as saved; put deliverables in the workspace.
