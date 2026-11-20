# ox-llm

The ox suite is a terminal coding agent that works with local models, cloud models, or both. It is
written in Rust and ships as one release. v0.2.1 has no Python, no controller daemon and no
background service: you start `oxa`, and `oxa` starts what it needs.

| Part | What you run | What it is |
|---|---|---|
| **oxa** | `oxa` | The agent and the front door: a terminal UI, a print mode for scripts, tools that read and edit your project, sessions, skills and MCP servers. It talks to the cloud providers itself and owns the local engine process. |
| **oxe** | started by `oxa` (`oxe-server`, `oxe`) | The engine for local models: CPU inference for 35B mixture-of-experts GGUF models on Snapdragon X laptops under Linux. |
| **ox AI panel** | an Omarchy bar widget | Shows models, usage, the lead session and workers. It runs only `oxa` and reads the JSON that `oxa` prints. In development: the source is in [`oxa/omarchy-panel/`](oxa/omarchy-panel) and it is not in the v0.2.1 packages yet. |

## Install

**Stable.** Each release attaches `ox-llm-<version>-linux-aarch64.tar.gz` and
`ox-llm-<version>-linux-x86_64.tar.gz` with `SHA256SUMS`, an SBOM and build provenance. The
archive holds one `usr/` tree with `oxa`, `oxe`, `oxe-server` and
`local-ai-memlat-boot`, plus licence files and four inert MEMLAT templates.

```console
$ sha256sum -c SHA256SUMS
$ mkdir ~/ox-llm-<version>
$ tar -xzf ox-llm-<version>-linux-aarch64.tar.gz -C ~/ox-llm-<version>
$ ~/ox-llm-<version>/usr/bin/oxa --version
```

Keep `oxa`, `oxe` and `oxe-server` in the same directory: `oxa` looks for the engine beside itself
and refuses an engine from another build. The full steps, the Arch packages and the upgrade from
v0.2.0 are in [install.md](oxa/docs/pages/install.md).

**Dev.** Every push to `main` publishes a rolling `dev` build. `tools/lab/dev-update.sh` downloads
it, checks the sums and the provenance, and installs it under
`~/.local/state/oxc/dev-builds/default/` without touching a stable install or your `PATH`. See
[docs/DEV.md](docs/DEV.md). On macOS (Apple silicon) the twin script is
`tools/lab/dev-update-macos.sh`; macOS support is in progress and no macOS build is published yet.

**From source.** `cd oxa && cargo build --release --locked -p oxa-cli` builds `oxa`;
`cd engine && cargo build --release --locked -p server --bin oxe-server` builds the engine.

Linux needs `bubblewrap` (`bwrap`) for the bash sandbox. The local engine is built for Snapdragon X
(ARM64) under Linux; on x86_64 and on macOS, use cloud models.

## First run with a cloud model

Keys live in one private file, outside any project: `~/.config/oxc/llm/cloud-secrets.json`, owned
by you, mode `0600`. Put the key of each provider you use in it:

```json
{
  "opencode": { "api_key": "..." },
  "openrouter": { "api_key": "..." },
  "openai": { "api_key": "..." },
  "anthropic": { "api_key": "..." }
}
```

- **OpenCode Go** (subscription): the key is enough. `oxa --list-models` lists the Go models, and
  `oxa --model opencode/<model>` starts a session on one.
- **OpenRouter, OpenAI, Anthropic** (metered): also set `oxa.engine.inProcessCloud` and
  `oxa.cloud.discover_models` to `true` in `~/.oxa/agent/settings.json`.
- **ChatGPT plan** (subscription): `oxa login chatgpt --experimental`, then add a `chatgpt` model.

The settings for each provider, prices and the optional daily budget are in
[quickstart.md](oxa/docs/pages/quickstart.md#cloud-models). Then:

```console
$ cd ~/src/my-project
$ oxa
```

Type a request and press Enter. `/model` switches the model. For one answer in a script:
`oxa -p "Say hello"`.

## First run with a local model

Point `oxa` at a GGUF file you already have. It asks the engine what the model needs and writes the
settings for you:

```console
$ oxa local add /path/to/model.gguf --enable
$ oxa --model model
```

`oxa` starts `oxe-server` when the first request needs it and unloads it when idle. It downloads no
weights, and it refuses to load a model that does not fit in the memory that is free. Details:
[quickstart.md](oxa/docs/pages/quickstart.md#local-models).

## Modes

The mode decides how much the agent may do without asking. The status bar always shows it.

| Mode | What it does |
|---|---|
| `standard` | Asks before a write outside the project and other steps that reach beyond it. Bash runs in the sandbox. |
| `auto` (default) | Does not stop to ask. Bash still runs in the sandbox, and the file tools still refuse credentials and other protected paths. |
| `yolo` | Full power. No questions, no sandbox, no protected paths: bash runs as you, with your files, your credentials and your network. Use it only in a session you watch, on a machine and a project you can restore. |
| `plan` | Read-only: the agent reads and plans, and changes nothing. |

On macOS there is no OS sandbox in any mode. Bash runs with your user rights and `oxa` says so at
start. The guards inside `oxa` (protected paths, permissions, project trust) stay on in `standard`
and `auto`.

Start with `oxa --standard`, `oxa --auto` or `oxa --yolo`, or switch in a session with `/mode`.
The whole picture is in [modes.md](oxa/docs/pages/modes.md).

## Lead and workers

`oxa work` runs tasks unattended, each in its own git worktree, and accepts a task only when its
checks pass. `oxa lead` is a dedicated session that watches the work board and the workers,
answers your questions about them and proposes the next step; `oxa lead serve` shows the same
state as a local web page. The lead is off until you enable it in your user settings. See
[work.md](oxa/docs/work.md) and [lead.md](oxa/docs/lead.md).

## Where the docs live

- [oxa/docs/](oxa/docs/README.md): the user documentation. Start with
  [install](oxa/docs/pages/install.md), [quickstart](oxa/docs/pages/quickstart.md) and
  [modes](oxa/docs/pages/modes.md); then [usage](oxa/docs/pages/usage.md),
  [cli](oxa/docs/pages/cli.md), [settings](oxa/docs/pages/settings.md) and
  [security](oxa/docs/pages/security.md).
- [docs/release-notes/v0.2.1.md](docs/release-notes/v0.2.1.md): what changed since v0.2.0.
- [docs/NATIVE-INSTALL.md](docs/NATIVE-INSTALL.md): upgrading a v0.2.0 install.
- [engine/README.md](engine/README.md): the engine, its models and how to build it.
- [ARCHITECTURE.md](ARCHITECTURE.md), [DECISIONS.md](DECISIONS.md),
  [KNOWN-ISSUES.md](KNOWN-ISSUES.md), [ROADMAP.md](ROADMAP.md), [CHANGELOG.md](CHANGELOG.md).
- [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md): how to send a change.
  [SECURITY.md](SECURITY.md): how to report a vulnerability.

## Licence

Our code is MIT ([LICENSE](LICENSE)). Vendored and ported code keeps its own licence. oxa ports the
pi coding agent (MIT) and statically links QuickJS (MIT); both licence texts are in
[NOTICE](NOTICE). Each part lists its components in its own `THIRD_PARTY_NOTICES`; the root
[NOTICE](NOTICE) and [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) index them. There are no model
weights and no vendor SDK, firmware or runtime files in this repository.

Snapdragon and Hexagon are trademarks of Qualcomm Incorporated. This is an independent project, not
affiliated with, sponsored by, or endorsed by Qualcomm.
