# oxa documentation

`oxa` is the terminal coding agent and the front door of the ox suite. It runs cloud models through its own gateway and local models through the `oxe` engine, which it starts itself. Its tools may use the network, and bash has network access by default; [security.md](pages/security.md) says what each part can reach. oxa runs bash in a sandbox on Linux, keeps sessions as trees, and can be extended with JavaScript, skills, prompt templates and MCP servers. These pages are for people who use it from a terminal.

## Quickstart

Install a release or a dev build with [install.md](pages/install.md), then choose a model with [quickstart.md](pages/quickstart.md): a provider key for a cloud model, or `oxa local add` for a GGUF file on your disk. When working from source, build from the `oxa/` directory of the checkout:

```console
$ cargo build --release --locked -p oxa-cli
$ export PATH="$PWD/target/release:$PATH"
$ cd ~/project
$ oxa
```

Type a request in the editor and press Enter. Escape stops a turn, Ctrl+C clears the editor, and Ctrl+D exits when it is empty. For a single answer in a script, use print mode:

```console
$ oxa -p --tools read,ls "List the files"
```

The default permission mode is `auto`: oxa does not stop to ask, bash runs in the sandbox, and the file tools still refuse credentials and other protected paths. `--standard` asks before steps that leave the project, `--yolo` is full access without the sandbox, and `--permission-mode plan` is read-only. [modes.md](pages/modes.md) explains each one. `--offline` refuses oxa's own network connections and takes the network from bash; the programs you start with `!cmd` or as stdio MCP servers run without network, but they are not otherwise sandboxed. See [security.md](pages/security.md#offline-mode).

User files live in `~/.oxa/agent/`. Project files live in `.oxa/` and load only in a project you trust.

## Pages

| Page | Read it for |
|---|---|
| [install.md](pages/install.md) | the stable release, the dev build, building from source, updating and removing |
| [quickstart.md](pages/quickstart.md) | cloud providers and keys, local models, the first prompt, where files live |
| [modes.md](pages/modes.md) | standard, auto, yolo and plan: what each mode does, and macOS |
| [usage.md](pages/usage.md) | the interface: editor, keys, slash commands, footer, selectors |
| [cli.md](pages/cli.md) | every flag, subcommand and environment variable |
| [work.md](work.md) | `oxa work`: unattended tasks in git worktrees, workers and the work board |
| [lead.md](lead.md) | `oxa lead`: the lead session that watches the board and the workers |
| [lead-web.md](pages/lead-web.md) | default-off lead web access scaffold and future Tailscale setup |
| [panel.md](pages/panel.md) | the ox AI panel: the JSON that `oxa` prints for it |
| [settings.md](pages/settings.md) | every settings key, its type and default, and how files are merged |
| [tools-context.md](pages/tools-context.md) | the tool formats and the context defaults, and the settings that change them |
| [sessions.md](pages/sessions.md) | session storage, resuming, forking, the tree, labels, export, what is stored and how to remove it |
| [compaction.md](pages/compaction.md) | context limits, compaction, the kept tail, cache misses |
| [tools.md](pages/tools.md) | the built-in tools, `subagent`, the repair turn, `web_search` |
| [security.md](pages/security.md) | permission modes, the bash sandbox, project trust, network and offline mode |
| [skills.md](pages/skills.md) | skills, prompt templates, context files, the identity, themes |
| [extensions.md](pages/extensions.md) | JavaScript extensions, exec hooks, packages |
| [mcp.md](pages/mcp.md) | MCP servers: configuration, OAuth, exposure |
| [rpc.md](pages/rpc.md) | print and JSON output, the RPC protocol, the Rust client |
| [differences.md](differences.md) | derived from pi: attribution and licence |
| [engine-support.md](pages/engine-support.md) | the engine features oxa uses, and their support status |

The security summary for the repository is in [SECURITY.md](../SECURITY.md). The project overview and the crate layout are in [README.md](../README.md).

## Conventions

- Commands in `sh` blocks run as written. Commands in `console` blocks show a prompt (`$`) and may need a key, an engine or a project.
- `~` is the home directory. `<project>` is the project root, the directory you start oxa in.
- Paths in the `.oxa/` tree are relative to a project, and paths in `~/.oxa/agent/` are relative to the agent directory.
- Setting names are written as they appear in JSON. Nested keys are shown with dots, for example `compaction.reserveTokens`.
- A prefix epoch is the part of a session that the engine caches: the system prompt and the tool list. A new prefix epoch starts with a new session, a resumed session, a model switch, or a `/reload` that changes one of them. Until the next epoch, the system prompt and the tool list stay byte-identical, so the engine can reuse its cache.
