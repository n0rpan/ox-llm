# Quickstart

This page gets `oxa` running against a local engine and sends a first prompt. For every flag see [cli.md](cli.md); for the terminal UI see [usage.md](usage.md).

## Requirements

- A Rust toolchain; hosted checks pin Rust 1.99.1.
- For native local models: a matched `oxe-server` binary beside `oxa` and an existing GGUF configured in user settings. Native local and cloud routing are enabled by default; no Python controller is required. See the [native install guide](../../../docs/NATIVE-INSTALL.md).
- For the bash tool: Linux with `bubblewrap` (`bwrap`) installed and unprivileged user namespaces enabled. Without a usable `bwrap` the bash tool is disabled and oxa says so at startup.

## Build

Run these commands from the `oxa/` directory of the checkout:

```console
$ cargo build --release --locked -p oxa-cli
$ ./target/release/oxa --version
```

The binary is `target/release/oxa`. Copy it somewhere on your `PATH`, or run it by its path.

## Point oxa at the engine

The Rust-only defaults select the in-process gateway and local engine. Configure
local model paths and limits in the user-only `oxa.local` object, or cloud model
catalog and budget in `oxa.cloud`; choose a configured model with `--model` or
`defaultModel`. See [native local configuration](../../oxc-local/CONFIGURATION.md)
and [settings.md](settings.md). Missing weights or invalid native settings produce
an error; they never silently select a controller on port 8092.

For an intentionally external OpenAI-compatible HTTP engine, explicitly set both
`oxa.engine.inProcessCloud` and `oxa.engine.inProcessLocal` to `false` in user
settings, then select its address with `OXA_ENGINE_URL` or `models.json`.
The legacy `http://127.0.0.1:8092` address is only this opt-out transport's
compatibility default. Project settings cannot switch the user transport.

oxa is tuned to the models the ox-llm engine serves. [tools-context.md](tools-context.md) lists the tool formats and the context defaults, and the settings that change them.

## First prompt

Start oxa in a project directory:

```console
$ cd ~/project
$ oxa
```

oxa opens the full-screen interface. Type a request in the editor at the bottom and press Enter. The model reads files with its tools, and the answer appears in the transcript above the editor. Ctrl+C clears the editor, Ctrl+D exits when the editor is empty, and Escape stops a running turn.

The default permission mode is `auto`: oxa does not stop to ask, bash runs in the sandbox, and the file tools still refuse credentials and other protected paths. Use `--standard` to be asked before writes outside the project, or `--yolo` for full access without the sandbox. Bash has network access by default in this mode, so a command can send what it reads to a host. See [security.md](security.md).

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
