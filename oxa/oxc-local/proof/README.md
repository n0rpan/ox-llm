# Existing synthetic native-local CLI proof

This publishes the existing throwaway print/RPC/tmux helper used for the local
handoff on issue #331. Only input paths, argument selection and whitespace were
made portable. It reuses the existing engine CI GGUF generator and public TEST
metadata fixture; no second fixture or proof framework is introduced. Publication
alone is not release, hardware, real-model or owner acceptance.

Use Linux with user/PID/network/mount namespaces, bubblewrap, procfs, tmux (TUI)
and the real strace executable (print). The helper changes no system settings.
Its Python process orchestrates the test outside the traced product process tree.
Only use generated synthetic GGUF, never real weights or credential files.

## Exact fixture and commands

From a repository checkout, create the same synthetic input outside the checkout:

```sh
proof_root=$(pwd -P)
proof_tmp=$(mktemp -d /tmp/oxa-native-cli-proof-XXXXXX)
python3 -I "$proof_root/engine/tools/ci/synth_qwen36_model.py" \
  --fixture "$proof_root/engine/tests/fixtures/qwen36-tokenizer.json.gz" \
  --out "$proof_tmp/model.gguf"
cargo build --offline --manifest-path "$proof_root/engine/Cargo.toml" \
  --target-dir "$proof_tmp/engine-target" -p server --bin oxe-server
```

The generator uses deterministic filler, not trained weights. Tokenizer/template
metadata comes from the already-published Apache-2.0 fixture. The exact input
used for the original proof was 103879200 bytes, SHA256
`9781f47338c4397351ddf4043f58b839cbc6020c897cc7aea036ba52cf1b23f6`.
The engine is always launched with `--engine stub`; no inference model is loaded.

Use an optimized CLI built from the reviewed local constructor plus the published
`agent/331-local-s7-hook` composition. This proof-source branch does not itself
wire the product selector. With `proof_oxa` set to that binary, run:

```sh
proof_oxa=/absolute/path/to/oxa
proof_oxe="$proof_tmp/engine-target/debug/oxe-server"
proof_trace=$(command -v strace)
python3 -I "$proof_root/oxa/oxc-local/proof/cli_stub.py" \
  "$proof_oxa" "$proof_oxe" "$proof_tmp/model.gguf" print "$proof_trace"
python3 -I "$proof_root/oxa/oxc-local/proof/cli_stub.py" \
  "$proof_oxa" "$proof_oxe" "$proof_tmp/model.gguf" rpc
python3 -I "$proof_root/oxa/oxc-local/proof/cli_stub.py" \
  "$proof_oxa" "$proof_oxe" "$proof_tmp/model.gguf" tui
rm -rf -- "$proof_tmp"
```

Use an isolated build HOME and cached offline Cargo dependencies as in the normal
repository test workflow. The launcher itself clears the child environment and
creates its own private HOME/project; no existing settings, sign-ins or provider
keys are used. Binary/GGUF paths are explicit and rebound inside private `/tmp`,
so throwaway build directories work too. A private strace binary may be supplied
explicitly; do not substitute a shim or weaken trace assertions.

## Existing fake-kernel namespace

The helper mounts these literal TEST values in its private namespace: cgroup
membership `0::/engine`, MemAvailable 8388608 kB, SwapFree 0 kB, full PSI avg10=0,
zero cgroup OOM counters, zero host oom_kill, memory.swap.max/current=0 and
pids.events max=0. It does not configure a host cgroup or qualify a real memory
budget. The root is read-only, `/tmp` is private, and only its temporary fixture
is writable. The existing factory/queue, health/auth and memory policy still run.

| Mode | Existing observation |
| --- | --- |
| print | Actual offline CLI reply, authentication-file cleanup, execve includes CLI and owned stub, excludes Python/controller executables; marker scripts untouched |
| rpc | Actual discovery/prompt/stream/end, CLI RSS/thread samples, descendant snapshot and marker checks, authentication-file cleanup |
| tui | Optimized actual CLI in isolated tmux server, synthetic prompt and rendered stub reply, marker checks |

Only print records execve. RPC samples are observations, not peak-memory or timing
benchmarks. TUI proves rendering, not every cancellation/cleanup path. No mode
claims real-model acceptance, external-network isolation beyond its namespace,
release-artifact qualification or a substitute for the orchestrator's final read.

Admission-contract update (#780): this same fixture now supplies explicit synthetic
allocation components instead of the former opaque byte total. At4096 positions
the anonymous budget is1MiB; the production factory adds the full synthetic GGUF
file size at cold launch. These are stub test inputs, not real-model qualification.
