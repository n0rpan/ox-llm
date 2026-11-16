# ox-llm

Local large language models, a coding agent and image generation on **Snapdragon X laptops under
Linux** (Omarchy), on the CPU. Local inference runs on the machine. Every speed number links its receipt.
Failed attempts are kept too.

The v0.2.1 runtime installs `oxa` and `oxe-server` only. The repository also
retains the v0.2.0 controller and experimental engines for legacy use and reproduction:

| Directory | Part | Language / build | What it is |
|---|---|---|---|
| [`engine/`](engine/README.md) | **oxe** | Rust, its own cargo workspace | Inference engines with an OpenAI-compatible HTTP API: `oxe-server` (35B mixture-of-experts models such as Ornith and Qwen3.6), Flash (a 125B MoE whose experts stream from the SSD) and the experimental `oxi` image engine (FLUX.2 klein) |
| [`oxc/`](oxc/README.md) | **oxc** | Python package | Legacy v0.2.0 controller, panel, Studio and build/qualification tools; not shipped in the v0.2.1 runtime |
| [`oxa/`](oxa/README.md) | **oxa** | Rust, its own cargo workspace | The coding agent: a Rust port of the pi coding agent (MIT, Mario Zechner), tuned for this engine. Run `oxa` directly; it owns native cloud routing and the local engine child |

## How they fit

```
  oxa (Rust CLI/TUI)
    +-- in-process gateway --> configured cloud provider
    +-- owned loopback child --> oxe-server
```

The native engine is **client-neutral**: they make no client-specific choices, pin no
client version and use no per-client byte templates. Caching is generic (exact prefix reuse,
token-exact echo, state snapshots as a mechanism). The policy for when to compact, checkpoint or reshape
history lives in the client. oxa is the client that is tuned for this engine. See
[ARCHITECTURE.md](ARCHITECTURE.md) and [DECISIONS.md](DECISIONS.md).

## Quick start

- **Start here:** [native installation](docs/NATIVE-INSTALL.md) describes the Rust-only
  default (`oxa` plus the native engine), package verification and the activation gate.
  The Python bootstrap belongs to the legacy v0.2.0 installation.
- **Prebuilt, any Linux:** each release attaches `ox-llm-<version>-linux-aarch64.tar.gz` and
  `ox-llm-<version>-linux-x86_64.tar.gz` (oxa, oxe-server, licences and upgrade note) with SBOMs, `SHA256SUMS` and build provenance.
- **Cloud only:** configure the native user `oxa.cloud` catalog and run `oxa` directly;
  see [native installation](docs/NATIVE-INSTALL.md). No Python controller service is installed.
- **Code:** run `oxa` in a project directory.
- **Panel, LAN share and Studio:** not included in the Rust-only default install.
- **Legacy v0.2.0 clients:** the old controller setup is documented in
  [OTHER-CLIENTS](oxc/docs/OTHER-CLIENTS.md); it is not the new default install.
- **Reproduce:** [oxc/docs/REPRODUCE.md](oxc/docs/REPRODUCE.md).
- **Build the engine:** `cd engine && cargo build --release --locked -p server --bin oxe-server`
  ([engine/README.md](engine/README.md)).
- **Build oxa:** `cd oxa && cargo build --release --locked` ([oxa/README.md](oxa/README.md)).
- **Run the lab tests:** `cd oxc && ./tools/run_lab_tests.sh`.

## The coding agent: oxa

oxa is the default coding client. The native default uses the in-process gateway and
native local engine path. The legacy `oxc` launchers are not installed. Explicit HTTP
endpoint configuration remains a separate client option, not a Python install step.

oxa's bash tool runs inside a bubblewrap (`bwrap`) sandbox; without `bwrap` the bash tool refuses
to run. The legacy source diagnostics remain in the repository, outside the default install.

## Flash: the expert sidecar on an unencrypted partition

For reproduction, the Flash expert sidecar (public model weights only) may live on a plain ext4
partition such as `/var/lib/local-ai-models/flash/`. SSD read latency is part of every decoded Flash
token, and disk encryption adds to it. Prompts, sessions, KV and state caches, request ledgers, logs and
receipts stay on the encrypted disk. Flash also works with the sidecar on an encrypted volume, more
slowly. Setup, measurements and the fallback: [oxc/docs/FLASH-STORAGE.md](oxc/docs/FLASH-STORAGE.md).

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
- Results and method: [oxc/docs/RESULTS.md](oxc/docs/RESULTS.md). What did not work:
  [oxc/docs/FAILED-PATHS.md](oxc/docs/FAILED-PATHS.md).

## Licence

Our code is MIT ([LICENSE](LICENSE)). Vendored and ported code keeps its own licence. oxa ports the pi
coding agent (MIT) and statically links QuickJS (MIT); both licence texts are in [NOTICE](NOTICE). Each
part lists its components in its own `THIRD_PARTY_NOTICES`; the root [NOTICE](NOTICE) and
[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) index them. There are no model weights and no vendor SDK,
firmware or runtime files in this repository.

Snapdragon and Hexagon are trademarks of Qualcomm Incorporated. This is an independent project, not
affiliated with, sponsored by, or endorsed by Qualcomm.
