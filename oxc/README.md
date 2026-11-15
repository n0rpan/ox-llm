# Snapdragon Local AI Lab

Large language models and image generation running locally on a **Snapdragon X Elite laptop under Linux**, on the CPU, with the receipts linked beside each speed number.

This directory is the `oxc/` part of ox-llm. It holds the controller, the OpenAI-compatible API, the `oxc` CLI and widget, the benchmark and qualification tools, the measurements and the failed attempts. The inference engines are in [`engine/`](../engine/README.md): `oxe` for 35B mixture-of-experts models, a streaming engine for a 125B MoE that does not fit in RAM, and the experimental `oxi` image engine. The coding agent oxa is in [`oxa/`](../oxa/README.md).

One laptop (Lenovo Yoga Slim 7x, X1E80100, 32 GB), one firmware. Read the numbers as "what this machine did and how to check it", not as a ranking of chips.

## Stock day one → installed today

Stock means the software and device tree we started with, on the same laptop. Today means the installed stack. Each cell links its receipt in `benchmarks/results/public/`. Where the two sides differ in metric or context, the cell says so. Rows are separate tracks; do not multiply them.

| Workload | Stock (day one) | Installed today | Same conditions? | Receipt |
|---|---|---|---|---|
| CPU memory reads (2 GiB, 6 threads) | 48.88 GB/s | **115.85 GB/s** (2.37×), best recorded median | yes, same binary, median of 3 | [RAM-MEMLAT](benchmarks/results/public/RAM-MEMLAT.json) |
| Flash 125B MoE decode, experts streamed from SSD | 356.49 ms/token (2.8 tok/s), 12 GiB expert cache | **81.13 ms/token** (12.3 tok/s), 14 GiB cache, measured before the 18 GiB prefix-store install | no: cache size differs. Same metric at 16 GiB: 84.90 ms/token (4.2×) | [G3](benchmarks/results/public/G3.json) · [063](benchmarks/results/public/063.json) · [G3-prime](benchmarks/results/public/G3-prime.json) |
| Image 512 px, FLUX.2 klein Q8, whole request in Studio | 77.85 s: stable-diffusion.cpp | **51.53 s** matched A/B (−34%); **24.89 s** on the installed VAE fix (one live smoke) | yes for 51.53 s; the 24.89 s smoke is not a matched comparison | [IMG-SHIP-1](benchmarks/results/public/IMG-SHIP-1.json) · [VAE-1](benchmarks/results/public/VAE-1.json) |

**Ornith 35B-A3B decode** has no like-for-like day-one receipt yet, so it is not in the table. The one matched pair that is public is memlat off → on, same harness: 12.73 → 17.18 tok/s (+35%, receipt [docs/MEMLAT.md:11](docs/MEMLAT.md#current-evidence)). The installed `oxe` figure of 54.4 tok/s is the mean of two VL15 runs with a 53-token prompt ([G1-prime](benchmarks/results/public/G1-prime.json)); its 12-run median is 42.8 tok/s. It was measured under different conditions from that pair; the two do not combine into a speedup, and it is not an isolated engine speedup.

Conditions: CPU only; no Adreno GPU or NPU is used in any number. The MEMLAT overlay applies to the installed cells only, not to the stock column. The full table, with prompt lengths, cache states and the rows that do not match, is in [docs/RESULTS.md](docs/RESULTS.md). The power source was not recorded for these runs.

## The memory discovery

The memory-latency driver (memlat) is built into the `linux-omarchy` kernel. But the Yoga's device tree did not include the SCMI `protocol@80` node, so the firmware's memory-latency and bandwidth scaling was never enabled. CPU memory reads stayed capped at about **49 GB/s**.

A one-node device-tree overlay adds that node. Installing it changes the default entry in `/boot/limine.conf` to the test entry; the stock entry is kept. With it, the same 2 GiB read benchmark reaches a median of **115.85 GB/s**, 2.37× the stock number ([RAM-MEMLAT](benchmarks/results/public/RAM-MEMLAT.json); method and boot setup in [docs/MEMLAT.md](docs/MEMLAT.md)). After two deep suspend and resume cycles the same benchmark measured 108.63 GB/s, so 115.85 GB/s is the best recorded median, not a typical one. Decode benefits directly, because it is bandwidth-bound: +35% in the matched pair above.

Be clear about the limits:

- **It is firmware- and laptop-specific.** Read [docs/MEMLAT.md](docs/MEMLAT.md) before trying it on any other machine.
- **Stability under GPU load is not settled** and has no public receipt yet. Upstream discussion and platform tracking: [Omarchy issue #18254](https://github.com/omacom/omarchy/issues/18254).
- **The stock boot entry remains the fallback.**

## Quick start

Chat models run through the engine binaries. The lab has the controller and Studio. The coding agent is oxa.

**1. Get the engine.** Download the attested Linux aarch64 tarball from the [engine releases](https://github.com/n0rpan/ox-llm/releases): `oxe-server-vX.Y.Z-linux-aarch64.tar.gz` for Ornith and Qwen, or the Flash tarball for the 125B model. The numbers here were measured on the Snapdragon X Elite (X1E80100) only; the X Plus has not been tested. The packaged `README.txt` says cores without i8mm or bf16 stop with "Illegal instruction"; that has not been tested on such a core.

**2. Verify it.** In the download directory:

```sh
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify oxe-server-vX.Y.Z-linux-aarch64.tar.gz --repo n0rpan/ox-llm \
  --signer-workflow n0rpan/ox-llm/.github/workflows/release.yml
```

The first checks integrity. The second checks the Sigstore-signed build provenance, and needs GitHub CLI 2.49 or newer.

**3. Get the weights and run.** The pinned revision and SHA-256 of every model are in [`dependencies/model-artifacts.json`](dependencies/model-artifacts.json). For the default model:

```sh
curl -LO https://huggingface.co/mudler/Ornith-1.5-35B-A3B-APEX-MTP-GGUF/resolve/4c326713c52d13fdb40983a2a09a46cca8e88739/Ornith-1.5-35B-A3B-APEX-MTP-Compact.gguf   # 17.4 GB
echo "a7373f724cf3693e8a6a5f8209ae9a06ff8071ae2af60659bdbdf8ff6f6c10df  Ornith-1.5-35B-A3B-APEX-MTP-Compact.gguf" | sha256sum -c
```

Then unpack and start the server:

```sh
tar xzf oxe-server-vX.Y.Z-linux-aarch64.tar.gz
cd oxe-server-vX.Y.Z-linux-aarch64
./bin/oxe-server --engine oxe --model ../Ornith-1.5-35B-A3B-APEX-MTP-Compact.gguf \
  --host 127.0.0.1 --port 8080 --ctx-size 32768 --oxe-cpus 0-7 --oxe-prefill-cpus 0-11 \
  --spec-type draft-mtp --spec-draft-n-max 1
curl http://127.0.0.1:8080/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"messages": [{"role": "user", "content": "Hello"}]}'
```

The CPU lists assume the X1E80100 core layout. The server listens on loopback only.

**4. Add the controller (optional).** To serve every model on one OpenAI-compatible port, build the controller from this repository and follow [docs/REPRODUCE.md](docs/REPRODUCE.md). There is no one-command installer yet.

Model-free checks run anywhere with Python 3:

```sh
PYTHONPATH=src python3 -B -m oxc doctor
PYTHONPATH=src python3 -B -m unittest discover -s tests
```

## Supported models

| Model | Role | Engine | Context |
|---|---|---|---|
| Ornith-1.5-35B-A3B, APEX-MTP Compact GGUF | everyday default | `oxe` | 128K window |
| Qwen3.6-35B-A3B MTP, UD-Q4_K_M | selectable | `oxe` | 128K window |
| Qwen3.8-Flash-Next, UD-IQ3_XXS (83.6 GB) | "think hard" model | Flash | 16K |
| FLUX.2 klein 4B Q8 | images | `oxi` (experimental, opt-in, 512 px fast path) or patched stable-diffusion.cpp (other sizes) | 512 px |

**Coding.** `oxc code` starts the coding agent oxa in `~/Work` (override with `OXC_CODE_DIRECTORY`), pointed at the controller with `OXA_ENGINE_URL=http://127.0.0.1:8092`. `oxc plan` starts oxa on Flash with read-only tools (read, grep, find, ls). The widget's **Code** button does the same as `oxc code`. When to compact a session is oxa's decision, not the controller's. Other clients: [docs/OTHER-CLIENTS.md](docs/OTHER-CLIENTS.md).

**History (v0.1.x, when the launcher started OpenCode).** v0.1.3 fixed the deep-mode long-append timeout: the engine sends an SSE keepalive comment while it reads a long append, and the controller waits up to 3600 s. Qualification through OpenCode 2.2.7: a 28,420-token append at 111,025 tokens of context took 1,080.8 s to read, with 72 keepalives, a longest gap of 15.0 s and no client retries. Receipt: [V013-INSTALL-G1](benchmarks/results/public/V013-INSTALL-G1.json). OpenCode 2.2.7 could also fold a prompt submitted above its compaction trigger into the compacted history, so the model saw no live user turn; if you use OpenCode yourself, send the request again after the compaction finishes.

Pinned revisions and SHA-256 of every file: [`dependencies/model-artifacts.json`](dependencies/model-artifacts.json). No weights are in this repository, and each model keeps its publisher's licence.

## Why purpose-built

Generic runtimes are written for every model on every machine. This project asks a narrower question: what does this one chip do for these models?

- **We measured the hardware first.** Memory bandwidth, thread placement and the cost of each graph step decided the design. Then we built engines for exactly the two model families this laptop runs, instead of one graph for everything.
- **Every speedup is gated on quality.** Each change declares its class before it is measured. *Lossless* changes must be bit-identical to the reference. *Reorder* changes must stay within twice the reference's own batch noise, measured as mean KLD. *Lossy* changes are opt-in only. Output identity is a bug detector, not the whole gate.

Honest limits:

- **Decode is memory-bandwidth-bound.** Each generated token streams the active weights from DRAM. A faster kernel cannot beat that ceiling. This is why the memory fix above is the largest bandwidth gain measured (2.37×).
- **Marketing TOPS are not usable throughput.** Peak figures on a spec sheet are not what a decode step can use. The memory system and per-step overheads set the real limit.
- **Some rows are not like-for-like.** The table says which ones. Do not read a gain from a row marked "no" as an isolated engine speedup.
- **Sustained load is hot.** Long runs sit at about 97.7–98.0 °C and throttle. A long-session thermal test has not been done.

## How the parts fit together

| | `oxc/` (oxc) | [`engine/`](../engine/README.md) (oxe) | [`oxa/`](../oxa/README.md) (oxa) |
|---|---|---|---|
| Role | Controller, CLI, widget, Studio, benchmarks, receipts, notes | The Rust inference engines | The coding agent |
| Contains | OpenAI-compatible API on port 8092, model loading, memory guards, qualification tools | `oxe` (35B MoE), Flash (SSD-streamed 125B MoE), `oxi` (experimental image engine) | A Rust port of the pi coding agent, tuned for the engine |
| Start here for | Using the models, reproducing numbers, the history | Building the engines, the engine internals | Coding with the local models |

The engine has its own correctness rules. The lab turns the engines into a service, and its receipts back every number in the engine and lab READMEs.

Finished experiments, superseded setups and milestone history are indexed in [docs/lab-notes/INDEX.md](docs/lab-notes/INDEX.md).

## Help wanted

We want help on concrete, scoped problems, each with context and acceptance criteria in [docs/HELP-WANTED.md](docs/HELP-WANTED.md):

- numbers from other Snapdragon X laptops;
- a matched Mac comparison, using `tools/mac_reproduce.py`;
- Flash expert prediction at small caches;
- a shared-expert verify step for MoE speculation;
- image int8 attention calibration;
- fuzzing the tool-call parser.

Read [CONTRIBUTING.md](CONTRIBUTING.md) first. Bug reports with a model, a command and a receipt are the most useful kind.

## Licence

MIT; see [LICENSE](LICENSE).

Copyright (c) 2026 Oscar Nordström (n0rpan)

Vendored and patched third-party code keeps the licence of its source; see [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES). Model weights, engine binaries and vendor runtimes are not included and have their own terms.

Snapdragon and Hexagon are trademarks of Qualcomm Incorporated. This is an independent project, not affiliated with, sponsored by, or endorsed by Qualcomm.
