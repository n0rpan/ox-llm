# Quickstart

This page takes an installed `oxa` to its first prompt, on a cloud model or on a local one. To install it, see [install.md](install.md). For every flag see [cli.md](cli.md); for the terminal UI see [usage.md](usage.md).

`oxa` is the front door of the ox suite. It talks to the cloud providers itself and it starts the local engine itself. There is no other program to start first.

## Requirements

- `oxa` on your `PATH`, or its full path ([install.md](install.md)).
- A model: a provider key for a cloud model, or a GGUF file and the `oxe` engine for a local model.
- For the bash tool on Linux: `bubblewrap` (`bwrap`) installed and unprivileged user namespaces enabled. Without a usable `bwrap` the bash tool is disabled and oxa says so at startup. On macOS there is no OS sandbox; see [modes.md](modes.md#macos).

## Cloud models

### Keys

Provider keys live in one private file, outside any project: `~/.config/oxc/llm/cloud-secrets.json`. It must be a regular file that you own, with mode `0600`. Each provider has one entry:

```json
{
  "opencode": { "api_key": "..." },
  "openrouter": { "api_key": "..." },
  "openai": { "api_key": "..." },
  "anthropic": { "api_key": "..." }
}
```

List only the providers you use. oxa reads a key when a request needs it and never puts it in the environment or a log. Keep keys out of `settings.json`.

Create the file private before a key goes in it, then paste the key in your editor:

```console
$ mkdir -p ~/.config/oxc/llm
$ install -m 600 /dev/null ~/.config/oxc/llm/cloud-secrets.json
```

The Brave Search key for `web_search` is a file of its own that holds only the key. This stores it without showing it on screen or in the shell history:

```console
$ (umask 077; read -rs KEY && printf '%s\n' "$KEY" > ~/.oxa/agent/brave-search.key)
```

Without a Brave key the `web_search` tool is not offered to the model at all. The ChatGPT plan uses no key: sign in with `oxa login chatgpt --experimental`.

The first interactive start of a fresh install lists, once, which of these are not set up yet: the Brave key, each cloud provider and the ChatGPT sign-in. It is a note, nothing is blocked, and `quietStartup` hides it. oxa checks only whether a key exists and never prints one.

### OpenCode Go

A Go key is enough: the `opencode` entry in the key file turns Go on.

```console
$ oxa --list-models
$ oxa --model opencode/<model>
```

Go is a subscription. oxa counts its requests and tokens and records any paid charge that the provider reports. Set `oxa.cloud.opencode.enabled` to `false` in your user settings to turn Go off while the key stays in place.

### OpenRouter, OpenAI and Anthropic

These are metered providers. Put the key in the protected key file (see above) and that is all: the gateway and model discovery turn on by themselves, and the current Anthropic and OpenAI models are built in with their limits and prices. Set `oxa.engine.inProcessCloud` to `false` to opt out. A budget is optional; merge this into `~/.oxa/agent/settings.json` to set one:

```json
{
  "oxa": {
    "cloud": {
      "daily_budget_usd": 10
    }
  }
}
```

`oxa --list-models` and the `/model` picker then show the discovered models. `daily_budget_usd` is optional. Without it there is no budget check, and every request is still recorded with its cost. With it, a request that would pass the cap is refused, and so is a model whose price is not known.

To name a model yourself, with its own limits and prices, add it under `oxa.cloud.models`:

```json
{
  "oxa": {
    "engine": { "inProcessCloud": true },
    "cloud": {
      "models": {
        "example-cloud": {
          "provider": "anthropic",
          "upstream_model": "replace-with-your-model",
          "context_window": 128000,
          "max_output": 8192,
          "pricing": {
            "input_per_million": 2,
            "output_per_million": 3,
            "cache_read_per_million": 0.2,
            "cache_write_per_million": 2.5
          }
        }
      }
    }
  }
}
```

The provider is `anthropic`, `openai`, `openrouter` or `chatgpt`. The numbers above are an illustration: use the provider's current limits and prices. Then start with `oxa --model example-cloud`.

These settings count only in your user settings file. A project cannot turn a provider on, change a budget or point oxa at another key file. Every field is described in [settings.md](settings.md#native-cloud-gateway).

### ChatGPT plan

Sign in once, in your own terminal:

```console
$ oxa login chatgpt --experimental
```

That is all: the plan models appear as `chatgpt/gpt-6.2-astra`, `chatgpt/gpt-6.2-sol` and the other Codex plan models, with no route and no limits to write. To add your own route or advisory daily limits (a warning at 80% and 100%, never a refusal), use:

```json
{
  "oxa": {
    "engine": { "inProcessCloud": true },
    "cloud": {
      "subscription_limits": { "daily_requests": 500, "daily_tokens": 5000000 },
      "models": {
        "example-plan": {
          "provider": "chatgpt",
          "upstream_model": "replace-with-your-model",
          "context_window": 128000,
          "max_output": 8192
        }
      }
    }
  }
}
```

The sign-in is experimental. `oxa logout chatgpt` removes the saved token. See [cli.md](cli.md#experimental-chatgpt-sign-in).

### What a cloud request costs

Usage is written to a private ledger in `~/.oxa/agent/cloud-ledger/`. In a session, `/usage` shows the session's statistics; `oxa usage --json` prints today's sums per pool. `--offline` refuses every cloud request.

## Local models

A local model is a GGUF file on your disk. The `oxe` engine runs it on the CPU. oxa owns the engine: it starts `oxe-server` when the first request needs it, keeps one model loaded, and unloads it after two hours without a request.

Wire a model in with one command, in your own terminal:

```console
$ oxa local add /path/to/model.gguf --enable
```

The command:

1. finds `oxe` and `oxe-server` in the directory of the running `oxa`. It does not search `PATH`; `--engine-dir <dir>` names another install.
2. asks the engine what the model needs. The engine reads only the GGUF header and reports the context window and the memory the model takes.
3. writes the model into your user settings, after saving a copy of the file as `settings.json.bak-<unix seconds>`.
4. with `--enable`, turns the local engine on (`oxa.engine.inProcessLocal`).

The model's name is the file name in lower case, without `.gguf`. Choose another with `--name`:

```console
$ oxa local add /path/to/model.gguf --name my-model --enable
$ oxa --model my-model
```

Other options: `--ctx <n>` for a smaller context window, `--max-output <n>` for a lower output limit, and `--replace` to write over a model of the same name. Everything after `--` goes to the engine as launch arguments.

What to expect:

- oxa downloads no weights. A missing file is reported with the setting to fix.
- A model loads only when free memory covers the whole file plus what the engine needs for the context. If it does not fit, oxa says how much is free and how much is needed. A smaller `--ctx` needs less.
- `oxa` and the engine must come from the same build. An engine of another version is refused.
- The local engine keeps working with `--offline`.

Local and cloud models work side by side: with both turned on, `/model` lists them together. The settings that `oxa local add` writes are described in the [native local configuration](../../oxc-local/CONFIGURATION.md).

## Another engine

oxa can also talk to any server that speaks the OpenAI-compatible HTTP API, for example an engine you start by hand. Name its address in `OXA_ENGINE_URL`, or list several endpoints in `~/.oxa/agent/models.json`:

```console
$ export OXA_ENGINE_URL=http://127.0.0.1:8080
```

When no model is configured at all, oxa has nothing to talk to. It stops with one line that names the address it tried, and exit code 2.

## First prompt

Start oxa in a project directory:

```console
$ cd ~/project
$ oxa
```

oxa opens the full-screen interface. Type a request in the editor at the bottom and press Enter. The model reads files with its tools, and the answer appears in the transcript above the editor. Ctrl+C clears the editor, Ctrl+D exits when the editor is empty, and Escape stops a running turn. `/model` switches the model, and the `defaultModel` setting chooses the one to start with.

The default permission mode is `auto`: oxa does not stop to ask, bash runs in the sandbox, and the file tools still refuse credentials and other protected paths. Use `--standard` to be asked before writes outside the project, or `--yolo` for full access without the sandbox. Bash has network access by default, so a command can send what it reads to a host. [modes.md](modes.md) explains each mode.

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
| `models.json` | endpoints of engines you run yourself |
| `keybindings.json` | key overrides ([usage.md](usage.md#keys)) |
| `mcp.json`, `mcp-auth.json` | MCP servers and their OAuth tokens ([mcp.md](mcp.md)) |
| `brave-search.key` | Brave Search key for `web_search` ([tools.md](tools.md#web_search)) |
| `trust.json` | saved project trust decisions and content approvals |
| `identity.md` | the identity text at the start of the system prompt |
| `skills/`, `prompts/`, `themes/`, `extensions/`, `agents/` | user resources ([skills.md](skills.md), [extensions.md](extensions.md)) |
| `sessions/` | session files, one directory per project ([sessions.md](sessions.md)) |
| `cloud-ledger/` | the private record of cloud requests and their cost |
| `tmp/` | spill files for long tool output |

Provider keys and the ChatGPT token are not in the agent directory. They are in `~/.config/oxc/llm/` (`cloud-secrets.json`, `chatgpt-auth.json`), and `OXA_AGENT_DIR` does not move them.

Project files live in `.oxa/` at the project root: `settings.json`, `mcp.json`, `skills/`, `prompts/`, `themes/`, `extensions/` and `agents/`. Project files are read only when the project is trusted. Context files (`AGENTS.md`, `CLAUDE.md`) are read from the project and its parents; see [skills.md](skills.md#context-files).

Session files are written with mode `0600` in `0700` directories, and the MCP token file is `0600`.

## Next steps

- [modes.md](modes.md): standard, auto, yolo and plan.
- [usage.md](usage.md): the interface, keys and slash commands.
- [cli.md](cli.md): every flag and subcommand.
- [security.md](security.md): what the model can reach, and how to limit it.
- [tools.md](tools.md): the built-in tools and `web_search`.
