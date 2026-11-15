# Quickstart

This page gets `oxa` running against a local engine and sends a first prompt. For every flag see [cli.md](cli.md); for the terminal UI see [usage.md](usage.md).

## Requirements

- A Rust toolchain, 1.87 or newer (the `rust-version` of the workspace in `Cargo.toml`).
- A running engine that speaks the OpenAI-compatible HTTP API. In an ox-llm install this is the controller (`oxc`) on `http://127.0.0.1:8092`.
- For the bash tool: Linux with `bubblewrap` (`bwrap`) installed and unprivileged user namespaces enabled. Without a usable `bwrap` the bash tool is disabled and oxa says so at startup.

## Build

```console
$ cargo build --release -p oxa-cli
$ ./target/release/oxa --version
```

The binary is `target/release/oxa`. Copy it somewhere on your `PATH`, or run it by its path.

## Point oxa at the engine

oxa reads the engine address from `OXA_ENGINE_URL`. When the variable is not set and no `models.json` names an endpoint, oxa uses the default `http://127.0.0.1:8092`.

```console
$ export OXA_ENGINE_URL=http://127.0.0.1:8092
```

If the engine is not reachable, oxa stops with one line and exit code 2:

```text
Error: cannot reach the engine at 127.0.0.1:9: Connection refused (os error 111) (start the engine or set OXA_ENGINE_URL)
```

Several endpoints, or a per-model setup, go in `~/.oxa/agent/models.json`. See [settings.md](settings.md) for the settings that choose a model.

oxa is tuned to the models the ox-llm engine serves. [tools-context.md](tools-context.md) lists the tool formats and the context defaults, and the settings that change them.

## First prompt

Start oxa in a project directory:

```console
$ cd ~/src/my-project
$ oxa
```

oxa opens the full-screen interface. Type a request in the editor at the bottom and press Enter. The model reads files with its tools, and the answer appears in the transcript above the editor. Ctrl+C clears the editor, Ctrl+D exits when the editor is empty, and Escape stops a running turn.

The default permission mode is `workspace`: reads and writes inside the project run without a prompt, and bash runs in the sandbox. Writes outside the project are asked for. Bash has network access by default in this mode, so a command can send what it reads to a host. See [security.md](security.md).

## Print mode

`-p` (`--print`) runs the prompt, prints the answer to stdout and exits. It is the form to use in scripts:

```sh
oxa -p "Say hello"
oxa -p --tools read,ls "List the files"
```

`--mode json` prints every event as one JSON line instead of plain text:

```sh
oxa --mode json -p "hi"
```

Print mode still reads the project's settings, skills and context files, but project-local code (extensions, hooks, MCP servers) loads only in a trusted project. Use `--approve` for a single run in a project you trust, or `--no-approve` to ignore project-local configuration. See [rpc.md](rpc.md) for the output formats.

## Where files live

oxa keeps user files in the agent directory, `~/.oxa/agent/` (change it with `OXA_AGENT_DIR`):

| Path | What |
|---|---|
| `settings.json` | user settings ([settings.md](settings.md)) |
| `models.json` | engine endpoints |
| `keybindings.json` | key overrides ([usage.md](usage.md#keys)) |
| `mcp.json`, `mcp-auth.json` | MCP servers and their OAuth tokens ([mcp.md](mcp.md)) |
| `brave-search.key` | Brave Search key for `web_search` ([tools.md](tools.md#web_search)) |
| `trust.json` | saved project trust decisions and content approvals |
| `identity.md` | the identity text at the start of the system prompt |
| `skills/`, `prompts/`, `themes/`, `extensions/`, `agents/` | user resources ([skills.md](skills.md), [extensions.md](extensions.md)) |
| `sessions/` | session files, one directory per project ([sessions.md](sessions.md)) |
| `tmp/` | spill files for long tool output |

Project files live in `.oxa/` at the project root: `settings.json`, `mcp.json`, `skills/`, `prompts/`, `themes/`, `extensions/` and `agents/`. Project files are read only when the project is trusted. Context files (`AGENTS.md`, `CLAUDE.md`) are read from the project and its parents; see [skills.md](skills.md#context-files).

Session files are written with mode `0600` in `0700` directories, and the MCP token file is `0600`.

## Next steps

- [usage.md](usage.md): the interface, keys and slash commands.
- [cli.md](cli.md): every flag and subcommand.
- [security.md](security.md): what the model can reach, and how to limit it.
- [tools.md](tools.md): the built-in tools and `web_search`.
