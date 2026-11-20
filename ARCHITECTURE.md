# Architecture

Three build units, coupled only by HTTP and by process spawn. This page describes what exists in the
repository today; the target design lives in the issues and in each product's own README.

## Shipped binaries (`packaging/release/build.sh`, `stage.py`)

| Binary | Source | Role |
|---|---|---|
| `oxa` | `oxa/oxa-cli` (+ internal crates) | Front door: TUI, print/json/rpc modes, tools, lead, board, in-process cloud gateway |
| `oxe-server` | `engine/crates/server` | Local inference, OpenAI-compatible HTTP + SSE (hyper/tokio), one process per model |
| `oxe` | `engine/crates/oxe-cli` | Engine CLI: run, bench, prefill-bench |
| `local-ai-memlat-boot` | `oxa/oxc-memlat` | Boot-menu / rollback helper for the memlat kernel setup |
| `oxc-native` | `oxc-rs` | Parser-only stub of the old `oxc` CLI; retiring |

Not staged in the release: `oxi` (image engine, built in CI), both QML panels, all Python.

## oxa workspace

- Base: `oxa-proto` (types, no internal deps) is the root of the graph that everything else builds on.
- Config and policy: `oxa-config` (settings, models, env table), `oxa-perm` (modes, trust),
  `oxa-sandbox` (bwrap; the only sandbox implementation).
- Agent core: `oxa-core` (agent loop, session, compaction, subagent, goal) on `oxa-session`,
  `oxa-tools`, `oxa-compact`, `oxa-resources` and `oxa-engine`.
- Model access:
  - `oxa-engine`: hand-written HTTP/1.1 + SSE client to an engine or controller at `OXA_ENGINE_URL`
    (`/v1/chat/completions`).
  - `oxc-gateway`: cloud providers in-process (Anthropic, OpenAI Responses + ChatGPT sign-in,
    OpenRouter, OpenCode Go) with ledger, budget and pricing; reqwest/tokio. Used when
    `oxa.engine.inProcessCloud` is set. Feature `share` (LAN share listener) is off in the release build.
  - `oxc-local`: owns one local `oxe-server` child (launch, admission, queue, memory check); a
    default-off setting.
- UI: `oxa-tui` (widgets, markdown, syntect) under `oxa-interactive` (app, driver, selectors, skins);
  `oxa-export` (HTML export); `oxa-rpc` (rpc mode).
- Extensions: `oxa-script` (QuickJS) under `oxa-ext` and `oxa-codemode`; `oxa-mcp` (stdio and
  streamable HTTP, OAuth); `oxa-web` (Brave search tool).
- Work and lead: `oxa-work` (task runner, board, claims, standing store, GitHub review, landing) plus
  `oxa-cli/src/lead_*.rs` and `work_*.rs` (loopback web page on `oxa.lead.serve.port`, peer proof, quotas).
- Not in the release binary: `oxa-a2a` (no dependents), `oxa-orchestrator` (only behind test-enabled
  features `managed-steering` and `a2a-control`), `oxa-fake` (fake engine for tests), `oxa-parity`
  (pi parity test suite).
- Panel: `oxa/omarchy-panel/*.qml` runs read-only `oxa` commands and renders their JSON. Not packaged yet.

## engine workspace

- `server` depends on `model` (Rust MoE engine: prefill, attention, MTP), which depends on
  `kernels-sys` (C++ kernels), `sched` (thread teams), `gguf` and `oxn` (NPU/shared-memory layer).
- `server --features flash` adds `qwen4exp` (the "Flash" engine), which links vendored ggml C++ from
  `engine/vendor/`. Off in the release build command.
- `chat` (minijinja templates, reply parser) and `tokenizer` (no deps).
- Image stack: `image-flow` (bin `oxi`) on `image-dit`, `image-vae`, `qwen3-te` and `int8-gemm`;
  `image-npu` (bin `plan_klein`, called only from the Python Studio).
- Several binaries are proofs and benches (`flash-m*`, `oxn-*`, `isa-roof`, ...) and are not shipped.
- Behaviour is steered by about 100 `OXE_*` environment variables read in `server` and `model`.

## Python (`oxc/`)

- `oxc/src/oxc/runtime`: the controller on loopback `:8092` (routes, queue, memory admission, cloud
  proxies, ledger, LAN share, Studio image UI). Still the default path for local models.
- `oxc/src/oxc/omarchy_plugin`: the older QML panel.
- `oxc/tests`: tests for the above, run by the lab job.
- `oxc/benchmarks/results`: raw benchmark receipts. `oxc/tools`, `engine/tools` and `tools/ci`: lab and
  CI scripts.

## How they talk

```
panel (QML) --exec--> oxa status/usage (JSON on stdout)
oxa --HTTP/SSE--> Python controller :8092 --HTTP + per-launch token--> oxe-server / flash / oxi
oxa --HTTP/SSE--> oxe-server directly (OXA_ENGINE_URL, or oxc-local owning the child)
oxa --in-process (oxc-gateway, HTTPS)--> Anthropic / OpenAI / ChatGPT / OpenRouter / OpenCode Go
oxa --spawn--> child oxa (subagents, workers in git worktrees), bwrap for bash, MCP servers (stdio/HTTP)
oxa lead --loopback HTTP--> browser page; oxa work/lead --> GitHub (board, reviews, CI probes)
```

## Engine contract (client-neutral)

- **Plain OpenAI-compatible API.** Any client works. No client version pins, client byte templates or
  per-client code in `engine/` or `oxc/src`. Client-specific behaviour lives only in thin optional
  adapters at the edge, never in the core.
- **Generic cache mechanisms in the engine:** exact prefix reuse, token-exact echo of generated ids,
  and state snapshots and checkpoints as a mechanism with a bounded store.
- **Policy in the client:** when to compact or checkpoint, how to shape history and how to handle
  sessions. A client may send small, documented, client-neutral hints.
- **Quality classes:** each engine change is lossless (bit-identical), reorder (within the reference's
  measured noise) or lossy (opt-in only). Bit identity is the bug detector.

## CI (`.github/workflows`)

- `checks.yml` runs on PRs, pushes, the merge queue and nightly. It calls `build.yml` (engine MoE,
  Flash, the oxa test archive, four oxa test shards and a doctest job) and runs the documentation
  links job, the path-filter tests and the secret scans. A main push whose commit already has a
  green merge-queue run skips the jobs that run would repeat, unless the push changed a
  dependency cache key (main is what seeds those caches); the classifier and the scans always run.
- `lane.yml` runs the affected-crate tests for a lane; `e2e.yml` runs the real-model prompt and tool
  loop; `oracle.yml` checks the gate and cache controls.
- `dev-build.yml` builds each architecture once per main push for the rolling `dev` prerelease. The
  two-build reproducibility comparison runs in its nightly schedule and in the release lane, its Arch
  source package rebuild runs beside the build, and the model-free source tests are skipped only for a
  commit the merge queue already covered.
- `macos.yml` (native macOS arm64 oxa-cli build) is called by `checks.yml` as the required `macos`
  job; standalone it runs on manual dispatch only.
- Also: `release-build.yml`, `release.yml`, `dev-build.yml`, `codeql.yml`,
  `go-review.yml`.

| Unit | Root | Lockfile |
|---|---|---|
| engine | `engine/Cargo.toml` (workspace) | `engine/Cargo.lock` |
| oxa | `oxa/Cargo.toml` (workspace) | `oxa/Cargo.lock` |
| repository | none | none |

## Naming traps

- "oxc" means three things: the Python controller (`oxc/`), the retiring Rust CLI stub (`oxc-rs/`),
  and three Rust crates inside the oxa workspace (`oxc-gateway`, `oxc-local`, `oxc-memlat`).
