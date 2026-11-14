# Tiny-real test model (gate T1.5)

A real, small model in oxe's own architecture, for a 2-minute hardware gate between the model-free
checks (T1) and the per-model numerics (T2). No engine change: the model file is rewritten.

## Model

| | |
|---|---|
| Source | `unsloth/Qwen3.5-2B-MTP-GGUF`, file `Qwen3.5-2B-Q4_K_M.gguf`, revision `e05864f8066d874d5f85aaff007ae57a2a7d1efe` |
| Licence | Apache-2.0 (Qwen3.5; the repo's licence field is apache-2.0 too) |
| Source sha256 | `18c92228fc1498a02506f64593f7cc8cc02f406f732a217c6ba99ad1254d4f0b` (1.27 GB) |
| Rewrite | `Qwen3.5-2B-Q4_K_M-qwen35moe-8of16.gguf`, sha256 `5e56dc636d7e3af306c5c04d3ac4be7460e6f1ad3d88f44bf3584f38e698e745` (2.13 GB; converter v1, defaults) |
| Location | `/var/lib/local-ai-models/tiny/` (source in `src/`), or `~/.local/share/local-ai-models/tiny/`. Never in git. |

Qwen3.5-2B is the same family as the production MoE model: 24 layers (18 gated delta-net, 6 gated
attention, interval 4), hidden 2048, 8 Q / 2 KV heads of 256, GDN 16 x 128, IMRoPE [11, 11, 10],
the same 248320-token vocabulary and `qwen35` pre-tokenizer, one MTP block. It differs in a dense
FFN (6144) and tied embeddings. Per-tensor types of the Q4_K_M file: Q4_K 113, Q6_K 27, Q5_K 18,
Q8_0 1 (`nextn.eh_proj`), F32 176 (norms, `ssm_*`, `ssm_alpha/beta`): all in oxe's set.

```sh
d=/var/lib/local-ai-models/tiny; mkdir -p "$d/src"
nice -n 19 ionice -c3 curl -L -o "$d/src/Qwen3.5-2B-Q4_K_M.gguf" \
  https://huggingface.co/unsloth/Qwen3.5-2B-MTP-GGUF/resolve/e05864f8066d874d5f85aaff007ae57a2a7d1efe/Qwen3.5-2B-Q4_K_M.gguf
python3 -I lab/tools/tiny_real/convert_dense_to_qwen35moe.py --inspect "$d/src/Qwen3.5-2B-Q4_K_M.gguf"
python3 -I lab/tools/tiny_real/convert_dense_to_qwen35moe.py "$d/src/Qwen3.5-2B-Q4_K_M.gguf" "$d/Qwen3.5-2B-Q4_K_M-qwen35moe-8of16.gguf"
```

## The rewrite (`lab/tools/tiny_real/convert_dense_to_qwen35moe.py`)

- Metadata `qwen35.*` -> `qwen35moe.*`; `expert_used_count` 8, `expert_count` 16,
  `expert_feed_forward_length` 512, `expert_shared_feed_forward_length` 2048 (2048 + 8 x 512 = 6144).
- Each dense FFN (24 target layers and the MTP block `blk.24`): gate/up rows [0, 2048) become the
  shared expert, rows [2048 + 512 e, +512) expert e (byte copies); down k-columns likewise, with the
  block scales of the shared part x2 and of the experts x8. Experts 8-15 are all-zero padding.
- Router `ffn_gate_inp` = zeros (F32 [2048, 16]); shared gate `ffn_gate_inp_shexp` = zeros.
- `output.weight` = byte copy of `token_embd.weight` (Q6_K): oxe has no tied-head path.

Why 16 slots for 8 experts: llama.cpp accepts `expert_count == expert_used_count` (asserts only
used <= count), but oxe's `kernels-sys` `argsort_top_k` asserts `k < n`: an "8 of 8" file panics
in the first MoE layer (found by this gate; not changed in the engine). With 16 equal logits the
top 8 are experts 0-7 in both engines (llama.cpp `ggml_argsort` and oxe's tie fallback are the
same `std::sort`, an insertion sort for n <= 16, so ties keep index order).

### Where it is exact, where it is not

| Part | Exact? | Why |
|---|---|---|
| gate / up slices | bit-exact | byte copies of whole rows |
| down slices | exact values | f16 block scales (d, dmin) x 2 or x 8: a power of two, checked for overflow; every f32 product the kernels form scales exactly |
| activation quantisation of h | bit-exact | Q8_K / Q8_0 blocks are 256 / 32 wide; all slices are block aligned |
| router weights | exact | softmax of 16 zeros = 1/16; normalised by the exact sum 8/16: 1/8 |
| shared gate | exact | sigmoid(0) = 1/2 |
| head | bit-exact | the same bytes as the tied embedding |
| summation order | **not exact** | dense: one f32 accumulation over 24 blocks; rewrite: shared + 8 experts, each accumulated alone, then added |

The summation order gives last-bit differences per FFN output (measured on layer 0: 2e-8 of 0.8).
They grow a lot downstream, because the next layer requantises its activations to int8 and a
last-bit change flips a rounding now and then (layer 6: 9e-5, layer 7 after attention: 2e-2).
That is the same mechanism as llama.cpp's own batched-vs-single noise, and the result is the same size:

| Prompt | rewrite vs dense, mean / max KLD | llama.cpp batch vs single (dense), mean / max KLD |
|---|---|---|
| code (chat) | 0.0020 / 0.022 | 0.0029 / 0.027 |
| arithmetic (chat) | 0.0018 / 0.021 | 0.0026 / 0.020 |
| completion | 0.0012 / 0.008 | 0.0014 / 0.006 |

So the rewrite equals the dense model up to llama.cpp's own noise, never bitwise. Bitwise identity
is the contract between oxe and llama.cpp on the same rewritten file: 134/134 logit rows
bit-identical, identical greedy runs, MTP depths 1-3 identical (acceptance 0.68-1.0).

Unit tests (`lab/tests/test_tiny_real_convert.py`, synthetic tensors, run in ci-lab): byte slices,
exact x2 / x8 on dequantised Q4_K / Q6_K / Q8_0 values, the rewritten FFN equal to the dense one
in float64, zero padding, a negative control (x4 instead of x8 is caught), and refusals (IQ types,
f16 overflow, unaligned splits, a non-`qwen35` source).

## Gate T1.5 (`tools/gates/tiny_real.sh`)

Needs release `oxe` + `oxe-server` from this tree and the llama.cpp oracle
(`engine/tools/oracle/build.sh`). Runs in a temporary HOME / XDG tree and port (default 18793,
8093 refused), removes everything at exit; ~100 s measured without oxa, 123 s with it, plus the weight repack on first load.

1. Identity: both sha256 pinned.
2. Rewrite vs dense in llama.cpp on 3 prompts: mean KLD <= 2x llama.cpp's own batch-vs-single noise.
3. oxe vs llama.cpp on the rewrite: every logit row bit-identical and greedy identical (teacher-forced
   `oxe verify`); `oxe mtp-check` depths 1-3 identical.
4. oxe-server: a real tool call (valid JSON), two appends with the reasoning dropped by the client
   (ECHO must restore it; C3: cached >= previous prompt + completion - 1), SIGTERM, restart on the
   same state store, the next append resumes from `disk` with a C3 hit.
5. oxa loop (`lab/tools/tiny_real/oxa_tiny_loop.py`, runs when `OXA_BIN` is set; another driver
   via `OXA_TINY_LOOP BASE_URL MODEL WORKDIR`): `oxa --mode rpc` in a scratch git repository with
   a private HOME / agent / session dir. Checks: read, edit (the file really changes), bash; a
   compaction forced by `oxa.compaction.softTokens` 3000; the first request after it reuses the
   static prefix (cached > 0); the `subagent` tool (`oxa.subagent.enabled`, a user agent `helper`)
   returns a result; `abort` after the first streamed delta settles the run as aborted and the
   next prompt works; every append within an epoch hits C3 (cached >= previous prompt +
   completion - 1) with no `cache_miss` event; no prefix rebase (prefix_epoch <= 1 + compactions).
   Without `OXA_BIN` the step prints SKIP and the verdict says `PASS (oxa SKIP)`.

   The server runs with `--prefix-cache` as production does: without it the first request after a
   compaction was cold (cached 0 of 3563), with it 2304 tokens of static prefix are reused.
   Measured on oxa 821c177: all ten oxa checks PASS; 9 appends, 8 exactly C3 and one 2 tokens
   above it (the turn after the subagent); cancel settles in 0.2 s; whole gate 123 s.

It guards the engine's non-MoE paths, MTP, the state store / ECHO and (with step 5) oxa. It does not
test sparse routing, the expert cache, production numerics, timing or memory under the real model:
those stay in T2 / T3. A diff only in the MoE / expert-cache code skips T1.5.
