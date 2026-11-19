# oxa

`oxa` is the terminal coding agent and the front door of the ox suite. It runs cloud models through its own gateway (OpenCode Go, OpenRouter, OpenAI, Anthropic and a ChatGPT plan) and local models through the `oxe` engine, which it starts itself. The model's tools may use the network; see [docs/pages/security.md](docs/pages/security.md). It reads and edits your project with a small set of tools, runs bash in a `bubblewrap` sandbox on Linux, keeps sessions as trees, and compacts its context to fit the window of the model. It has an interactive terminal interface, a print mode, a JSON event stream and an RPC mode, and it can be extended with skills, JavaScript extensions, MCP servers and a `web_search` tool.

Status: v0.2.1, part of ox-llm. oxa is a Rust port of the pi coding agent (MIT). See `LICENSE` and `NOTICE` for the licence and the attribution.

## Quickstart

Install a release or a dev build with [docs/pages/install.md](docs/pages/install.md). To build from source, run this from the `oxa/` directory of the checkout:

```console
$ cargo build --release --locked -p oxa-cli
$ export PATH="$PWD/target/release:$PATH"
$ export OXA_ENGINE_URL=http://127.0.0.1:8080
$ cd ~/project
$ oxa
```

oxa needs a model before the first prompt: a provider key for a cloud model, or `oxa local add <model.gguf> --enable` for a local one. [docs/pages/quickstart.md](docs/pages/quickstart.md) has both. `OXA_ENGINE_URL` names an engine you started yourself (`oxe-server` listens on `http://127.0.0.1:8080`); with no model configured oxa dials nothing and says what to configure, and with a dead address it stops with an error that names it. See [tools and context handling](docs/pages/tools-context.md) for the defaults. For one answer from a script, use print mode:

```console
$ oxa -p --tools read,ls "List the files"
```

## Documentation

The documentation is in [docs/](docs/README.md):

- [install](docs/pages/install.md), [quickstart](docs/pages/quickstart.md) and [modes](docs/pages/modes.md): install, cloud and local models, first prompt, permission modes
- [usage](docs/pages/usage.md): the interface, keys and slash commands
- [cli](docs/pages/cli.md) and [settings](docs/pages/settings.md): every flag, variable and key
- [sessions](docs/pages/sessions.md), [compaction](docs/pages/compaction.md) and [tools and context](docs/pages/tools-context.md)
- [tools](docs/pages/tools.md), [skills](docs/pages/skills.md), [extensions](docs/pages/extensions.md) and [mcp](docs/pages/mcp.md)
- [security](docs/pages/security.md) and [SECURITY.md](SECURITY.md): permission modes, the sandbox, network and offline mode, and how to report a vulnerability
- [rpc](docs/pages/rpc.md): print and JSON output, and the RPC protocol

## Layout

| Crate | Layer | What |
|---|---|---|
| `oxa-proto` | 0 | shared types and traits (the wire, events, hooks, errors) |
| `oxa-engine` | 1 | HTTP and SSE client for the engine; request builder |
| `oxa-session` | 1 | session files (JSON Lines tree), transcript fold, tree operations |
| `oxa-tools` | 1 | the seven built-in tools and output shaping |
| `oxa-sandbox` | 1 | `bubblewrap` sandbox for bash |
| `oxa-perm` | 1 | permission modes, access policy, project trust |
| `oxa-compact` | 1 | compaction and branch-summary policy |
| `oxa-config` | 1 | settings, keybindings, model endpoints, environment, directories |
| `oxa-resources` | 1 | context files, skills, prompt templates, packages, built-in skills |
| `oxa-script` | 1 | embedded QuickJS runtime |
| `oxa-ext` | 2 | JavaScript extensions and exec hooks |
| `oxa-codemode` | 2 | codemode scripts, `tool_search` and `call_tool` |
| `oxa-mcp` | 2 | MCP client: stdio and HTTP transports, OAuth |
| `oxa-web` | 2 | the `web_search` tool (Brave Search) |
| `oxa-core` | 3 | agent loop, session runtime, SDK surface |
| `oxa-tui` | 4 | terminal UI library: components, editor, markdown, themes |
| `oxa-interactive` | 5 | interactive mode: layouts, selectors, dialogs, slash commands |
| `oxa-rpc` | 5 | print, JSON and RPC modes; Rust RPC client |
| `oxa-export` | 5 | HTML export of sessions |
| `oxa-cli` | 6 | the `oxa` binary |
| `oxa-fake` | test | fake and scripted engines, invariant checks, test isolation |
| `oxa-parity` | test | behaviour suite run against the built binary |

Twenty product crates and two test crates. The product crates have no async runtime; they use threads and channels. Each crate has a `README.md` with its modules and contracts.

## Files that steer the work

- `PARITY.md`: oxa’s behavior regression contracts, retaining historical row IDs and pi provenance (P: inherited behavior; A: adapted behavior), with the tests that check them.
- `docs/fake-script.md`: the script format of the fake engine.
- [Hosted checks](../.github/workflows/checks.yml) build and test the products. `ci/check_net_surface.py` checks where the code connects to the network.

## Build and test

```console
$ cargo check --workspace --all-targets
$ cargo build -p oxa-cli -p oxa-fake
$ OXA_REQUIRE_BWRAP=1 cargo test --workspace --no-fail-fast
```

The tests that need `bwrap` are skipped with a message when it is missing, unless `OXA_REQUIRE_BWRAP=1` makes a missing `bwrap` a failure. CI sets it. The release profile unwinds on panic, so that panics stay contained at their boundaries.

## What the engine sees

- Every request names its session (`X-Session-Id`). The idle prefill, the park before a subagent and the boot preload (a zero-token prefill of the frozen prefix when a session opens) carry `X-Oxe-Snapshot: keep` when the engine advertises checkpoints, and only then.
- A `subagent` child is a separate `oxa` process with its own session id and background priority. It dies with its parent (parent-death signal), and an abort ends its process group.

## Known limits

- `!cmd` in the interface and stdio MCP servers run with your rights and outside the bash sandbox. In offline mode they run through `bubblewrap` without network, and they are not otherwise limited.
- The package caches that the sandbox leaves writable are shared with your own builds.
- oxa does not update itself. Install a new build to upgrade.

## Licence

MIT. See `LICENSE` for the terms and `NOTICE` for the attribution to the pi coding agent and the other third-party code in the tree.
