# Modes: standard, auto, yolo and plan

The permission mode decides how much the agent may do without asking you. This page says what each mode does and what it does not do. The rules behind the modes (the permission table, the sandbox, the deny-list, project trust) are in [security.md](security.md).

## The four modes

| Mode | Asks you | Bash | File tools |
|---|---|---|---|
| `standard` | before a write outside the project, a read outside it, and other steps that reach beyond the project | in the sandbox | refuse credentials and protected paths |
| `auto` (default) | no; one exception: a write to the project's `.oxa` configuration | in the sandbox | refuse credentials and protected paths |
| `yolo` | never | **no sandbox**: runs as you | refuse nothing |
| `plan` | as `standard` | read-only commands only | no writes |

The status bar always shows the mode. In print, JSON and RPC mode it is written to stderr at start.

## standard

Work inside the project runs without a question: reading, editing, and bash in the sandbox. oxa asks before it reads or writes outside the project and before an extension command that wants the network. It offers "Allow once", "Allow for this session" and "Deny". A session grant is kept in memory and is gone when the session ends.

Choose it when you want to see each step that leaves the project.

## auto

`auto` is the default of a fresh install. It removes the questions and nothing else:

- bash still runs in the sandbox, with its deny-list;
- the file tools still refuse `~/.ssh`, `~/.gnupg`, cloud and git credentials, `~/.config/oxc`, browser profiles and the agent directory;
- `.git/hooks` and `.git/config` are never written;
- a write to the project's `.oxa` configuration still asks.

The questions that `auto` answers for you are counted, and the status bar shows the count.

Bash has network access by default, in `auto` as in `standard`. A command can send what it can read to a host. Set `oxa.sandbox.net` to `off`, or start with `--offline`, to take the network away.

## yolo

`yolo` is full power. It removes every guard that oxa has:

- no questions;
- no bash sandbox: a command runs as you, in your own environment, with your files, your credentials and your network;
- no deny-list and no protected paths: the file tools read and write whatever you can.

A yolo session can read your SSH keys and your provider keys, push to any remote you can push to, publish what it finds, and delete anything you can delete. oxa does not inspect what a yolo command does, and it does not stop one. The model can also be steered by text it reads: a file in the project, a web page, the output of an MCP server.

Two things still hold in yolo. Plan mode stays read-only. With `--offline`, bash still has no network.

Use yolo only in a session you watch, on a machine and in a project you can restore. When you want no questions but still want the sandbox, use `auto`.

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

On Linux, oxa never runs bash unsandboxed outside yolo. Without a usable `bwrap` the bash tool is turned off, and oxa says so at start.

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
