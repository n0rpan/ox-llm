# Hosted CI timing audit (O-221)

The baseline is nine completed full-gate runs (six merge-queue runs and three manual runs).
The before column below uses the median of the three latest successful queue gates:
[43676182212](https://github.com/oxstack/ox-llm/actions/runs/43676182212),
[43675374727](https://github.com/oxstack/ox-llm/actions/runs/43675374727), and
[43674011206](https://github.com/oxstack/ox-llm/actions/runs/43674011206).
Job durations exclude time waiting for a runner. The oxa path includes its build, workers,
artifact transfers and aggregate gate after this change.

| Full-gate measurement | Before, median of 3 | After, median of 3 |
|---|---:|---:|
| oxa required test gate | 28m 48s | Pending hosted measurements |
| oxa test compilation | 19m 03s | Pending hosted measurements |
| extra parity invocation | 4m 04s | Removed duplicate invocation |
| MoE engine job | 11m 56s | Pending hosted measurements |
| Flash engine job | 8m 31s | Pending hosted measurements |
| lab job | 7m 16s | Pending hosted measurements |
| clippy job | 1m 51s | Pending hosted measurements |
| production ARM64 oxa build | 1m 09s | Pending hosted measurements |

Old oxa cache results in those runs: **0/3 exact hits**, **2/3 successful prefix restores**.
The failed restore still spent time looking up the old prefix. The compile step then took
18m 52s, 19m 35s and 19m 03s. Cache transport success did not mean a useful test build was reused.
New cache exact-hit outputs and three hosted after runs are required before claiming the target.
Local validation: all 2,758 inventoried tests passed across two archived partitions with mandatory
bwrap and zero runtime skips; the three opt-in ignores were retained. Both partitions together
matched the inventory exactly. The partitions took 98.5s and 88.7s locally; these are not hosted
measurements or a claim about x86_64 performance.

## Changes and boundaries

Production ARM64 oxa and engine release profiles stay unchanged. The model-free oxa tests
inherit release behavior (including panic/overflow settings), with a CI-only profile that
uses optimization level 1, no LTO and 16 codegen units. This avoids optimizing/linking each
of hundreds of standalone harnesses for production size.

One x86_64 job compiles all tests and both child binaries and publishes a nextest archive.
Two hosted x86_64 workers reuse that archive, each with mandatory working bwrap and isolated
HOME/XDG roots. The required check verifies both workers succeeded and compares their reports
against the full archive inventory: no lost tests, no duplicates, no runtime skip notices.
Only the three existing opt-in measurement/transcript-dump ignores are allowed. Missing
reports, new ignored tests, failures and an unusable sandbox fail closed. Retries stay off.

The compiler-aware Rust cache uses the permitted, pinned `actions/cache` backend, runs after toolchain setup,
and separates Fedora/Ubuntu, architecture and feature configurations. Engine MoE and Flash
keep separate builds because their features and linked libraries differ; clippy performs a
separate analysis rather than generating reusable runtime objects. Production ARM64 binaries
cannot replace x86_64 CI test executables. The artifact reuse is between identical test builds.

Drafts retain formatting and scans only. Ready PRs check changed Rust components with clippy
and run the stub library suite for engine changes; all retain oxa/stub build-smoke and loopback
contract checks. Unknown paths conservatively select all components. Queue/manual/nightly
runs retain every gate, main retains the existing fast post-queue policy, and skipped required
jobs still report. No workflow paths filters or additional cancellation are introduced.

## Redundant-test proposal

Remove only the second CI invocation of `cargo test -p oxa-parity`. The preceding workspace
invocation already includes the identical crate under `OXA_PARITY_PROFILE=local`; in all three
baseline logs the same parity binaries and test counts ran twice. Previously the later step
also ensured the child binaries existed. The archive now requires both binaries before any
partition starts, and the inventory/report comparison proves every parity test ran once.
Workspace doctests still run separately in the archive job (all were empty in the baseline).
No test case, security test, sandbox assertion or scanner is removed. No further test deletion
is proposed without evidence from the new per-test reports.

The first hosted attempt, [43680020949](https://github.com/oxstack/ox-llm/actions/runs/43680020949),
failed validation before jobs started: repository policy permits only GitHub/oxstack actions,
so the third-party Rust cache action was rejected. It contributes no timing sample. The final
implementation uses the existing GitHub cache action with explicit compiler/native-toolchain,
manifest/config, build-flag and source fingerprints. Source-only changes retain a compatible
restore prefix; compiler, feature/profile variant and dependency changes do not. Cargo retains
its normal freshness validation; no source or artifact timestamps are rewritten.
