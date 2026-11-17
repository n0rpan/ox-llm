# Idle wake and watcher cadence (design)

Status: proposed. **No runtime change in this note's PR.** The engine part needs an explicit
engine-owner go before any line of `engine/` changes; the client part is a separate oxa PR.
Issue: #878 ("Felt speed 4/10: design signal-driven engine idle and qualify watcher cadence").

Tags: [S] read from source, [E] estimate or hypothesis, [M] measured. Nothing here is [M]: no
measurement was taken for this note, and a stub cannot produce one.

## What the felt cost is

Two independent timers run for as long as a model is resident, and neither is needed for
correctness.

**Server, 20 Hz.** `engine/crates/server/src/main.rs:1268` holds a `TERM` atomic; `:1519` installs
`on_term` for SIGTERM and SIGINT; `:1529` spawns a thread that sleeps 50 ms, loads the flag and
loops. An idle engine therefore wakes a thread 20 times a second forever, and that thread holds a
clone of `State` for the life of the process. Each wake is cheap (a sleep expiry and one atomic
load), but it is a permanent 20 Hz timer on a laptop that otherwise idles at a fraction of a
percent of CPU, and it keeps one core out of deep idle residency [E]. `TERM` is read nowhere else
in the crate [S].

**Client, 4 Hz.** `oxa/oxc-local/src/native.rs:474` waits on the wake condvar with a 250 ms
timeout and calls `advance()` on every expiry, whether or not anything changed. `advance()` runs
`Lifecycle::poll` (`oxa/oxc-local/src/lifecycle.rs:406`), which samples memory
(`oxa/oxc-local/src/memory.rs:21`: seven procfs/cgroup reads — `self/cgroup`, `meminfo`,
`pressure/memory`, `memory.events`, `memory.swap.current`, `memory.swap.max`, `vmstat`,
`pids.events`) and, through `Supervisor::poll` (`oxa/oxc-local/src/lib.rs:230`), issues a health
HTTP probe every `health_interval` = 250 ms (`oxa/oxc-local/src/configured.rs:162`). So a Ready,
idle engine costs four procfs sample sets and four HTTP probes per second [S].

Both are the same shape of waste: a fixed-rate poll standing in for an event that is rare. The
issue's 4/10 is a felt-speed and energy complaint, not a correctness bug.

## Part 1 — engine: an async-signal-safe blocking wake

**Goal.** The shutdown thread sleeps until a signal actually arrives. No polling, no locks and no
allocation in a signal handler, and no change to what shutdown does.

**Preferred form: `sigwait`, no handler at all.** Block SIGTERM and SIGINT in the process before
any thread is created (`pthread_sigmask` with `SIG_BLOCK`, before the tokio runtime and before the
engine's own threads), then have one thread call `sigwait` on that set. `sigwait` is not a signal
handler, so the async-signal-safety rules do not apply to the code that follows it: the thread
wakes with the signal number and runs the existing shutdown body directly. Delete `TERM`,
`on_term` and the 50 ms loop.

**Fallback form: self-pipe.** If the mask cannot be established before every thread exists (the
tokio workers and the engine's pool are created in several places), keep a handler that does only
`write(fd, &1u8, 1)` — async-signal-safe — and have the thread block in `read`. Same shape, one
more file descriptor.

**What does not change.** The body between the wake and `std::process::exit(0)`
(`main.rs:1534-1573`) is untouched: cancel `running`/`idle`/`persist`/`preload`, `fair.cancel_all()`,
take the worker within 1 s, write only what is not on disk yet until **2500 ms after the signal**
(`main.rs:1539-1540`), manifest last, one log line, `exit(0)`. `t0` is still taken at the moment the
signal is observed, so the persistence deadline and its receipts are identical. Exactly-once holds
for the same reason it holds today: the thread runs the body once and exits, and a second signal
during the body is not observed.

**Sketch** (about 40 lines in `main.rs`, no new dependency; `libc` is already used):

```rust
// before the runtime and before any engine thread
let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
unsafe {
    libc::sigemptyset(&mut set);
    libc::sigaddset(&mut set, libc::SIGTERM);
    libc::sigaddset(&mut set, libc::SIGINT);
    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
}
std::thread::spawn(move || {
    let mut sig: libc::c_int = 0;
    if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
        return; // no signal: nothing to do, the process keeps serving
    }
    // ... the existing shutdown body, unchanged, with t0 taken here ...
    std::process::exit(0);
});
```

**Risks and how the patch answers them.**

- *Mask inheritance.* A child spawned after the mask is set inherits it and would ignore SIGTERM.
  The engine spawns no children after startup [S], but the mask must be set before the runtime and
  before the engine threads; the patch sets it at the top of `serve`, and the test below fails if
  it is set too late.
- *A signal that arrives before the thread starts.* It stays pending on the blocked set and
  `sigwait` returns immediately. This is the property the poll loop cannot have.
- *A signal that arrives while the body runs.* Ignored, as today.
- *A signal that never arrives.* The thread blocks forever, which is the point; it costs no wakeups.

**Acceptance evidence, all with existing synthetic controls and no hardware.**

- `engine/crates/server/src/tests_v020_fair.rs:780`
  (`sigterm_cancels_a_suspended_run_and_frees_the_worker`) already replays the shutdown body; it
  must keep passing unchanged.
- `engine/crates/server/src/tests_state_p2b.rs:793`
  (`shutdown_dispatch_never_starts_the_unbounded_legacy_writer_with_the_state_cache_on`) must keep
  passing unchanged.
- New: a real-signal test. The test binary re-executes itself
  (`std::env::current_exe()` plus an env marker) into a child that installs the mask and the
  `sigwait` thread, then the parent sends SIGTERM and asserts the child exits 0 within the
  deadline. Before the patch the child dies from the default action, so the test fails first. Run
  it for the three states the issue names: idle, generating, persisting.
- New: exactly-once. The child writes one manifest and one log line; the parent asserts one of
  each and a single exit.
- New: no deadlock. The parent asserts the child exits within the 1 s worker bound plus the 2.5 s
  persistence bound, with a generous CI margin.
- `engine/tools/ci/cache_functional.py` already stops the server with `SIGTERM` and has a SIGKILL
  restart arm; both must keep passing.

## Part 2 — client: watcher cadence (separate oxa PR, no engine change)

**Today.** One 250 ms condvar timeout for every state, and `advance()` on every expiry.

**Proposal.** Keep the condvar notify for every state change — spawn, request, cancel, drop,
shutdown already wake the watcher immediately — and make only the *timeout* depend on state:

| State | Timeout | Health probe | Memory sample |
|---|---|---|---|
| Starting, Stopping, Backoff | 250 ms (unchanged) | every `health_interval` | every wake |
| Ready, idle | 1–2 s | every `health_interval` | every wake |
| Ready, generating | 250 ms (unchanged) | every `health_interval` | every wake |

**Why it is safe.**

- The memory guard's job while Ready and idle is to notice a host running out of memory because of
  *other* processes. Its input is PSI `full avg10`, a ten-second average, so a 1–2 s sample does
  not materially change detection latency [S]. The refusal path (`MemorySample::admit`) is
  evaluated on request admission, not by the watcher, so a request is never admitted on a stale
  sample.
- A dead idle engine is caught by `try_wait` on the next wake; the 1–2 s delay changes only how
  fast a dead *idle* engine is noticed, and a request wakes the watcher immediately through the
  condvar, not through the timeout.
- Loading and generating keep 250 ms, so startup timeout, health and the guard behave exactly as
  today where they matter.

**What must be proven before it lands.** With the existing synthetic memory source and stub
process controls: low-memory detection still trips, refusal still trips, unhealthy detection still
trips within `unhealthy_limit`, shutdown stays bounded, and a request issued during idle is served
without waiting for the timeout. Each of those is a test that fails if the cadence is applied to
the wrong state.

## The 60 s idle record

The issue asks for before/after records that separate the four sources. Counters, not a profiler,
so the record is reproducible and cheap:

1. **Server signal-thread wakes** — a counter incremented in the shutdown thread (20/s before,
   0/s after).
2. **Client watcher wakes** — a counter in `watch()` (4/s before, 0.5–1/s after).
3. **procfs/cgroup samples** — the same counter, or `strace -c -f -e trace=openat,read` on the
   client for 60 s.
4. **Health probes** — the engine's request log, or the same `strace` count on the loopback port.

Record on the maintainer's laptop with a resident model, 60 s of no requests, before and after each
part. A stub runner cannot produce these numbers, and this note claims none of them.

## What this PR does not do

- No engine edit (needs engine-owner go), no oxa edit, no cadence change, no default or flag
  change, no numerics, no persistence-deadline change, no `oxc/` change.
- No measurement. The 20 Hz and 4 Hz figures are read from source [S]; the felt-speed gain is a
  hypothesis until the 60 s record exists.

## Order of work

1. This design (this PR).
2. Engine-owner go, then the Part 1 patch with the tests above.
3. The Part 2 oxa PR with the 60 s before/after record.
