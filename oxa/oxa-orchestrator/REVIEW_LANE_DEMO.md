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

## Native reviewer cancellation

Run the same isolated scenario with `--cancel-reviewer` to hold the native SDK
reviewer before response headers and enable the default-off disconnect watcher.
The real TUI sends Esc, then verifies the same parent is paused with unchanged
budget/accounting, both worker processes are reaped, the provider mock observes
EOF, and the reviewer ledger row is a reserved 499 with unknown usage. The author
proposal remains collected, source files stay unchanged, no follow-on inference
occurs, and Ctrl-D exits zero. The successful review/pricing mode remains the
default. See CLOUD_CANCELLATION.md for implementation scope and limits.

Add `--tls-reviewer` to either mode to use an owned native TLS provider mock with
a generated temporary CA. The gateway retains certificate verification, the
reviewer endpoint guard requires its exact HTTPS loopback address, and the child
worker still talks only to the plain loopback gateway. Cancellation over TLS and
successful shared-model/tier-cache pricing both pass. The focused gateway tests
also reject an untrusted certificate before a provider request is sent. No trust
store or mock-only endpoint/key guard is disabled or installed globally.

## Native SDK length truncation

Use `--truncate-reviewer --pricing --shared-model`, optionally with
`--tls-reviewer`, to exercise length handling through the actual TUI, private
worker, gateway and pinned native Anthropic SDK. This remains an owned mock run,
with cleared environments, synthetic capabilities, exact loopback-port guards
and the existing verified temporary TLS CA. It never uses the shared gateway or
a real provider key. The truncation and cancellation modes are mutually exclusive.

```sh
/tmp/oxa-review-sdk/bin/python -I oxa-orchestrator/tools/review_lane_demo.py \
  --truncate-reviewer --pricing --shared-model --tls-reviewer
```

The operator fixes the reviewer's `response_tokens` at 512. The native upstream
mock requires that exact `max_tokens` value, then emits a Messages stream ending
with `stop_reason: max_tokens` and 512 synthetic output tokens. Its partial text
is deliberately a complete-looking structured DONE reply followed by whitespace:
parsing that text must not override protocol truncation.

The worker stops as `truncated` / blocked `native_response_truncated`, retains
532 observed synthetic tokens (12 uncached input, 3 cache reads, 5 cache writes,
512 output), and is reaped. The parent retrieves that verified report and remains
blocked as `reviewer_response_truncated`; no independent review is accepted as
complete. The author proposal and its exact delegated bundle still verify, and
the source remains unchanged. Neither the worker nor the parent retries or starts
follow-on inference. All ten gateway rows settle with usage under three separate
session identities, including shared parent/author model aliasing.

Tiered synthetic ledger costs remain independently checked: parent 0.000196,
author 0.000056 and reviewer 0.00521275 USD. These are fixture calculations, not
paid spend. Both plain HTTP and verified TLS truncation runs pass; the original
successful priced/shared-model review still passes with 28 reviewer tokens and
reviewer cost 0.00017275. Every actual TUI exits zero.

This extends local-engine truncation tests to the official SDK and owned async
native gateway path. It does not reproduce every aspect of the historical paid
Sonnet/Haiku smoke, prove provider tokenization/prices or cancellation billing,
or authorize another paid attempt. The mock parent/author use Responses while
the reviewer uses Messages. Bootstrap frozen-input qualification remains separate.

## Observed-threshold overrun

`--overrun-reviewer --pricing --shared-model` uses the same owned native SDK
stream with a 500-token reviewer threshold and a separate 512-token per-response
output allowance. The first request is admitted at zero observed usage; its
20 input/cache tokens plus 512 output tokens settle at 532. No speculative
prompt bound or strict output-plus-input ceiling is applied.

The worker records `budget_exhausted`, runtime `token_budget`, the full 532
observed tokens and `token_overrun: 32`. Budget exhaustion takes precedence over
length when both apply. The parent retrieves the verified report and remains
blocked as `reviewer_token_budget`. Both workers are reaped, the source stays
unchanged, all ten gateway rows settle with usage, and there is no retry or
follow-on inference. The same synthetic tier/cache cost calculation is retained;
no usage is clamped or refunded to make the outcome fit its threshold. No
independent review is counted as completed.

Cancellation, truncation and overrun modes are mutually exclusive. This is
observed-threshold acceptance against owned loopback mocks, not a paid run or
a strict pre-request token guarantee. The gateway's configured daily budget
remains the monetary authority. Strict token admission awaits trusted prompt
bounds in [issue 445](https://github.com/oxstack/ox-llm/issues/445).
