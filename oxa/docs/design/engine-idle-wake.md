# Signal-driven engine idle, and a pre-registered watcher-cadence plan

Status: proposed; **no runtime change in this document's PR**. The engine patch below needs the
engine owner's explicit go before it is written, and the watcher cadence below is a separate
proposal that is not authorized by this document. Nothing here changes an interval, a default, a
threshold or a persistence deadline.

Task: issue #878 (parent #864), milestone v0.2.2, label `after-0.2.1`. Every line reference was
read from the checkout at `69a81515ad3ce39dbf048112fd0cfad476f4b73f`; no build, benchmark, provider
call or hardware run was performed for this document. **No idle-power improvement has been measured
anywhere in this repository.** The gains discussed below are hypotheses until a separate immutable
measurement receipt exists.

## 1. What is on main today

Engine (`engine/crates/server/src/main.rs`):

- `1268` `static TERM: AtomicBool`; `1270` `on_term` stores `true` with `SeqCst`. The handler is
  already async-signal-safe: one atomic store, no lock, no allocation, no formatting.
- `1517`–`1527` `libc::signal` registers `on_term` for `SIGTERM` and `SIGINT`.
- `1529`–`1574` a detached thread that `sleep(50 ms)` (`1530`), tests `TERM` (`1531`) and, on a
  trip, runs the bounded shutdown write: `t0` at `1540`, a **2500 ms** deadline at `1541`, the
  running/idle/persist/preload flags and `fair.cancel_all()`, a `try_lock` on the worker with a
  **1 s** cap (`1566`) and a 10 ms retry sleep (`1570`), then `std::process::exit(0)` (`1573`).
- The 50 ms sleep is the only reason the thread exists while idle. It is a poll, not a wait.

oxa client (`oxa/oxc-local/`):

- `src/native.rs:457` `watch()` holds the state mutex and loops: `advance()` (`465`), then
  `wake.wait_timeout(state, 250 ms)` (`472`–`476`). Every timeout wake runs `advance()`.
- `src/native.rs:479` `advance()` calls `lifecycle.poll(now, source.sample(), health)`.
- `src/memory.rs:21` `MemorySource::sample()` reads `/proc/self/cgroup`, `/proc/meminfo`,
  `/proc/pressure/memory`, the cgroup `memory.events`, `memory.swap.current`, `memory.swap.max` and
  `/proc/vmstat` — seven file reads per tick, on every tick, including a fully idle Ready engine.
- `src/configured.rs:162` `health_interval: 250 ms`; `src/lib.rs:230` `next_health = now +
  health_interval`; `src/lib.rs:89` rejects a zero interval. With `unhealthy_limit: 2`
  (`configured.rs:163`) an unhealthy engine is detected within about 500 ms.
- `src/lifecycle.rs:406` `poll()` is the single decision point: memory guard, supervisor health,
  idle unload (`493`–`497`). `src/queue.rs:74` already states the intended rule for admission:
  "No polling timer is used: release, shutdown and cancellation drive wakes."

So an idle resident engine costs, per 250 ms: one condvar timeout, seven procfs/cgroup reads, and
one HTTP `/health` probe. The engine's signal thread wakes every 50 ms. Neither figure is a power
measurement; both are counts read from the source.

## 2. Design: an async-signal-safe blocking wake for the engine

Goal: the signal thread blocks until a signal arrives, and the handler stays async-signal-safe.
Replace **only** the signal-flag poll. Nothing else in the shutdown path moves.

### Options

| Option | Handler work | Process-wide change | Notes |
|---|---|---|---|
| Self-pipe (`pipe2(O_CLOEXEC\|O_NONBLOCK)`) + `poll(2)` | one `write(2)` | none | keeps the existing `libc::signal` registration and handler shape; no new dependency |
| `signalfd(2)` + `poll(2)` | none | must block the signals in every thread before the thread starts | a thread that misses the mask gets the default action and the process dies |
| `eventfd(2)` + `poll(2)` | one `write(2)` of 8 bytes | none | same shape as the self-pipe, fewer bytes |

**Recommendation: the self-pipe.** It is the smallest change that keeps the current handler, needs
no process-wide signal mask (the tokio runtime threads at `1575` are untouched), and adds no
dependency. `signalfd` is the fallback if the owner prefers no handler at all; it is a larger
change because the mask must be installed before any thread is created.

### Patch plan (one engine PR, engine-owner go required)

1. Next to `TERM` (`1268`), add `static WAKE_W: AtomicI32` holding the write end, `-1` when unset.
2. In `on_term` (`1270`), keep the `TERM.store(true, SeqCst)` and add a non-blocking
   `libc::write(fd, b"\0".as_ptr().cast(), 1)` when the fd is set. `write(2)` is on the
   async-signal-safe list. Ignore the result. No lock, no allocation, no formatting, no `eprintln!`.
3. Before spawning the thread, `pipe2(&mut fds, O_CLOEXEC | O_NONBLOCK)`, publish `fds[1]` in
   `WAKE_W`, keep `fds[0]` for the thread.
4. In the thread body, replace `sleep(50 ms)` + `TERM.load` (`1530`–`1533`) with
   `libc::poll(&mut pollfd { fd: r, events: POLLIN, revents: 0 }, 1, -1)`, looping on `EINTR`,
   then draining the pipe with a non-blocking `read(2)`, then the existing `TERM` check and the
   existing shutdown block unchanged.
5. Keep the 10 ms retry sleep at `1570`: it waits for the worker inside the bounded shutdown, it is
   not a signal poll.

Unchanged by this patch: the 2500 ms deadline (`1541`), the 1 s worker cap (`1566`),
`shutdown_writes`, `fair.cancel_all()`, `process::exit(0)`, engine numerics, default flags,
thresholds, gate limits and every persistence receipt.

### Hazards the patch must handle

- **Signal before the pipe exists.** `TERM` is set but the thread would block forever in `poll`.
  Fix: after installing the pipe, test `TERM` once before entering `poll` (or make the first
  `poll` a zero-timeout one). This ordering hazard gets a test.
- **`pipe2` fails.** Keep the current 50 ms poll as a fallback and log one line. Shutdown must not
  start depending on a new syscall succeeding.
- **`write` returns `EAGAIN`** (pipe full). The flag is already set; the first wake sees it. Ignore.
- **Spurious wake / `EINTR`.** Loop; re-check `TERM`.
- **Handler re-entrancy.** The handler stays a store plus a `write`; it never touches the mutexes
  the shutdown path takes.

### Ordering note

The engine patch removes 20 wakes/s of a thread that does nothing but sleep. The oxa watcher still
wakes 4 times/s and does seven file reads plus a health probe each time. The engine patch is
therefore the smaller of the two effects, and it is the one that needs no policy decision: it
changes no interval and no threshold. The larger idle cost sits in the cadence proposal in
section 3, which is the one that needs the owner's decision.

## 3. Pre-registered watcher-cadence qualification plan (oxa side)

This is a **separate proposal**. It is not authorized by this document, and it is not a reason to
slow the memory guard. Loading, generating, backoff and stopping keep the current cadence; request,
cancel, shutdown and target-change wakes stay immediate.

**Hypothesis H1.** With the engine's signal thread blocking, the remaining idle wake cost is the
oxa watcher: one condvar timeout per 250 ms per resident engine, each doing seven procfs/cgroup
reads and, when Ready, one `/health` probe.

**Proposal P1 (not authorized here).** A Ready+idle cadence of 1–2 s, entered only when the state
is Ready, no request is in flight, no request is queued, no target or switch is pending, and the
last samples were healthy. Any notify wakes immediately. The cadence is a property of the wait,
not of the guard: `lifecycle.poll` keeps its thresholds and its `unhealthy_limit × health_interval`
detection bound.

### Instrumentation (required before any measurement)

A wake-source counter that separates (a) condvar timeout wakes, (b) notify wakes, (c) procfs
samples, (d) health probes. It must be test-only — a `#[cfg(test)]` accessor or a test-only Cargo
feature — so that it changes no runtime behaviour. Where it lives is open question 3 below.

### Procedure

60 s idle with one resident Ready engine and no requests; record the four counters before and
after; repeat three times; report the median and the range. The engine-side change and the oxa-side
cadence are measured in separate runs, so the two wake sources are never conflated: run A is the
engine patch alone with the watcher cadence unchanged, run B adds the cadence.

### Pre-registered pass/fail (fixed before any measurement is taken)

P1 is **accepted** only if the 60 s idle record shows the timeout-wake count reduced by at least
4× and the procfs sample count reduced by at least 4×, with zero missed detections in the
fault-injection controls below.

P1 is **rejected** if any of these holds:

- low-memory detection for a Ready+idle engine exceeds the current bound
  (`unhealthy_limit × health_interval` = 500 ms) by more than the cadence;
- a shutdown or cancel wake is delayed beyond the current bound;
- a queued request waits longer than the current bound;
- the counters show the cadence did not actually change.

If the counters cannot separate the sources, the measurement is **void** and P1 stays unapproved.

### Fault-injection controls (existing synthetic stubs, no hardware)

`oxa/oxc-local/examples/stub_lifecycle.rs`, `stub_supervision.rs` and `native_local_smoke.rs` with a
fake memory source that flips to a low-memory sample, and a stub engine that stops answering
`/health`. Assert the stop reason and the detection-latency bound under both cadences. The engine
side reuses the existing stub signal/process/lifecycle controls.

### Receipt

A number is evidence only as an immutable receipt attached to the PR: command, head SHA, host,
the four counters and the raw log. A number without a receipt is not a result, and no number in
this document is a measurement.

## 4. What the implementation PR must show (acceptance mapping)

- `SIGINT`/`SIGTERM` while idle, while generating and while persisting, through the existing
  synthetic stub signal/process/lifecycle controls: exactly-once shutdown, no deadlock, the same
  2500 ms persistence bound and the same receipts.
- Prompt cancellation unchanged.
- Before/after 60 s idle records that separate server signal-thread wakes from client
  watcher/procfs/health reads.
- Any approved cadence change preserves low-memory and refusal detection and shutdown behaviour.
- Native CPU-feature tests stay on the existing qualified route; no runner or policy change; no
  hardware inference from a stub.

## 5. Explicitly out of scope

No interval or default change, no timer waiver, no engine edit before the engine owner's go. No new
daemon or framework, no credentials, no real provider, device, SDK or model runs, no sysfs writes,
no new numerical, default or admission policy, no force-push, no merge.

## 6. Open questions for the owner

1. Self-pipe or `signalfd`?
2. Is the fallback to the current 50 ms poll on `pipe2` failure acceptable, or should that failure
   be fatal?
3. Should the wake-source counters be a test-only feature inside `oxa/oxc-local`, or a separate
   measurement binary under `oxa/oxc-local/examples/` so that no product code changes at all?
