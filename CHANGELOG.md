# Changelog

Release notes per version. Each public release is one squashed commit; its message carries the notes
and the receipt ids (`oxc/benchmarks/results/public/<id>.json`).

## v0.2.0 (unreleased)

Combined release under one name, **ox-llm**: the engine oxe (`engine/`), the controller and tooling
oxc (`oxc/`; the controller and CLI, formerly oxc) and the coding agent oxa (`oxa/`) ship together with one tag. Previous releases came
from two repositories.

- **oxa: user-only settings hold.** Keys marked user-only (`oxa.sandbox.net`, `oxa.offline`,
  `oxa.allowHomeCwd`, `defaultProjectTrust` and the rest) are dropped from a project settings file with a
  warning, and `Settings::set` for the project file refuses them. A trusted project can no longer
  widen the sandbox or network (yolo power plan, S8).
- **oxa is the coding client.** `oxc code` and the widget's code button start oxa (a Rust port of
  the pi coding agent, tuned for the engine). The OpenCode launcher, the OpenCode config that the
  bootstrap and installer generated and the OpenCode wording in the widget are removed. Your own
  OpenCode install and config are never touched; config an earlier release wrote gets a one-time
  note. Other clients: `oxc/docs/OTHER-CLIENTS.md`.
- **Releases and operations:** the oxa binary is part of install, update and rollback, the health
  digest reports the oxa version, and the OEM drills cover it. Install and update run
  `oxa identity --refresh` (best effort; `--offline` when the controller is not up). The oxa release asset ships with a
  sha256 and a build attestation. The public export now includes `oxa/`. New `oxc uninstall`
  (dry run by default, `--apply` acts; removes only recorded files with their installed sha256,
  only after a proven service stop; keeps model weights unless `--remove-models`). `oxc doctor`
  checks for `bwrap`, which sandboxes oxa's bash tool. `oxc update` applies locally staged releases
  only; it does not download.
- **Engine:** Ornith's production profile uses the `pfg` f16 prefill attention (`--pf-attn pfg`); Qwen
  stays on `tiled` (`auto` would pick fa2 above 131K). Also an opt-in bit-identical decode `V^T.p`
  kernel (`--attn-out pb`), user-message checkpoints (`--user-snapshots`, default off) and the
  `isa-roof` model-free ISA roofline probe. The fix batch includes echo, state-store and preload
  robustness.
- **Lab:** a lab fix batch, the panel redesign, a client-free user-snapshots gate and a query string on
  the Rust Flash inference routes refused (403, consistent with the legacy route).
- **Removed dead code:** `flash-iobench`, prompt-lookup drafting (`--spec-type lookup`, `--lookup-*`),
  the no-op `--compaction-cut` flag and the explicit-routing module (`OXC_ENABLE_ROUTING`,
  `local-auto`). `oxc plan` opens oxa on Flash with read-only tools.
- **Fix: oxa's prefix preload on Ornith / Qwen3.5.** The boot preload (system + tools, no user turn,
  `max_tokens: 0`) got HTTP 400 `No user query found in messages.` from these templates. The engine
  now renders such a prefill as the exact prefix of the first real request (cut before its user
  turn), so the first prompt starts warm.
- **Licences:** the root NOTICE and `oxa/NOTICE` carry the pi (MIT) and QuickJS (MIT) licence texts.
- Release tooling: one exporter for the whole repository. Private CI is self-hosted only. Version
  0.2.0 everywhere.
- OpenAI v1 fit: a request that ends with an assistant message now gets a new assistant turn (it was a
  400); only an explicit in-place continuation (`add_generation_prompt: false`) is still refused. The
  SSE "queued" comment has one format for the engine and the controller queue:
  `: queued [pos=<n>] wait_s=<s>`.
- Controller: the admission queue (early SSE headers, round-robin lanes, background pre-emption) and
  the replay of a cut stream are on by default; `OXC_QUEUE=0` / `OXC_REPLAY=0` turn them off.
- Engine OpenAI v1 conformance: a default output limit for requests that name none
  (`--default-max-tokens`, default min(16384, ctx/4), published in `/v1/models` and `/props`);
  `GET /v1/models/{id}`; the OpenAI error object (string `code`, `param`; overflow =
  `context_length_exceeded`); image parts are a clear 400 on the text-only server;
  `usage.completion_tokens_details.reasoning_tokens`; `presence_penalty`, `frequency_penalty` and
  `logit_bias` in the sampler (bit-identical output when absent).
- Engine: `--spec-type draft-mtp` on a model without an MTP block no longer stops the server: it warns,
  runs without speculative decoding and reports it in `/props.speculative` (and the controller's
  capabilities).
- Engine: a GGUF whose MoE routes every token to all experts (`expert_used_count == expert_count`)
  runs (it panicked); `expert_used_count > expert_count` is refused at load.

## v0.1.3

Correctness and agentic release. The engine sends an SSE keepalive during long appends, and the
timeouts are 3600 s (fixes the v0.1.2 deep-mode known issue). The controller and Studio are split into
smaller modules. Proxy security fixes. LAN share and a tool-output cap are available, both off by
default. Adds the quality runner. No speed claim and no default change.

## v0.1.2

Chat and agentic release. 128K window. Fast mode is the default (compaction near 49.5K tokens) and
deep mode is selectable. A durable state cache (64 GB default budget, 7-day TTL) lets sessions resume
after a restart or a model switch. History rendering keeps the prefix cache valid.

## v0.1.1

Security hotfix: engine ports are internal behind per-launch tokens, and each route has one canonical
path. Adds opt-in Flash cache and prefix reuse, and release tooling.

## v0.1.0

First public release: the controller (one loopback OpenAI-compatible endpoint for Ornith, Qwen3.6 and
the 125B Flash model), harness configs, Image Studio (FLUX.2 klein 512 px), the MEMLAT device-tree
overlay, benchmarks, quality gates and receipts.
