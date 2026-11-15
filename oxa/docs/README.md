# oxa documentation

`oxa` is the terminal coding agent of ox-llm. Its model talks only to the local engine. Its tools may use the network, and bash has network access by default; [security.md](pages/security.md) says what each part can reach. oxa runs bash in a sandbox, keeps sessions as trees, and can be extended with JavaScript, skills, prompt templates and MCP servers. These pages are for people who use it from a terminal.

## Quickstart

Build the binary from the repository root and point it at a running engine:

```console
$ cargo build --release -p oxa-cli
$ export OXA_ENGINE_URL=http://127.0.0.1:8092
$ cd ~/src/my-project
$ ./target/release/oxa
```

Type a request in the editor and press Enter. Escape stops a turn, Ctrl+C clears the editor, and Ctrl+D exits when it is empty. For a single answer in a script, use print mode:

```console
$ oxa -p --tools read,ls "List the files"
```

The default permission mode is `workspace`: reads and writes inside the project run without a prompt, bash runs in the sandbox, and writes outside the project are asked for. Set `--permission-mode plan` for read-only work. `--offline` refuses oxa's own network connections and takes the network from bash; the programs you start with `!cmd` or as stdio MCP servers run without network, but they are not otherwise sandboxed. See [security.md](pages/security.md#offline-mode).

User files live in `~/.oxa/agent/`. Project files live in `.oxa/` and load only in a project you trust. Start with [quickstart.md](pages/quickstart.md) for the full set-up.

## Pages

| Page | Read it for |
|---|---|
| [quickstart.md](pages/quickstart.md) | building, connecting to the engine, the first prompt, where files live |
| [usage.md](pages/usage.md) | the interface: editor, keys, slash commands, footer, selectors |
| [cli.md](pages/cli.md) | every flag, subcommand and environment variable |
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
| [differences.md](differences.md) | what differs from the earlier agent, and what is not included |
| [engine-support.md](pages/engine-support.md) | the engine features oxa uses, and their support status |

The security summary for the repository is in [SECURITY.md](../SECURITY.md). The project overview and the crate layout are in [README.md](../README.md).

## Conventions

- Commands in `sh` blocks run as written. Commands in `console` blocks show a prompt (`$`) and may need a key, an engine or a project.
- `~` is the home directory. `<project>` is the project root, the directory you start oxa in.
- Paths in the `.oxa/` tree are relative to a project, and paths in `~/.oxa/agent/` are relative to the agent directory.
- Setting names are written as they appear in JSON. Nested keys are shown with dots, for example `compaction.reserveTokens`.
- A prefix epoch is the part of a session that the engine caches: the system prompt and the tool list. A new prefix epoch starts with a new session, a resumed session, a model switch, or a `/reload` that changes one of them. Until the next epoch, the system prompt and the tool list stay byte-identical, so the engine can reuse its cache.
