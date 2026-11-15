# GPT author and native Anthropic reviewer in the same parent session

This model-free demo exercises #94's project workflow through the actual TUI,
native worker processes, A2A cleanup verification and cloud gateway. It builds on
the pinned cloud integration in [CLOUD_GATEWAY_DEMO.md](CLOUD_GATEWAY_DEMO.md).
The author changes a private file snapshot; the parent retrieves the completed
worker's actual review bundle and delegates that exact bundle to a separate
read-only Sonnet reviewer. No proposed changes apply to the source checkout.

## Run

Use x86_64 Linux with working bubblewrap/user namespaces and tmux. Build the
feature binary and install the repository-pinned optional cloud SDK in a private
virtual environment (not into the system Python):

```sh
cd oxa
cargo build -p oxa-cli --features orchestrator -p oxa-fake
python3 -m venv /tmp/oxa-review-sdk
/tmp/oxa-review-sdk/bin/python -m pip install 'anthropic==0.125.0'
/tmp/oxa-review-sdk/bin/python -I oxa-orchestrator/tools/review_lane_demo.py
```

The SDK version matches `lab/pyproject.toml` at gateway revision `d5f0ff8`.
`--gateway-root` and `--bin-dir` can select other explicit checkouts/builds.
The opt-in [session accounting and pricing variant](SESSION_ACCOUNTING.md) also
checks separate session ledgers when parent and author use the same model alias,
including prompt tiers and native cache-write counts from the #162 gateway.
No provider key is needed. Before gateway or SDK imports, the script re-executes
with a cleared environment and private temporary HOME/XDG directories. It creates
one synthetic 0600 capability file outside the worker workspace. Both the shared
OpenAI connection path and the native Anthropic SDK factory check the exact owned
literal-loopback upstream port and synthetic key before opening a connection.
No installed service, actual model or production configuration is used. Shell
network access, ambient context, extensions, MCP, skills and web search are off.
The real TUI and workers also receive cleared environments.

## What the run proves

1. An external client funds a goal in the actual TUI session. That parent uses
   the real `worker_lane` tool to launch a fixed-role GPT author with one editing
   file grant and its own immutable budget.
2. The author invokes the actual `write` tool in its private snapshot, then
   completes. A barrier ensures the parent first waits for the result. A2A result
   verification and native cleanup complete before the parent retrieves `status`.
3. The parent obtains the collected `ReviewBundle`, checks its file path,
   before/after contents and SHA-256 values, then starts the fixed-role Sonnet
   reviewer with that bundle in its goal. The native Messages mock decodes the
   actual request and compares the entire received bundle with the author's
   collected bundle. It has no file or shell tools. The environment note appended
   after the delegated JSON is parsed separately from that JSON envelope.
4. The reviewer completes through the official Anthropic SDK and gateway
   translation. Its result wakes the same parent, which retrieves the verified
   stopped report before declaring DONE. Both cloud workers appear in the TUI and
   are reaped. The source file remains its original contents throughout.
5. Separate parent/author/reviewer usage reconciles with ten settled gateway
   rows. Seven parent Responses requests cost 140 synthetic tokens, two author
   requests cost 40, and one Messages request costs 23 (12 uncached input + 3 cache
   reads + 8 output). The reviewer ledger separately reports the three cached
   tokens. The parent keeps its original session and goal allowance; Ctrl-D exits 0.

The temporary artifact directory contains `result.json`, `status.json`, the
synthetic `proposal.json`, ledger rows and working/completed terminal frames.
Failures preserve status, a terminal frame and synthetic provider requests.
All owned mock servers, worker supervisors and the private tmux server are
cleaned up. No full transcript or patch fixture is committed.

This tests transport, permissions, proposal handoff, result events and accounting.
The replies are deterministic mocks; it does not establish actual model judgment
or count as the required independent coordinator/T2/T3 review. It does not stage,
apply, commit or merge the proposed patch, open real PRs or enter a merge queue.
Actual provider billing, quiet-stream cancellation, authenticated GitHub operations
and #94's full working evening remain acceptance gates. Paid cloud use still
requires Oscar's go.
