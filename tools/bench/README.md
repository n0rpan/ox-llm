# Benchmark measurement path (dev-only)

Adapters that run oxa as a custom agent in FrontierHarness tasks, plus the usage parser. Python,
standard library only, outside the release.

| File | Purpose |
|---|---|
| `install-oxa.sh` | Copy or download the portable Linux x86_64 `oxa` and verify its checksum on the host |
| `oxa_bench.py` | Shared core: argv, in-container settings, run script, exit policy, collection |
| `harbor_oxa.py` | Harbor 0.22.0 agent (`--agent-import-path harbor_oxa:OxaAgent`) |
| `pier_oxa.py` | Pier 0.3.1 agent (`--agent-import-path pier_oxa:OxaAgent`) |
| `usage_details_oxa.py` | Parser for `--usage-log` (per-trial input, cached, output, cost) |
| `test_oxa_adapters.py` | Fake-engine smoke test (no network, no Docker): `python3 -I -m unittest discover -s tools/bench` |

## What an adapter does

1. **Install** (`setup`): checks the binary's sha256, uploads it to the container as
   `/usr/local/bin/oxa`, runs `oxa --version`, writes `~/.oxa/agent/settings.json` (in-process
   cloud, OpenCode `base_url` = the proxy) and `~/.config/oxc/llm/cloud-secrets.json` (mode 0600,
   the stub key `fh-stub`). The container never gets the real key; the host proxy adds it.
2. **Run**: `oxa -p --yolo --mode json --model opencode/kimi-k3 --no-context-files --no-mcp
   --no-web-search --no-session --max-time <task timeout - margin> --usage-log
   /logs/agent/oxa-usage.jsonl "<instruction>"`, stdout to `oxa.jsonl`, stderr to `oxa.stderr`, the
   exit code to `oxa.exit`. All in `/logs/agent`. Everything else is oxa's default.
3. **Collect**: files the framework did not already sync from `/logs/agent` are downloaded into the
   trial's agent log directory. The token counts of the Harbor/Pier context come from the usage log
   through `usage_details_oxa.py`. The binary's sha256 is saved as `oxa-binary.sha256`.

Exit codes: 0, 1 and 130 (done, failed after model calls, ended by `--max-time`) end the command
with 0 so the verifier scores the trial. Any other code (budget, credentials, ledger, offline,
setup refusal, crash) is passed on, so the trial shows as an error: `infra_invalid`.

## Settings

Agent kwargs (`--ak key=value` in Harbor, `--agent-kwarg` in Pier) win over the environment (`--ae` /
`--agent-env`, then the host environment).

| Kwarg | Environment | Default |
|---|---|---|
| `oxa_binary` | `OXA_BENCH_BINARY` | required: path of the verified binary |
| `oxa_sha256` | `OXA_BENCH_SHA256` | the `oxa.sha256` next to a file named `oxa`, if any |
| `proxy_url` | `OXA_BENCH_PROXY_URL` | required: `https://<proxy-host>:<port>` (or HTTP loopback) of the key-injecting proxy; the adapter refuses URLs the native transport would reject and refuses `opencode.ai` |
| `agent_timeout_sec` | `OXA_BENCH_AGENT_TIMEOUT_SEC` | 3600; set it to the task's agent limit |
| `margin_sec` | `OXA_BENCH_MARGIN_SEC` | 120; `--max-time` is the timeout minus this |
| `allow_home_cwd` | `OXA_BENCH_ALLOW_HOME_CWD` | off; sets `OXA_ALLOW_HOME_CWD=1` (record it if used) |
| `proxy_trial_prefix` | `OXA_BENCH_PROXY_TRIAL_PREFIX` | off; appends `/<trial dir name>` to the base URL so the proxy can tag usage per trial |
| model (`-m`) | `OXA_BENCH_MODEL` | `opencode/kimi-k3` |

Neither framework tells an agent the task timeout, so pass it with `agent_timeout_sec`.

## Usage

```sh
sh tools/bench/install-oxa.sh <dir-or-url-of-oxa-portable-linux-x86_64> ~/fh-internal/oxa-bin
# Terminal-Bench under Harbor
PYTHONPATH=tools/bench harbor run -d terminal-bench@2.0 -i <task> -m opencode/kimi-k3 \
  --agent-import-path harbor_oxa:OxaAgent --jobs-dir <jobs> -y \
  --ae OXA_BENCH_PROXY_URL=https://bench-proxy.example.invalid:8089 --ae OXA_BENCH_BINARY=$HOME/fh-internal/oxa-bin/oxa \
  --ak agent_timeout_sec=<task agent timeout>
# DeepSWE under Pier
PYTHONPATH=tools/bench pier run -p <deep-swe>/tasks/<task> --model opencode/kimi-k3 \
  --agent-import-path pier_oxa:OxaAgent --jobs-dir <jobs> \
  --agent-env OXA_BENCH_PROXY_URL=https://bench-proxy.example.invalid:8089 --agent-env OXA_BENCH_BINARY=$HOME/fh-internal/oxa-bin/oxa \
  --agent-kwarg agent_timeout_sec=3600
```

Per trial, summarize the log with `python3 tools/bench/usage_details_oxa.py <trial>/agent/oxa-usage.jsonl`.

## Limits of the test

The smoke test replaces Harbor, Pier, the container and oxa with stubs. It proves the command
construction, the file layout, the exit policy and the collection, not the frameworks' real
interfaces. Confirm those once against the pinned versions (`--help`, a one-task smoke) before a run.
The secrets file uses the flat form `{"opencode": "<key>"}`; check it against the host file's
structure (key names and types only, never values).
