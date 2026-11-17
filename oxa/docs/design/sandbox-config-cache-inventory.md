# Sandbox config checks and child waits: source inventory

Status: source inventory only, for issue #880 (parent #864, owner O-383, milestone v0.2.2,
label `after-0.2.1`). **No runtime change, no cache, no supervision change, no security or
durability decision and no proposed implementation.** This document records what the code does
today so a later hard-tier architecture review starts from facts.

Every line reference is to commit `69a81515ad3ce39dbf048112fd0cfad476f4b73f` (current `main`).
The inventory is source-only: no build, benchmark, provider call or hardware run was performed
for it, and no timing claim below is a measurement.

## 1. What runs per sandboxed command

`oxa_sandbox::Bwrap` implements `oxa_proto::Sandbox`. Each of `run`, `run_streaming` and
`run_with_stdin` (`oxa/oxa-sandbox/src/lib.rs:136`, `:143`, `:154`) first calls
`bwrap::refusal(s, &self.home, &self.layout)` and then `launch` (`lib.rs:217`), which builds the
argv with `bwrap_args_with` (`oxa/oxa-sandbox/src/bwrap.rs:52`). `bwrap_args` is also reachable
on its own (for example `oxa/oxa-cli/src/dispatch.rs:2125`), so the argv path and the run path
each do their own checks.

The git safety boundary is therefore evaluated **per command**, not once per session. Inside one
`bwrap_args_homes` call (`bwrap.rs:57`) the config/hook checks are reached from:

- `configured_hook_paths` (`bwrap.rs:860`) → `gitdirs::configured_hooks_paths`
  (`oxa/oxa-sandbox/src/gitdirs.rs:811`) → `read_hooks_paths` (`gitdirs.rs:820`), which
  `fs::read`s and parses each `config` / `config.worktree` and every file they include
  (depth ≤ `MAX_INCLUDE_DEPTH` = 5, `gitdirs.rs:80`);
- `hooks_mounts` (`bwrap.rs:952`) → `gitdirs::unsafe_reason` (`gitdirs.rs:210`) per git
  directory;
- the `git_writable` decision (`bwrap.rs:212`), the cwd `.git` pin (`bwrap.rs:231`) and the
  protected-entry pass (`bwrap.rs:265`), each calling `unsafe_reason` again;
- `protect_hooks_paths` (`bwrap.rs:908`), which calls `hooks_mounts` and
  `configured_hook_paths` a second time.

Inside `refusal` (`bwrap.rs:623`) the same work repeats: `hooks_mounts` (`bwrap.rs:774`),
`unsafe_reason` per git dir, and `alias_refusal` (`gitdirs.rs:1159`) → `check_config`
(`gitdirs.rs:1199`) → `config_includes` (`gitdirs.rs:791`) for each config file, recursing into
every include.

`unsafe_reason` and `check_config` both call `config_includes`, which reads and parses the file
and then, when `git` is on `PATH`, cross-checks the parser against
`git config --file <p> --no-includes --null --get-regexp` (`gitdirs.rs:949` → `run_git_config`,
`gitdirs.rs:971`). So the number of `git config` subprocesses per command is a multiple of
(git directories × config files × the call sites above), plus one per included file reached
through `check_config`. Nothing in the current code counts or bounds that multiple; only the
gate below bounds concurrency and repeats after a timeout.

## 2. Current cache keys

There is **no content cache and no identity cache** for config files or git directories. The
only retained state is:

1. **`GitCheckGate`** (`gitdirs.rs:871`), a process-global `OnceLock` static (`git_gate`,
   `gitdirs.rs:934`). Its key is the repository's canonical common dir: `common_of`
   (`gitdirs.rs:941`) reads `commondir` when present, resolves it relative to the git dir and
   canonicalises, else canonicalises the git dir itself. Its values are, per key, a
   cooldown-until `Instant` and a "check running" flag (`GateState`, `gitdirs.rs:879`). It
   stores no result: a check that succeeds leaves nothing behind, so the next command re-reads
   and re-spawns.
2. **`Layout.git`** (`GitBinds`), resolved once when the session's sandbox is built
   (`lib.rs:82` → `gitdirs::resolve`, `gitdirs.rs:124`). This is a session-scoped list of git
   dirs, alternates and protected entries, not a config cache; the per-command checks above
   re-validate the protected entries against the current file system.
3. **Host-side placeholder files** created just before a run by `create_missing_git_files`
   (`bwrap.rs:567`, called from `lib.rs:140`, `:151`, `:164`): a missing `config`,
   `config.worktree` or `gitdir` is created as an empty file on the host so it can be bound
   read-only, while a missing `commondir` is created containing `.\n` (`bwrap.rs:604`–`:610`):
   git dies on an empty `commondir`, and `.` names the git directory itself, which is what an
   absent `commondir` means. These are durable side effects on the repository, not a cache,
   and the module comment (`gitdirs.rs:41`–`:43`) explains why removing them after the run is
   not sound.

## 3. Ownership

- `GitCheckGate` is owned by the process: one static instance per process, shared by every
  session and every command in it, guarded by a `Mutex` (`gitdirs.rs:873`). It is not shared
  across processes and has no on-disk form.
- `Layout` and `Layout.git` are owned by the `Bwrap` value that the session holds
  (`lib.rs:68`, `:82`).
- The per-command checks own nothing: they read the file system and return a refusal string or
  `None`.

## 4. Invalidation

- **Gate state:** `begin` (`gitdirs.rs:893`) refuses while a cooldown is active and refuses a
  second concurrent check of the same key; `begin_waiting` (`gitdirs.rs:911`) waits up to
  `GIT_CHECK_TIMEOUT * 2` (10 s) for a running check instead of refusing; `end`
  (`gitdirs.rs:925`) clears the running flag and, only on a timeout, sets the cooldown to
  `GIT_CHECK_COOLDOWN` = 60 s (`gitdirs.rs:866`). A cooldown entry is dropped on the next
  `begin` after it expires. A successful check invalidates nothing, because it stores nothing.
- **Config content:** nothing is invalidated, because nothing is remembered. Every command
  re-reads the bytes and re-runs the `git config` cross-check. The only identity facts read
  today are `symlink_metadata` (file type, and `nlink` for the hard-link refusal at
  `gitdirs.rs:291`, `:1208`, `:1263`), `fs::metadata` (`is_file`, `nlink`) and
  `fs::canonicalize`. `dev`, `ino`, `size`, `mtime` and `ctime` are **not** read anywhere in
  `oxa/oxa-sandbox/src/`; there is no `MetadataExt` use beyond `nlink`.
- **Session state:** `Layout.git` is fixed at session build; the per-command checks re-validate
  it, and a git dir that vanished is skipped (`bwrap.rs:204`, `:219`).

## 5. Existing child-wait paths

Two independent wait loops exist.

**`run::execute`** (`oxa/oxa-sandbox/src/run.rs:188`), used by every sandboxed and unsandboxed
command:

- one merged stdout/stderr pipe, read by a detached thread that sends chunks over an `mpsc`
  channel (`run.rs:233`);
- the supervisor loop (`run.rs:258`) blocks on `rx.recv_timeout(POLL)` with `POLL` = 20 ms
  (`run.rs:10`), then polls `child.try_wait()` (`run.rs:270`) and records `exited_at`;
- timeout or cancel sends `SIGTERM` to the process group (`signal_group`, `run.rs:176`), then
  `SIGKILL` after `KILL_GRACE` = 2 s (`run.rs:16`) or as soon as the group is gone
  (`group_alive`, `run.rs:183`);
- the loop ends on child exit only once `settled` and either EOF or `DRAIN` after the exit
  (`run.rs:292`–`:297`). Without a termination signal `settled` is true at once, so a
  still-open pipe (a background process holding it) bounds the wait after a normal exit at
  `DRAIN` = 200 ms (`run.rs:12`). During cancel or timeout `settled` is false until the hard
  kill (`run.rs:286`–`:291`), so ordinary-exit draining does not apply: a child that exits on
  `SIGTERM` while a surviving process holds the pipe open keeps the call inside the loop until
  `KILL_GRACE` = 2 s after `SIGTERM` (or until the child has exited and the whole group is
  gone, `group_alive`). The 200 ms `DRAIN` is therefore an ordinary-exit bound, not a
  whole-call bound once a termination signal was sent;
- output is bounded by `CAPTURE_CAP` = 16 MiB, head and tail halves with a dropped-byte count
  (`Capture`, `run.rs:125`);
- stdin, when given, is written from its own thread so a child that writes before it reads
  cannot deadlock the supervisor (`run.rs:225`);
- `die_with_parent` (`run.rs:322`) sets `PR_SET_PDEATHSIG` to `SIGTERM` between fork and exec
  and re-checks the parent pid.

**`run_git_config`** (`gitdirs.rs:975`) has its own loop, independent of `run::execute`:

- one deadline, `GIT_CHECK_TIMEOUT` = 5 s (`gitdirs.rs:861`), bounds the child's exit and the
  end of its stdout;
- the child is polled with `child.try_wait()` every 2 ms (`gitdirs.rs:1030`);
- stdout is read by a detached thread that is never joined (`gitdirs.rs:1019`), so a grandchild
  holding the pipe cannot block the caller; the read is capped at `GIT_CHECK_MAX_OUTPUT` = 1 MiB
  (`gitdirs.rs:961`);
- on expiry `hung` (`gitdirs.rs:1002`) sends `SIGKILL` to the process group, kills and waits for
  the child, and returns the timeout error that starts the 60 s cooldown;
- after a normal exit the group is `SIGKILL`ed too (`gitdirs.rs:1041`).

## 6. Test surface that any later change must keep passing

- `oxa/oxa-sandbox/tests/sbx_gitsym.rs` — the `sbx-gitsym` assertions (symlinked, hard-linked
  and included config, hooks, worktrees, writable caches).
- `oxa/oxa-sandbox/tests/sbx_gitsym_git_hang.rs` — `git_that_leaves_its_stdout_held_open_is_refused_within_the_deadline`:
  a fake `git` that exits but leaves its stdout held open must be refused, fail closed, well
  within 7 s.
- `oxa/oxa-sandbox/tests/git_worktree_binds.rs` — the `git_worktree_binds` assertions.
- `oxa/oxa-sandbox/tests/escape/git.rs` — the escape/git assertions, including
  `a_git_swap_after_start_is_rechecked` (`escape/git.rs:219`), which asserts that a git swap
  after session start is re-checked per command.
- `oxa/oxa-sandbox/tests/sbx_hooks.rs`, `oxa/oxa-sandbox/tests/a9_bwrap_sandbox_and_bash_tool.rs`
  and the in-crate tests of `gitdirs.rs` (`GitCheckGate` cooldown/concurrency, `gitdirs.rs:1378`,
  `:1405`, `:1425`) and `run.rs`.

## 7. Open questions for the later review (not a design)

Recorded only so the review does not have to rediscover them; no answer is proposed here.

- Which of the repeated `unsafe_reason` / `config_includes` call sites in one command are
  redundant, and what identity facts would have to be bound to a parsed result for a reuse to be
  sound (the current code reads no `dev`/`ino`/`size`/`mtime`/`ctime`).
- How a reuse would stay correct across rename, symlink retarget, `includeIf` context and
  environment changes, and concurrent mutation, given that the current design deliberately
  re-reads per command.
- Whether the 20 ms poll, the 200 ms drain, the 16 MiB capture cap, the 2 s kill grace and the
  5 s / 60 s git-check bounds are load-bearing for the assertions in section 6.
- What a measurement would have to record (median/p95 of 20 sandboxed `bash true` calls in a
  synthetic git worktree, plus the `git` subprocess count) before any speed claim is made; the
  5–25 ms figure in issue #880 is unmeasured.
