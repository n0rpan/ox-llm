# Architecture

Three products in one repository, three build units, one tested combination per commit.

## Components

| Layer | Product | Runs as | Talks to |
|---|---|---|---|
| Client | `oxa` (`oxa/`), or any OpenAI-compatible client | a CLI/TUI on the user's machine | the controller over HTTP (`/v1/chat/completions`) |
| Controller | `oxc` (`oxc/src/oxc/runtime/`) | user services on loopback `:8092` | engines over HTTP with a per-launch token |
| Engines | `oxe-server`, Flash, `oxi` (`engine/crates/`) | one process per loaded model, on internal ports | nothing (leaf) |
| Desktop | Omarchy panel widget, Studio (`oxc/src/oxc/omarchy_plugin/`, `runtime/studio`) | the panel, a local web page | the controller |
| Tooling | bootstrap, staging, qualification, benchmarks (`oxc/tools/`, `oxc/benchmarks/`) | by hand or in CI | the products and receipts |

Details: [oxc/docs/ARCHITECTURE.md](oxc/docs/ARCHITECTURE.md) covers the controller and Studio,
[oxc/docs/OPENAI-API.md](oxc/docs/OPENAI-API.md) the client-facing API and
[engine/README.md](engine/README.md) the engines.

## The engine contract (client-neutral)

- **Plain OpenAI-compatible API.** Any client works. No client version pins, client byte templates or
  per-client code in `engine/` or `oxc/src`. Client-specific behaviour lives only in thin optional
  adapters at the edge (the OpenCode integration is legacy), never in the core.
- **Generic cache mechanisms in the engine:** exact prefix reuse, token-exact echo of generated ids,
  and state snapshots/checkpoints as a mechanism with a bounded store.
- **Policy in the client:** when to compact or checkpoint, how to shape history and how to handle
  sessions. A client may send small, documented, client-neutral hints that any client can use.
- **Quality classes:** each engine change is lossless (bit-identical), reorder (within the reference's
  measured noise) or lossy (opt-in only). Bit identity is the bug detector.

## Build units

| Unit | Root | Lockfile | CI (self-hosted helpers, path-filtered) |
|---|---|---|---|
| engine | `engine/Cargo.toml` (workspace) | `engine/Cargo.lock` | `ci-engine.yml` on `lab-arm64` |
| oxa | `oxa/Cargo.toml` (workspace) | `oxa/Cargo.lock` | `ci-oxa.yml` on `lab-arm64` |
| repository | none | none | `ci-repo.yml` on `lab-x64`: secret and SDK scans, isolation, release tooling |

There is no root cargo workspace and no shared `Cargo.lock`. `oxa/` must not have a `path =`
dependency outside `oxa/`, and `engine/` none outside `engine/`; `ci-repo.yml` enforces this. The only
coupling between oxa and the engine is HTTP.

## Public boundary

Public releases are squashed snapshots built by `tools/release/build_public_snapshot.sh`. It applies
one combined exclude list (the root list plus each product's `tools/public-exclude.txt`), then runs the
secret, private-word, receipt, private-path, file-type, SDK-material and workflow scans. Private CI is
self-hosted only. The public repository gets GitHub-hosted workflows from
`tools/release/public-workflows/`. `engine/npu/` stays out of every public tree except
`engine/npu/oxn/common/`, which the default CPU build includes.
