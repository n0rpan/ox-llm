# Decisions

This file records the decisions that shape the engine, the lab package and the oxa harness, in ADR style.
When two entries conflict, the newer decision wins; superseded entries stay as one-line pointers.
Each entry keeps its original O-number IDs so older private notes can cross-reference it.

## 1. Architecture and product scope

### ADR-01: Project goal and priority order (O-0, O-56, O-58; 2026-11-10)
**Decision:** Run local LLMs on Snapdragon X laptops as close to the hardware limit as possible, at acceptable quality, for real agentic coding. Chat and agentic workloads come first; learnings carry over to images after.
**Why:** Agentic coding is the main daily use, and its latency is what users feel.
**Status:** active.

### ADR-02: Client-neutral engine and controller core (O-133, O-131, O-132; 2026-11-13)
**Decision:** The engine and controller must not depend on any client's quirks: no client version pins, no client byte templates, no per-client hacks in the core. Client-specific behaviour lives only in thin, optional edge adapters.
**Why:** A system tuned to one client's quirks breaks on the next client release and cannot serve other clients.
**Status:** active. Supersedes the client probe/profile plan (O-131) and the version-aware launcher (O-132).

### ADR-03: Clean OpenAI-compatible API; mechanism in the engine, policy in the harness (O-135; 2026-11-13)
**Decision:** The engine and controller expose a clean OpenAI-compatible API for any client. Cache mechanisms (prefix reuse, token-exact echo, snapshots) are generic and live in the engine. Policy (when to compact or checkpoint, history shaping, session handling) lives in the harness, at most through small, documented, client-neutral hints any client may send.
**Why:** Keeps the core reusable while still letting our own client be optimized for speed.
**Status:** active.

### ADR-04: oxa, our own Rust coding-agent harness with full pi parity (O-134, O-137, O-138, O-140; 2026-11-13)
**Decision:** Build `oxa`, a rewrite of the pi coding agent (version 1.5.2) in Rust, tailored to this engine. The goal is full feature parity, not an MVP; any omission needs a stated reason. Parity is checked by a test suite written independently of the implementation. The external client used so far becomes legacy.
**Why:** All engine-specific choices need one home, and a harness we own can make them without generic-client compromises.
**Status:** active, in progress.

### ADR-05: oxa default permission mode is "workspace" (O-137; 2026-11-13)
**Decision:** oxa's default permission mode is `workspace`.
**Why:** A sensible default between unrestricted access and constant prompts for a coding agent working in one project.
**Status:** active.

### ADR-06: One monorepo with three products (O-139, O-102; 2026-11-13)
**Decision:** One repository with `engine/` and `oxa/` as separate Cargo workspaces and `lab/` as the Python package. The private development repository starts from fresh history; oxa starts there.
**Why:** The three products ship together and share decisions and docs; separate build units keep their dependencies apart.
**Status:** active.

### ADR-07: No Anthropic Messages API shim (O-102; 2026-11-11)
**Decision:** The controller offers the OpenAI-compatible API only.
**Why:** One well-tested API surface is enough for our clients.
**Status:** active.

### ADR-08: Model lineup and defaults (O-1, O-35, O-100, O-113, O-27, O-95; 2026-11-10 to 2026-11-12)
**Decision:** Ornith-1.5-35B-A3B is the default chat model (bootstrap default too), with Qwen3.6-35B-A3B as the second MoE and the large-MoE Flash engine for hard tasks. The dense 27B model is removed. Ornith is no longer labelled beta.
**Why:** MoE models give the best speed per quality on this hardware; Flash is kept for problems the smaller models cannot solve.
**Status:** active.

### ADR-09: Image tool scope (O-25, O-38, O-100, O-103, O-113, O-30; 2026-11-10 to 2026-11-12)
**Decision:** The oxi image tool ships the CPU fast path at 512 px, and Studio defaults to 512 px. Larger sizes and other image paths are not part of the public build. FLUX.2 klein is the bootstrap default image model, subject to its licence check. The prompt-conditioning cache is opt-in.
**Why:** Ship only what passed its precision gate; chat and agentic work have priority over images.
**Status:** active.

### ADR-10: LAN sharing is opt-in (O-102, O-119; 2026-11-11)
**Decision:** LAN sharing is a setting, off by default. Loopback-only stays the default; with LAN sharing off there is no external listener.
**Why:** A local AI server should not be reachable from the network unless the user asks for it.
**Status:** active, shipped in v0.1.3.

### ADR-11: NPU work is parked (O-4, O-35, O-61, O-66, O-71, O-76, O-106, O-109, O-113; 2026-11-10 to 2026-11-13)
**Decision:** NPU experiments are parked and are not part of the public build. The public engine builds and runs on the CPU.
**Why:** The measured results did not justify a product path yet.
**Status:** active.

### ADR-12: Maintenance mode after the release (O-130, O-24; 2026-11-13)
**Decision:** After the next release the project moves from continuous optimization to maintenance: work restarts when something is off or a new goal is set.
**Why:** The project has no users beyond the lab yet; continuous work has diminishing returns.
**Status:** active.

## 2. Engine and kernels

### ADR-13: Optimize what the measured wall time says (O-12, O-77, O-84, O-109; 2026-11-10 to 2026-11-12)
**Decision:** Pick performance work by its measured share of real session wall time. Prefill came before decode first; long context (32K to 64K and beyond, many turns) is now the target profile, and full-attention prefill is the largest remaining gap.
**Why:** Profiles of real agentic sessions showed where the time goes; assumptions about short sessions were wrong.
**Status:** active.

### ADR-14: Speculative decoding: MTP depth 1 (O-20, O-68, O-84, O-88, O-114; 2026-11-10 to 2026-11-12)
**Decision:** Keep MTP speculative decoding at depth 1 with no context cutoff. No small draft model. Prompt-lookup drafting is built only if measured data supports it.
**Why:** Depth 1 measured best; decode already runs close to the memory-bandwidth roof.
**Status:** active.

### ADR-15: Long-context levers that are not pursued (O-13, O-70, O-77, O-84, O-109; 2026-11-10 to 2026-11-12)
**Decision:** No sliding-window attention, attention sinks or KV eviction, no int8/q4/fp8 KV cache, no Flash long context, no chunked-prefill overlap.
**Why:** Each either loses quality agentic work depends on, or did not pay back in pricing.
**Status:** active. Supersedes the conditional int8 KV approval (O-13).

### ADR-16: Prefill attention kernels are opt-in until measured (O-70, O-109, O-117, O-119, O-123, O-136; 2026-11-12 to 2026-11-13)
**Decision:** New attention kernels (fused verify attention, fa2 variants, the f16 "PFG" path) ship behind flags, default off, and become default only after the registered quality predicate and a whole-append timing at 32K and 64K pass.
**Why:** Attention dominates long-context time, but a faster kernel is only worth it with unchanged quality.
**Status:** active. PFG is wired into Ornith's v0.2.0 production profile (`--pf-attn pfg`, O-141); Qwen stays `tiled`; the v0.2.0 gate pack qualifies it before release.

### ADR-17: Decode microbench "DEC-PB" (O-129, O-136; 2026-11-13)
**Decision:** Pursue the bit-identical DEC-PB decode change, microbench first.
**Why:** It is exact by construction, so only speed needs measuring.
**Status:** active.

### ADR-18: Flash expert cache sizing (O-8, O-27, O-30, O-32, O-39, O-16; 2026-11-10 to 2026-11-11)
**Decision:** The Flash expert cache size is a user preference (auto or a fixed tier). Auto picks the largest tier that fits and falls back to smaller tiers with a visible note, never an error. Public defaults are conservative; larger tiers are opt-in.
**Why:** The best size depends on how busy the desktop is; users must be able to choose.
**Status:** active. A dynamic cache governor (O-16) stays deferred.

### ADR-19: Memory guard based on pressure, not free memory (O-27, O-30, O-39; 2026-11-11)
**Decision:** Admission uses total memory minus engine needs minus a small resident floor; idle desktop pages may be swapped. Runtime protection uses memory pressure (PSI), OOM risk and a host floor. The engine's own memory never swaps.
**Why:** A pre-load free-memory rule refuses loads on a quiet desktop that would run fine.
**Status:** active.

### ADR-20: Flash disk prefix store (O-36, O-43, O-43b; 2026-11-11)
**Decision:** Flash keeps a disk prefix store so a reload does not re-prefill from scratch.
**Why:** It saves most of the reload time with all identity, memory and security gates passing.
**Status:** done, shipped.

### ADR-21: Flash IO core idle hint scoped to load (O-22; 2026-11-10)
**Decision:** While Flash is loaded, its IO core uses a shallow-idle latency hint instead of busy spinning; the hint is cleared on unload and on failure. Spinning stays a selectable fallback.
**Why:** Same speed as spinning, much cooler cores.
**Status:** active.

### ADR-22: System tuning is runtime-only and measured (O-14, O-15; 2026-11-10)
**Decision:** System-level tuning (memory frequency floors, swap and compression settings) is tried at runtime only, kept only on a measured gain, and any persistent change needs a separate decision.
**Why:** System changes are hard to undo and easy to over-credit.
**Status:** active.

### ADR-23: Storage classes for data (O-21, O-3, O-17; 2026-11-10)
**Decision:** Only public model weights may live on unencrypted storage, and only checksum-verified. Prompts, sessions, prefix and KV caches stay on encrypted storage.
**Why:** Weight reads are faster without encryption; user data must stay protected.
**Status:** active.

## 3. Quality gates and measurement policy

### ADR-24: Three classes of numeric gate (O-63, standing rules; 2026-11-10)
**Decision:** Lossless changes need identical tokens. Re-ordering changes need mean KLD within about 2x the engine's own batched-vs-single noise. Lossy changes need a stated KLD budget plus the hard-task set. Bit identity is a bug detector, not a quality gate.
**Why:** Gates stricter than the reference implementation's own noise would block exact-in-spirit improvements.
**Status:** active.

### ADR-25: Lean promotion gates (standing rules; 2026-11-10)
**Decision:** Promotion needs: the numeric gate for its class; one real agentic task passing; no OOM or swap at the max context; same boot, 3 warm runs vs 3 baseline runs, not slower by more than 3% (a gain only above 3%); a written one-step rollback. Nothing with a real regression goes to production.
**Why:** A lab needs fast, honest gates, not enterprise process.
**Status:** active.

### ADR-26: Lossy and accelerator gates use an exact f32 reference (O-46, O-62, O-63, O-64, O-65, O-68; 2026-11-11 to 2026-11-12)
**Decision:** Compare both the old and the new path against an exact f32 reference of the same model and positions. Use teacher-forced top-1 agreement over at least 1000 positions (code, prose, chat, tool JSON), KLD vs f32, the count of wrong top-1 at large f32 margins, and a task-level check. Free-running first-divergence is not used.
**Why:** Matching an older path's rounding is not quality; near-tie flips are noise, large-margin flips are not.
**Status:** active.

### ADR-27: Gates are registered before the run (O-67, O-73b, O-117, O-129; 2026-11-12 to 2026-11-13)
**Decision:** Gates and kill rules are written down as a decision before the measurement runs. Changes after data exists are recorded as revisions with a reason.
**Why:** Prevents choosing the gate to fit the result.
**Status:** active.

### ADR-28: Quality predicate for attention kernels (O-117; 2026-11-12)
**Decision:** A new attention kernel is compared teacher-forced on the same saved state at 32K and 64K against the current kernel, with a measured control floor for KLD and top-1, an f32 attention reference for logit and output error, and 100% tool-JSON validity on a long-session replay.
**Why:** Long-context kernels must not degrade the outputs agentic work depends on.
**Status:** active.

### ADR-29: Public performance claims (standing rules, O-79; 2026-11-10 to 2026-11-12)
**Decision:** README numbers need: same model file, quant and prompt; same boot and day; at least 3 warm runs (median and range); a pinned upstream llama.cpp baseline; commit, binary hash, receipt and reproduce command in the repo; settings and power state stated; decode and prefill separate. Comparisons use matched pairs; no inflated multipliers.
**Why:** Claims must be reproducible by others.
**Status:** active.

### ADR-30: Measure, do not estimate (O-18, O-76, O-67; 2026-11-10 to 2026-11-12)
**Decision:** Claim only measured numbers and log real wall time. Paper projections are discounted to 0.3 to 0.5x by default. No device work on a lever before a profile shows its time share and its transfer cost is priced on paper.
**Why:** Paper projections were repeatedly too optimistic.
**Status:** active.

### ADR-31: Build the final candidate, then test once (O-18, O-83, O-83a; 2026-11-10 to 2026-11-12)
**Decision:** Write the full solution first and run the full gate once on the final candidate, in a few combined hardware batches. Short idea-check probes (seconds to a few minutes) are encouraged when they can kill an expensive build early.
**Why:** Code is cheap; hardware time is the bottleneck.
**Status:** active.

### ADR-32: When to stop optimizing (O-24, O-76; 2026-11-10 to 2026-11-12)
**Decision:** "Done" is judged on real sessions (time to first token and per-turn latency targets). A workload stops after three consecutive measured levers under 3%, or when no further large gain is reachable without losing quality.
**Why:** Avoids endless tuning with no visible benefit.
**Status:** active.

### ADR-33: Decision process (O-5, O-19, O-34; 2026-11-10 to 2026-11-11)
**Decision:** Routine decisions are made by the maintainers when an independent review agrees. Anything public, secrets, paid usage and destructive actions on user data stay with the owner. Every production change keeps a one-step rollback and a post-install regression pass.
**Why:** Fast progress without losing control over irreversible steps.
**Status:** active.

## 4. Cache, sessions and compaction

### ADR-34: Persistent sessions and primers (O-9, O-10; 2026-11-10)
**Decision:** Parked sessions survive idle unload and are stored on disk. Harness primers (system prompt and tool schemas only, never user turns) persist with owner-only file permissions.
**Why:** Avoids full re-prefill after idle unload.
**Status:** done, shipped.

### ADR-35: Stable history rendering (O-73, O-73b; 2026-11-12)
**Decision:** Render chat history so earlier assistant turns stay byte-identical across requests (preserve empty think blocks for the MoE profiles).
**Why:** Template changes to old turns made the prefix cache miss mid-session.
**Status:** done, shipped.

### ADR-36: Exact cache design (O-86, O-91, O-92, O-94, O-96; 2026-11-12)
**Decision:** The prefix cache must be exact: deterministic rendering, a per-turn hit check with alarms, snapshots of KV and recurrent state at turn boundaries, longest-match resume, RAM and disk tiers with eviction, fast resume after a model switch or reboot. Cache identity uses a numerics epoch (enforced in CI) rather than the binary hash. With thinking on, reasoning stays in context until compaction.
**Why:** A local cache miss costs minutes of re-prefill at long context.
**Status:** done, shipped. Further cache tuning is stopped (O-109).

### ADR-37: Disk cache budget and lifetime (O-93, O-96; 2026-11-12)
**Decision:** Default disk budget 64 GB, configurable in the widget; session lifetime 7 days by default, configurable.
**Why:** Large enough for long sessions, bounded so it never fills the disk.
**Status:** active.

### ADR-38: Context window and default mode (O-6, O-89, O-102, O-105, O-120; 2026-11-10 to 2026-11-13)
**Decision:** The context window is 128K. The default mode is "fast", which compacts near 48K; "deep" (about 100K) is selectable.
**Why:** Measured append-prefill speed drops steeply with depth, so earlier compaction is faster overall.
**Status:** active. Supersedes the 32K limit (O-6) and the earlier deep default (O-89, O-102).

### ADR-39: No default tool-output cap (O-74, O-77, O-119; 2026-11-12 to 2026-11-13)
**Decision:** Tool output is not truncated by default; a cap exists as a flag, default off. Compaction keeps a recent-token tail.
**Why:** No measured gain, and truncation can cause re-reads and retries.
**Status:** active.

### ADR-40: Compaction policy and the "swallowed prompt" issue (O-120, O-124, O-128, O-135; 2026-11-13)
**Decision:** Keep extra user snapshots at compaction (exact, no extra RAM). The case where a prompt submitted above the compaction trigger is folded into history is a client behaviour; it is fixed in oxa by compacting after the reply, not by a generic workaround in the engine.
**Why:** Snapshots avoid re-prefill after compaction; the client issue belongs in the client.
**Status:** active.

### ADR-41: Long requests stay alive (O-111, O-119, O-126; 2026-11-12 to 2026-11-13)
**Decision:** The engine sends SSE keepalive comments during long prefill, and the controller allows long timeouts, so very large appends at deep context do not abort or retry.
**Why:** Clients that only count data events as liveness aborted long prefills.
**Status:** done, shipped in v0.1.3.

### ADR-42: Pinning sampling settings in the client config (O-7; 2026-11-10)
**Status:** superseded by ADR-02.

## 5. Releases and versioning

### ADR-43: Releases ship only qualified work (O-29, O-36, O-103, O-119; 2026-11-11 to 2026-11-13)
**Decision:** A public release is one squashed snapshot per version, published only on real progress (qualified, installed improvements or fixed bugs) with receipts, updated docs, clean scans and green CI from a fresh clone. Work that has not passed its gates moves to the next version.
**Why:** The public history should hold only working, evidenced changes.
**Status:** active.

### ADR-44: Mandatory pre-release audit (O-31, O-87, O-90; 2026-11-11 to 2026-11-12)
**Decision:** Before each release, after code freeze, run a full audit: bugs, dead code, tests that cannot fail, stale or wrong docs, and user-facing issues. Every finding is fixed or deferred with a reason.
**Why:** Audits found real bugs and wrong claims each time.
**Status:** active.

### ADR-45: Install qualification with a real rollback (O-122, O-126; 2026-11-13)
**Decision:** A release candidate is installed temporarily after its gates pass, the actual rollback is run and checked with a real chat, then the candidate is reinstalled. Production is always left on a fully verified state.
**Why:** A rollback that was never exercised is not a rollback.
**Status:** active.

### ADR-46: Fixes carry forward (O-108; 2026-11-12)
**Decision:** Every bug fix, change and learning carries forward into all later candidates.
**Why:** Avoids regressions between parallel release lines.
**Status:** active.

### ADR-47: Known issues are documented, not hidden (O-111, O-121, O-124; 2026-11-12 to 2026-11-13)
**Decision:** A release may ship with a known issue only if it is documented with a workaround and has a planned fix.
**Why:** Honest release notes beat a delayed release for rare paths.
**Status:** active.

### ADR-48: Public release needs the owner's go (O-29, O-104, O-115, O-125; 2026-11-11 to 2026-11-13)
**Decision:** Each public release needs the owner's explicit go, conditional on verified docs with no private information and a green final build. Substantive README wording changes go back to the owner.
**Why:** Public actions cannot be undone.
**Status:** active.

### ADR-49: Released versions v0.1.0 to v0.1.3 (O-30, O-33, O-116, O-119, O-127; 2026-11-11 to 2026-11-13)
**Decision:** v0.1.0 first public snapshot; v0.1.1 CI binaries, security fixes and Flash improvements; v0.1.2 chat and agentic release; v0.1.3 correctness release (long-request keepalive, controller split, proxy security fixes).
**Status:** done.

### ADR-50: Plans for v0.1.4 (O-107, O-110, O-114, O-125, O-129, O-130, O-131; 2026-11-12 to 2026-11-13)
**Status:** superseded by ADR-51.

### ADR-51: Next release is v0.2.0 (O-136, O-139; 2026-11-13)
**Decision:** There is no separate v0.1.4. The next release, v0.2.0, ships the client-neutral core, oxa and the pending engine work together. Install gates run once, for that combined release; hardware is used meanwhile only for decision-making measurements.
**Why:** With no users beyond the lab, a separate release cycle would only slow the new goal.
**Status:** active.

## 6. Repository, CI and public boundary

### ADR-52: Private development, public snapshots (O-29, O-102, O-139; 2026-11-11 to 2026-11-13)
**Decision:** Development happens in a private repository; public repositories receive only release snapshots. A pre-push secret and path scan must pass. v0.2.0 goes to a new public repository whose name is decided at release; the older public repositories are left as they are for now.
**Why:** Keeps work-in-progress and private notes out of public history.
**Status:** active.

### ADR-53: Only our own code in the repositories (O-44, O-44d, O-48, O-51, O-113; 2026-11-11 to 2026-11-12)
**Decision:** Repositories, public and private, contain only our own code and properly licensed vendored code; no copied third-party text, tables, constants or code. NPU code is not in the public build, apart from what the default build needs.
**Why:** Keeps the project's history clean and safe to publish.
**Status:** active.

### ADR-54: Build and test on CI runners (O-97, O-99; 2026-11-12)
**Decision:** Builds and tests run on separate arm64 and x64 CI runners wherever possible. The target laptop is reserved for hardware gates and timing.
**Why:** Builds on the target laptop disturb timing and compete with measurements.
**Status:** active.

### ADR-55: Branch hygiene (O-78, O-102; 2026-11-12)
**Decision:** Delete a branch after it is merged; superseded work-in-progress branches are removed after a backup bundle.
**Why:** Keeps the repository readable.
**Status:** active.

### ADR-56: Security hygiene of local state and the proxy (O-100, O-119; 2026-11-12 to 2026-11-13)
**Decision:** Local state directories are owner-only and files are owner-readable only. The proxy never forwards cookies or authorization headers to the backend and closes on malformed requests.
**Why:** Local session data is private; the proxy must not leak credentials.
**Status:** active.

## 7. Licensing

### ADR-57: MIT for our code; vendored code keeps its licence (O-23, O-134; 2026-11-10 to 2026-11-13)
**Decision:** Our own code is MIT; the copyright line is exactly as in LICENSE. Vendored or borrowed code keeps its source licence in its own directory with its notices, and changed files say they were modified. Ported pi design or code keeps pi's MIT attribution and NOTICE.
**Why:** Simple licensing for users, correct attribution for upstream authors.
**Status:** active. Supersedes an earlier Apache-2.0 choice.

### ADR-58: Trademark notice (O-79, O-80; 2026-11-12)
**Decision:** READMEs state that Snapdragon and Hexagon are trademarks of their owner and that this is an independent project, not affiliated with, sponsored by or endorsed by Qualcomm.
**Why:** The project names the hardware it targets.
**Status:** active.
