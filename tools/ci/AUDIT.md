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
| oxa required test gate | 28m 48s | 5m 16s |
| oxa test compilation | 19m 03s | 1m 37s |
| extra parity invocation | 4m 04s | Removed duplicate invocation |
| MoE engine job | 11m 56s | 11m 52s |
| Flash engine job | 8m 31s | 7m 55s |
| lab job | 7m 16s | 7m 14s |
| clippy job | 1m 51s | 1m 40s |
| production ARM64 oxa build | 1m 09s | 1m 12s |

Old oxa cache results in those runs: **0/3 exact hits**, **2/3 successful prefix restores**.
The failed restore still spent time looking up the old prefix. The compile step then took
18m 52s, 19m 35s and 19m 03s. Cache transport success did not mean a useful test build was reused.
The three successful after runs are
[43681205618](https://github.com/oxstack/ox-llm/actions/runs/43681205618),
[43682202613](https://github.com/oxstack/ox-llm/actions/runs/43682202613), and
[43685496974](https://github.com/oxstack/ox-llm/actions/runs/43685496974).
Their oxa paths were 6m20s, 5m16s and 4m25s (archive job plus slower worker plus aggregate).
The longest full-gate job was MoE: 11m52s, 11m02s and 12m03s, median **11m52s**.
This meets the 10–12 minute target for active execution; runner queue waits and scheduling
gaps are excluded, so it is not a promise of submission-to-green time under saturation.
The compilation row measures child-binary/archive compilation; separate retained doctests
had a median 50s, included in the oxa path. All three runs passed the complete 2,758-test
inventory with mandatory bwrap, three existing static ignores and zero runtime skips.

Across the six Rust cache jobs, exact hits were **0/6 cold**, **6/6 warm**, and **3/6 after
source/config changes**: **9/18 overall (50%)**, or **9/12 after the cold run (75%)**.
The third run's three misses restored compatible dependency prefixes; source changes still
trigger Cargo freshness checks. These are same-branch measurements, not a claimed cross-PR
hit rate. Cache transport success alone is not counted as an exact hit.
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

[43680225010](https://github.com/oxstack/ox-llm/actions/runs/43680225010) exposed two runner
compatibility issues and is not a successful after sample. Minimal Fedora checkout used a
source archive without `.git`; the key helper now explicitly supports that checkout mode
and excludes generated targets. The new x86_64 workers are pinned to Ubuntu 24.04, matching
the ARM64 sandbox baseline: `ubuntu-latest` had migrated to 26, whose symlink error text broke
an existing read-only assertion. The assertion is retained unchanged. The native OS identity
also enters the cache fingerprint.

The third hosted attempt, [43683257769](https://github.com/oxstack/ox-llm/actions/runs/43683257769),
caught a pre-existing TUI script race in two daily-driver tests: seeing the final reply text
is earlier than receiving `AgentSettled`, so `/new` or `/tree` could be rejected as busy.
A 250ms mock gap before SSE `[DONE]` reproduced it locally (the old script failed at its
unchanged 60-second deadline). The scripts now use a read-only HTML export as a worker fence:
its notice follows `AgentSettled` in the same FIFO, and exporting does not add a tree entry.
The existing controls/navigation assertions and deadlines remain. Both flows retain the
explicit mock gap, and all 18 daily-driver tests pass locally. This is synchronization,
not a retry, timeout increase or removed test. The failed attempt remains part of the audit
record; the third successful sample confirms the final harness.

After measurement, main merged B2. The real workflow conflict was resolved by retaining the
archive/shard implementation and removing the retired `OXA_PARITY_PROFILE` environment
variable. A separate full run validates that composition; the timing samples above precede
that feature deletion and are not presented as measurements of a smaller test inventory.
