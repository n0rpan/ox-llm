# Shared-session orchestrator through the actual cloud gateway

This dependency-validation branch applies the A2A stack from draft #159
(`0062e0f`) to draft #24's cloud stack. The first proof used `87c86c2`; the
current integration includes `d5f0ff8`, including upstream profile removal and
cloud accounting/admission updates. It is not a replacement
for #159's integration with main. Neither dependency is merged by this work.
The gateway/provider implementation is unchanged. The targets are #94's shared
session, worker-result wake, continuation recovery and compaction requirements.

## Run

On x86_64 Linux with Python 3.9+, tmux and working bubblewrap/user namespaces:

```sh
cd oxa
cargo build -p oxa-cli --features orchestrator -p oxa-fake
python3 -I oxa-orchestrator/tools/cloud_gateway_demo.py --case expiry
python3 -I oxa-orchestrator/tools/cloud_gateway_demo.py --case compaction
```

`--gateway-root` selects a checkout containing the cloud gateway and its test
helpers; `--bin-dir` selects the feature-built binaries. The default paths derive
from the script. The script re-executes with a cleared environment and private
temporary HOME/XDG directories before importing gateway modules. It runs the real
controller handlers and remote OpenAI Responses adapter against an owned literal
loopback mock. Its synthetic credential is stored outside the workspace with mode
0600. Provider connections are checked against the exact owned port and synthetic
credential. The real TUI and native worker also run with cleared environments.
Shell network access is disabled; SDK inference uses the owned gateway. Extensions,
MCP, skills, context files and web search are disabled. No real API key, production
service or installed binary is used.

The external client sets a budgeted goal in the actual TUI session. The model
starts a separately budgeted cloud-routed worker with a fixed role and one file
grant. The worker reads that file through its real tool. A barrier holds its final
reply until the parent is blocked waiting for workers. The harness then clears
the gateway's native Responses cache, releases the worker and requires the
verified result to wake and complete the same parent session. Tool names and
arguments arrive over native streaming events. Worker opaque reasoning is replayed
within its own continuation and never appears in the parent's recovered request.
The worker must be reaped, both ledgers must reconcile with settled gateway usage,
the terminal must show the cloud location and worker row, and Ctrl-D must exit 0.

The `expiry` case preserves closed public tool history. The `compaction` case
sets a small fixture-only soft threshold and recent-message budget using nested
settings objects. It requires two actual model summaries and retention of summary
context in the next parent request. It does not require compacted-away tool calls
to remain verbatim. The parent goal allowance and session identity stay unchanged.

## Evidence

Successful real-terminal runs with this pinned dependency produced:

| Case | Parent tokens | Worker tokens | Summary calls | Settled gateway rows |
| --- | ---: | ---: | ---: | ---: |
| Expired native cache | 60 | 40 | 0 | 5 |
| Compaction + expired cache | 100 | 40 | 2 | 7 |

Every mock inference reports 12 input and 8 output tokens. The assertions compare
the separate parent/worker goal totals with their respective gateway ledger rows,
including summary requests. This is token-accounting evidence, not proof of real
provider billing, cache pricing, canceled-stream settlement or monetary caps.

Negative checks established two dependencies:

- With the same client and mock but gateway revision `2f97b18` (before the #64
  fix), the worker finishes and is reaped, but the parent receives HTTP 409
  "OpenAI tool continuation expired; start a new turn". Only four upstream
  requests occur; there is no hidden paid retry.
- Before the feature-gated UI adaptation, the real modern HUD labels the remote
  parent `local`. Copying the model location on refresh and displaying it in the
  modern HUD makes the same terminal assertion pass. Both hooks are under the
  existing default-off `session-control` feature. Provider code is unchanged.

Temporary evidence includes `result.json`, successful socket `status.json`,
synthetic gateway ledger entries and a captured terminal frame. Failure preserves
the socket status, terminal frame and synthetic upstream requests. Full transcripts
are local artifacts, not checked-in fixtures. Owned processes stop in `finally`.

## Remaining acceptance

This is a reproducible loopback integration demo, not #94's full working evening.
Real provider cancellation/billing, #47/#48/#71 monetary accounting, authenticated
GitHub workspace operations, public CI and required coordinator/T2/T3 reviews
remain separate gates. The brief still requires Oscar's go before paid cloud use.
No merges are authorized for this worker.
