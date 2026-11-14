# Engine selection

The Rust engines (`oxe-server`) are the default. A profile runs on Rust when it has a top-level
`rust_engine` block (`{"oxe": {...}}` for the MoE/dense profiles, `{"oxe_flash": {...}}` for Flash);
the [example MoE profile](../examples/runtime/llm-runtime.json) has one. The block applies with or
without MEMLAT. The older location, `memlat_overrides.oxe` / `memlat_overrides.oxe_flash`, is still
read; there the Rust engine applies only on a MEMLAT boot (the name is historical).

| Switch (chat unit environment) | Default | Effect |
| --- | --- | --- |
| `LOCAL_AI_ENABLE_OXE` | on | `0` runs MoE/dense profiles on the classic llama-server fallback |
| `LOCAL_AI_ENABLE_OXE_FLASH` | on | `0` runs Flash on the classic BigMoe streamed backend |
| `LOCAL_AI_IMAGE_NPU` | off | `1` enables NPU images (needs your own Hexagon SDK / QAIRT build) |
| `LOCAL_AI_IMAGE_WATCHDOG_KILL_CLIENT` | off | only the exact value `1` lets the NPU image watchdog stop a client that keeps the image slot past its hold limit; any other value or unset is off |

Unset, `1` or any other value means on; only `0`, `off`, `false` or `no` switches a Rust engine off.
The classic stack is therefore an explicit opt-in. If a Rust binary is missing, a load fails with
`The Rust engine binary is missing: <path>` and names the switch for the fallback; it never falls
back silently. At start the controller logs one line per profile (`local-ai: <alias>: engine rust`,
or `engine llama.cpp (classic fallback: ...)` with the reason), and `/_local/status` reports
`engine` and `engine_note` for the selected profile.

Defaults when the unit sets no path: `LOCAL_LLM_CONFIG` is `$XDG_CONFIG_HOME/local-ai/llm/runtime.json`
and `LOCAL_AI_LEASE` is `$XDG_STATE_HOME/local-ai/inference.lock`.

## Opt-in engine experiments

An existing `rust_engine.oxe` or `rust_engine.oxe_flash` block may contain an
`experimental` object. The historical `memlat_overrides.oxe` / `.oxe_flash`
locations accept the same object. Keep the block's existing binary identity,
model, admission and qualification fields unchanged.

| Setting | Default | Meaning |
|---|---|---|
| `fair_quantum_ms` | unset, engine off | integer 0–600000; 0 disables fair time-slicing; a positive value becomes `--fair-quantum-ms` |
| `mtp_guard` | unset, engine off | `"off"`, `"0"`, or `"RATIO[:WINDOW[:COOLDOWN]]"`, passed to `--mtp-guard` |

For an explicitly chosen experiment, merge an object such as
`"experimental": {"fair_quantum_ms": 25, "mtp_guard": "0.6"}` into the existing
engine block. These are example measurement settings, not new defaults.
Use the guard with an existing speculative-drafting configuration. An MTP ratio must be finite and in (0, 1]; window is at least 1 and cooldown is
non-negative. Omitted window/cooldown values use the engine's existing defaults
(256/512). The controller checks the native f32 ratio and 64-bit counter bounds.

Unset settings append no flags and preserve the existing command. Explicit 0/off
values append disabling flags. A structured setting cannot also name the same
flag in raw `oxe_args`; choose one configuration path. Both features remain
implemented in the engine and off by default. Hardware speed, memory and long-run
qualification remain maintainer measurements; this config plumbing establishes
no performance claim.
