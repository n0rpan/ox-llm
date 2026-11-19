# Modes: standard, auto, yolo and plan

The permission mode decides how much the agent may do without asking you. This page says what each mode does and what it does not do. The rules behind the modes (the permission table, the sandbox, the deny-list, project trust) are in [security.md](security.md).

Execution context matters independently of the permission mode. Interactive CLI sessions do not use an OS sandbox by default; dangerous commands ask for your approval. Unattended runs without yolo keep the Bubblewrap sandbox by default. Yolo never uses an OS sandbox, including for unattended work it starts. In every context, the in-process guards remain: secret masking, private-path denials, user-only settings, and offline mode.

## The four modes

| Mode | Asks you | Bash | File tools |
|---|---|---|---|
| `standard` | before a write outside the project, a read outside it, and other steps that reach beyond the project | no OS sandbox interactively; sandboxed when unattended | refuse credentials and protected paths |
| `auto` (default) | no routine questions; protected configuration and dangerous commands still ask | no OS sandbox interactively; sandboxed when unattended | refuse credentials and protected paths |
| `yolo` | never for routine or dangerous commands | **no sandbox**: runs as you | in-process security guards remain |
| `plan` | as `standard` | read-only commands only | no writes |

The status bar always shows the mode. In print, JSON and RPC mode it is written to stderr at start.

## standard

Work inside the project runs without a routine question: reading, editing, and bash. In an interactive CLI session, bash runs with your user rights and dangerous commands require approval. In an unattended standard run, bash uses the OS sandbox. oxa also asks before it reads or writes outside the project and before an extension command that wants the network. It offers "Allow once", "Allow for this session", "Always allow" and "Deny". A session grant is kept in memory and is gone when the session ends.

Choose it when you want to see each step that leaves the project.

## auto

`auto` is the default of a fresh install. It removes routine permission questions, but does not bypass dangerous-command approval in an interactive CLI session:

- interactive bash runs with your user rights; unattended bash keeps the sandbox unless the run is yolo;
- the file tools still refuse `~/.ssh`, `~/.gnupg`, cloud and git credentials, `~/.config/oxc`, browser profiles and the agent directory;
- `.git/hooks` and `.git/config` are never written;
- a write to the project's `.oxa` configuration still asks.

The questions that `auto` answers for you are counted, and the status bar shows the count.

Bash has network access by default, in `auto` as in `standard`. A command can send what it can read to a host. Set `oxa.sandbox.net` to `off`, or start with `--offline`, to take the network away.

## yolo

`yolo` is full power for command execution. It removes the permission prompts and OS sandbox:

- no questions;
- no bash sandbox: a command runs as you, in your own environment, with your files, your credentials and your network;
- the in-process guards listed above remain, including secret masking and private-path denials.

A yolo command runs with your user rights and can reach files and credentials available to that process, push to any remote you can push to, and delete anything you can delete. Oxa still masks secrets and hard-refuses publishing them or an OS-breaking wipe, but it does not ask before other commands. The model can also be steered by text it reads: a file in the project, a web page, or the output of an MCP server.

The in-process guards still hold in yolo. Plan mode stays read-only, and with `--offline`, bash still has no network.

Use yolo only on a machine and in a project you can restore. When an unattended run needs OS isolation, use a non-yolo mode.

The status bar shows a flame and `YOLO full access`, and the first switch to yolo in a session prints a one-line notice.

## plan

`plan` is for reading and planning. It refuses writes, the `subagent` tool, `web_search` and every command that is not read-only. Yolo does not lift it.

## macOS

On macOS there is no OS sandbox, in any mode. This is the honest picture:

- Bash and exec hooks run as `bash -c` with your full user rights. Every result is marked as unsandboxed, and oxa says so at start and tells the model.
- The guards that live inside oxa stay on in `standard` and `auto`: the file tools still refuse credentials and protected paths, and permissions and project trust work as on Linux.
- Bash starts from the same cleared environment as in the Linux sandbox, so the credential variables of the oxa process are not passed on.
- oxa cannot take the network from bash there. When `--offline` or `oxa.sandbox.net: "off"` asks for that, bash is turned off for the run instead.

So on macOS a bash command can read any file you can read, in `standard` and `auto` too. The file tools protect your credentials; bash does not. To approve each command yourself, start with `--permission-mode ask`.

On Linux, unattended non-yolo runs require a usable `bwrap`; without it, the bash tool is turned off and oxa says so at start. Interactive non-yolo sessions deliberately run without that OS sandbox and use dangerous-command approvals instead.

## Choosing and switching

At start, for one run:

```console
$ oxa --standard
$ oxa --auto
$ oxa --yolo
$ oxa --permission-mode plan
```

As your default, in the user settings:

```json
{ "permissions": { "defaultMode": "standard" } }
```

In a session:

| Action | How |
|---|---|
| open the mode picker | `/mode` |
| switch | `/mode standard`, `/mode auto`, `/mode yolo`, `/mode plan` |
| switch and keep as default | `/mode <mode> --save` |
| cycle standard, auto, yolo | Alt+M |
| toggle yolo | `/yolo`, or Ctrl+Alt+O; turning it off returns to the mode before it |

A project's settings can lower the mode, for example to `standard`. They can never raise it.

## Workers, subagents and background jobs

- A subagent gets the mode its parent has when it starts.
- A background job keeps the rights it started with. Switching the mode changes the commands that come after, not a process that is already running.
- An `oxa work` worker runs in the sandbox by default. It runs in yolo only when your own user settings say `permissions.defaultMode` is `yolo`. A project cannot raise it. See [work.md](../work.md).

## What no mode changes

- **Project trust.** A project's own code (extensions, hooks, MCP servers) loads only in a project you trust, in every mode.
- **Offline.** `--offline` refuses oxa's own network connections in every mode.
- **User-only settings.** A project file cannot choose the engine, the provider keys, the budget or the sandbox settings, in any mode.
