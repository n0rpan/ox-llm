# ox-llm

Local large language models, a coding agent and image generation on **Snapdragon X laptops under
Linux** (Omarchy), on the CPU. Nothing leaves the machine. Every speed number links its receipt.
Failed attempts are kept too.

ox-llm is one release with three parts. Each part is its own build unit:

| Directory | Part | Language / build | What it is |
|---|---|---|---|
| [`engine/`](engine/README.md) | **oxe** | Rust, its own cargo workspace | Inference engines with an OpenAI-compatible HTTP API: `oxe-server` (35B mixture-of-experts models such as Ornith and Qwen3.6), Flash (a 125B MoE whose experts stream from the SSD) and the experimental `oxi` image engine (FLUX.2 klein) |
| [`lab/`](lab/README.md) | **oxc** | Python package | The controller: one loopback OpenAI-compatible endpoint, model loading and unloading, memory admission, a request ledger. It also holds the `local-ai` CLI, the Omarchy panel widget, Studio (images), the installer, the benchmark and qualification tools, the receipts and the docs |
| [`oxa/`](oxa/README.md) | **oxa** | Rust, its own cargo workspace | The coding agent: a Rust port of the pi coding agent (MIT, Mario Zechner), tuned for this engine. It is the coding client that `oxc code` and the widget's code button start |

## How they fit

```
  oxa (oxc code, widget code button)   other OpenAI-compatible clients (curl, ...)
                    \                          /
                     v                        v
     oxc controller (loopback 127.0.0.1:8092, OpenAI-compatible; admission, queue, ledger)
                                  |
                                  v
     oxe-server (MoE) | oxe-server --engine flash (125B) | oxi (images, via Studio)
```

The engine and the controller are **client-neutral**: they make no client-specific choices, pin no
client version and use no per-client byte templates. Caching is generic (exact prefix reuse,
token-exact echo, state snapshots as a mechanism). The policy for when to compact, checkpoint or reshape
history lives in the client. oxa is the client that is tuned for this engine. See
[ARCHITECTURE.md](ARCHITECTURE.md) and [DECISIONS.md](DECISIONS.md).

## Quick start

- **Install:** [lab/README.md](lab/README.md#quick-start) covers the engine tarball, its verification
  and the controller. Bootstrap: [lab/docs/BOOTSTRAP-INSTALL.md](lab/docs/BOOTSTRAP-INSTALL.md).
- **Cloud only, no local engine:** [lab/docs/CLOUD-ONLY-QUICKSTART.md](lab/docs/CLOUD-ONLY-QUICKSTART.md)
  (any Linux, x86_64 or arm64; cloud models through oxc, plus oxa).
- **Code:** run `oxc code` in a project directory, or press the code button in the widget. Both
  start oxa against the controller.
- **Other clients:** any OpenAI-compatible client can use `http://127.0.0.1:8092/v1`. See
  [lab/docs/OTHER-CLIENTS.md](lab/docs/OTHER-CLIENTS.md).
- **Reproduce:** [lab/docs/REPRODUCE.md](lab/docs/REPRODUCE.md).
- **Build the engine:** `cd engine && cargo build --release --locked -p server --bin oxe-server`
  ([engine/README.md](engine/README.md)).
- **Build oxa:** `cd oxa && cargo build --release --locked` ([oxa/README.md](oxa/README.md)).
- **Run the lab tests:** `cd lab && ./tools/run_lab_tests.sh`.

## The coding agent: oxa

oxa is the default coding client. `oxc code` and the widget's code button start it with
`OXA_ENGINE_URL` set to the controller (`http://127.0.0.1:8092`). Started by hand, oxa reads its
endpoint from `OXA_ENGINE_URL` or from `~/.oxa/agent/models.json`.

oxa's bash tool runs inside a bubblewrap (`bwrap`) sandbox; without `bwrap` the bash tool refuses
to run. `oxc doctor` checks that `bwrap` is installed.

### Migrating from OpenCode

Earlier releases launched OpenCode. ox-llm v0.2.0 removes that launcher: the code button and
`oxc code` start oxa only.

- **Your own OpenCode install and config are never touched.** ox-llm does not delete or rewrite
  `~/.config/opencode` or OpenCode itself. OpenCode sessions are not imported.
- **Config an earlier release wrote for you** (for example
  `~/.config/opencode/local-ai-bootstrap.fragment.json`, or the local-ai entries an earlier install
  pinned in the global `opencode.json`) is left in place; the upgrade prints a one-time note about it.
  Remove those entries yourself if you no longer use OpenCode with ox-llm.
- **To keep using OpenCode** (or pi, or another client) against ox-llm, point it at the controller
  yourself: [lab/docs/OTHER-CLIENTS.md](lab/docs/OTHER-CLIENTS.md).

## Flash: the expert sidecar on an unencrypted partition

For reproduction, the Flash expert sidecar (public model weights only) may live on a plain ext4
partition such as `/var/lib/local-ai-models/flash/`. SSD read latency is part of every decoded Flash
token, and disk encryption adds to it. Prompts, sessions, KV and state caches, request ledgers, logs and
receipts stay on the encrypted disk. Flash also works with the sidecar on an encrypted volume, more
slowly. Setup, measurements and the fallback: [lab/docs/FLASH-STORAGE.md](lab/docs/FLASH-STORAGE.md).

## NPU (Hexagon): bring your own SDK

The NPU (Hexagon) paths need Qualcomm's Hexagon SDK / QAIRT. You obtain it from Qualcomm and accept
its licence yourself. No SDK files, headers, skels or objects generated from them are in this
repository or in its releases. The CPU paths, which all published numbers use, work without it.

## Updates and uninstall

- `oxc update` applies a release that you have already staged on this machine. It does not
  download anything.
- `oxc uninstall` is a dry run by default: it lists what it would remove. Add `--apply` to act.
  It removes only files an install record lists (exact path and the sha256 ox-llm installed); anything
  else, or anything you changed, is reported and kept. It stops the services first and stops without
  changing anything if it cannot prove they stopped. It keeps model weights unless you add
  `--remove-models`, which deletes only the exact model files the config names.

## Repository map

- [ARCHITECTURE.md](ARCHITECTURE.md): the components, the engine contract and the build units.
- [DECISIONS.md](DECISIONS.md): the decision record (ADR-style).
- [KNOWN-ISSUES.md](KNOWN-ISSUES.md), [ROADMAP.md](ROADMAP.md), [CHANGELOG.md](CHANGELOG.md).
- [CONTRIBUTING.md](CONTRIBUTING.md): measurement rules, privacy, tests and CI.
- Results and method: [lab/docs/RESULTS.md](lab/docs/RESULTS.md). What did not work:
  [lab/docs/FAILED-PATHS.md](lab/docs/FAILED-PATHS.md).

## Licence

Our code is MIT ([LICENSE](LICENSE)). Vendored and ported code keeps its own licence. oxa ports the pi
coding agent (MIT) and statically links QuickJS (MIT); both licence texts are in [NOTICE](NOTICE). Each
part lists its components in its own `THIRD_PARTY_NOTICES`; the root [NOTICE](NOTICE) and
[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) index them. There are no model weights and no vendor SDK,
firmware or runtime files in this repository.

Snapdragon and Hexagon are trademarks of Qualcomm Incorporated. This is an independent project, not
affiliated with, sponsored by, or endorsed by Qualcomm.
