# Escape suite findings (task 60, part B)

Scope: the oxa bwrap sandbox, the permission-mode path policy and the MCP/`--offline` boundary.
Method: the adversarial suite in the three `tests/escape/` directories (see `README.md`), run
with a real bwrap in this session and in CI (`OXA_REQUIRE_BWRAP=1`). Reproducers are committed
tests, not scripts. Severity is judged from the threat model in `oxa/SECURITY.md`.

The suite is committed **before** the product fixes, in a separate commit: a reviewer can take
the tests alone and watch them fail, then take the fix commit.

---

## F1 — Critical where the host exposes an authority-bearing IPC service: host runtime IPC is reachable from bash

**Class:** sandbox escape (session D-Bus, container control) and `--offline` DNS bypass
(systemd-resolved). Security-sensitive; this finding describes the class and the test only.

**Reproducer:** `oxa-sandbox/tests/escape/ipc.rs`:
`the_user_session_bus_is_masked`, `the_host_resolver_is_masked`,
`container_control_sockets_are_masked`; `offline::offline_cannot_use_the_host_resolver`.

**Root cause:** `bwrap_args_with` does `--ro-bind / /`, so `/run` is visible read-only, and
`--unshare-all`/`--unshare-net` do not contain Unix sockets. The deny-list is home-only
(`sensitive_paths`). A sandboxed command can therefore connect to the user's session bus
(which starts units/processes outside the sandbox), the system bus, Docker/Podman control
sockets, and systemd-resolved — the last one resolves names even when the network namespace
is empty, so `--offline` and `oxa.sandbox.net: off` do not keep DNS in.

**Fix:** `bwrap.rs` masks `/run/user/<uid>`, `/run/systemd/resolve`, the two buses, the
container sockets and the common name-service sockets after every bind. The flag lives on
`Layout::mask_runtime_ipc`, set by `Bwrap::detect` and by the CLI's layout builder; pure-argv
tests keep the default off. The list is targeted rather than a tmpfs over `/run`, so a NixOS
`/run/current-system` still works.

**Risk / follow-up:** the list is curated; a new host IPC service under `/run` needs an entry.
`/run` remains visible for non-IPC files (as documented: `/` is read-only).

**Coverage / impact:** the service-specific tests print a skip notice and return when the host
socket is absent (a CI image often has no user session bus or container daemon), so a green
run does not prove those assertions ran. The impact is Critical where the host exposes an
authority-bearing service (the session/system bus, container control); it is conditional on
that authority, not universal.

---

## F2 — High: a symlinked `.git` lets a command plant hooks and config

**Reproducer:** `oxa-sandbox/tests/escape/git.rs::a_symlinked_dotgit_is_refused` and
`a_git_swap_after_start_is_rechecked`.

**Root cause:** the sandbox protects a real `.git` directory by binding it onto itself (a
mount point cannot be renamed) and then binding `hooks`/`config` read-only. A `.git` that is
a *symlink* was only checked by `gitdirs::resolve`, which returns no binds for it. The target
inside the cwd is part of the writable cwd bind, so renaming the target and recreating it
with a hook works; git outside the sandbox follows the symlink later.

**Fix:** `refusal` refuses a cwd whose `.git` is a symlink. The per-command check also sees a
swap made after the session's layout was built (the TOCTOU test).

**Risk:** a legitimate repository with a symlinked `.git` can no longer run bash commands;
git itself still works outside the sandbox. This is the fail-closed choice; binding and
pinning the target would need a full back-pointer validation of an arbitrary directory.

---

## F3 — High: a `.git` file naming a gitdir inside the cwd is not protected

**Reproducer:** `oxa-sandbox/tests/escape/git.rs::a_worktree_gitdir_inside_the_project_is_refused`.

**Root cause:** for a worktree `.git` *file*, `gitdirs::resolve` protects the gitdir only when
it lies outside the cwd (those dirs are bound read-write and their `hooks`/`config` are then
bound read-only). A crafted `.git` file that names a directory inside the cwd — a worktree in
the project itself, or a forged `gitdir:` line — leaves `gitdata/hooks` and `gitdata/config`
inside the ordinary writable cwd bind. Git outside the sandbox reads them. The same holds for a
real `.git` directory with a `commondir` file that names a writable directory: git reads hooks
and config from the common dir. A `..` in either target made a purely lexical check miss it.

**Fix:** `refusal` reads the `gitdir:` target of a `.git` file and the `commondir` target of a
real `.git` directory, normalises it lexically when it does not exist yet, and refuses when the
target is inside the writable cwd (or a layout write bind, or a home-workspace entry).
Canonicalisation judges a symlinked or existing relative target by where it really goes. A
`.git` file whose target `gitdirs::resolve` did not validate is refused too: its config could
name a hooks path inside the writable cwd even though the git directory itself is outside. A
real `.git` *directory* with a `commondir` is not the plain layout the sandbox protects (a
linked worktree has a `.git` file), so it is refused whatever the target is. A validated
worktree whose gitdir or common dir lies inside the writable cwd is refused too (only the
plain `.git` layout is self-bound). Reproducers:
`a_worktree_gitdir_inside_the_project_is_refused`,
`a_dotgit_file_with_dotdot_to_a_missing_gitdir_is_refused`,
`a_gitdir_with_a_commondir_inside_the_project_is_refused`,
`a_real_dotgit_with_an_external_commondir_is_refused`,
`a_worktree_common_dir_inside_the_project_is_refused`,
`an_unvalidated_external_dotgit_is_refused`.

**Risk:** an unusual but valid separate-git-dir/worktree layout inside the project, and a
`.git` file that does not pass the sandbox's worktree validation, are refused for bash. Git
still works outside.

---

## F4 — High: `core.hooksPath` points at a writable project directory

**Reproducer:** `oxa-sandbox/tests/escape/git.rs::the_configured_hooks_path_is_read_only`
and `a_hooks_path_in_an_included_config_is_protected`.

**Root cause:** the sandbox protected `.git/hooks` and `.git/config`, but git runs the hooks
in `core.hooksPath` when a repository config sets it (common: `.githooks`, `.husky/_`). The
directory is ordinary project content in the writable cwd. The config parser only followed
`include.path` for the symlink/alias check; it did not extract `core.hooksPath`, so an
included config could hide it too.

**Fix:** `gitdirs::configured_hooks_paths` extracts `core.hooksPath` from `config` and
`config.worktree`, following `include`/`includeIf` files (depth-limited, cycle-safe).
`bwrap.rs` normalises the value (`..`, `~/`) before the containment check, binds each such
directory that lies in a writable root read-only, or masks it with a read-only tmpfs when it
does not exist yet. A relative value is resolved from the worktree root (the directory that
holds the `.git` entry, recorded in `GitBinds::worktree`), never from the session cwd: a
session in a nested directory would otherwise derive a different path and leave the real hook
directory writable. A symlinked path or a missing parent is refused instead of guessed at. It
runs in plan mode too, where caches are still writable.
Reproducers: `a_hooks_path_with_dotdot_is_normalised`,
`a_hooks_path_in_a_writable_cache_is_read_only_in_plan_mode`,
`a_nested_cwd_resolves_a_relative_hooks_path_from_the_worktree_root`; the nested-cwd control
`a_nested_cwd_does_not_protect_a_hooks_path_derived_from_the_session_cwd` shows that a
legitimate nested session is not over-refused.

**Risk:** a missing hooks directory is created empty on the host as the mount point (the same
documented side effect as a missing `.git/hooks`). A `core.hooksPath` in the user's global
`~/.gitconfig` is not parsed; it points into the user's home (hidden or masked here) unless
the user deliberately set it to a project path.

---

## F5 — High: the file tools could write git metadata (`.git` pointer, `config.worktree`)

**Reproducer:** `oxa-perm/tests/escape/main.rs::git_metadata_is_never_written`.

**Root cause:** `GIT_PROTECTED_IN_CWD` listed `.git/hooks` and `.git/config` only. A worktree's
`.git` file (which names the gitdir) and `.git/config.worktree` (which can set `core.hooksPath`
when `extensions.worktreeConfig` is on) were allowed writes inside the cwd in `workspace` mode.

**Fix:** `.git/config.worktree` joins the never-write list; `protected_write` also returns
never-write for the `.git` entry itself. The bash sandbox already bound a worktree's `.git`
file read-only and masked `config.worktree` when the extension was on.

**Risk:** none expected for tools; a user editing git internals by hand is unaffected.

---

## F6 — High: `--offline` used a remote MCP proxy

**Reproducer:** `oxa-mcp/tests/escape/main.rs::offline_refuses_a_remote_proxy`,
`offline_refuses_an_ambient_remote_proxy_when_not_excluded`, and the positive control
`offline_allows_a_loopback_target_excluded_by_no_proxy`.

**Root cause:** `HttpTransport::new` installed `httpProxy` (or ureq's environment proxy) and
`post` checked offline only against the target URL. A loopback target passes the gate, but the
connection (and the request, including the bearer token) goes to the proxy's host.

**Fix:** offline, the transport classifies the effective route per request: a proxy whose host
is not loopback is refused in `post` with the standard offline error, unless ureq's `NO_PROXY`
rules route the target direct (a target on the proxy's exclusion list is not sent to the
proxy). A loopback proxy is still allowed. The OAuth agent runs direct offline. The error
names the proxy host, never a password embedded in the proxy URL.

**Risk:** offline users with a remote ambient proxy and a target it would carry get a clear
refusal instead of a direct connection; that is the intended policy (the switch says no host
other than loopback). An excluded loopback target keeps working, as it would without the proxy.

---

## F7 — High: `--offline` followed an MCP redirect to a remote host

**Reproducer:** `oxa-mcp/tests/escape/main.rs::offline_refuses_a_redirect_to_a_remote_host`.

**Root cause:** ureq follows up to 10 redirects by default. The offline gate checked the first
URL only, so a loopback MCP endpoint could answer `307 Location: http://remote/` and the
client connected to the remote host.

**Fix:** offline, the HTTP transport and the OAuth agent set `max_redirects(0)`, and a `3xx`
answer is turned into the one offline refusal; the redirect target is never read. Online
behaviour is unchanged.

**Risk:** an MCP server that legitimately redirects while offline is refused; the protocol
expects a direct endpoint.

---

## Attempts that failed as designed (no finding)

These are in the suite as green tests: symlinked and hard-linked credentials, `..`/absolute
writes, host PIDs and `/proc/1/root`, mount/mknod/sysctl writes, dotfiles in a home workspace,
parent-environment leakage, secret-shaped toolchain variables, control-character file names,
a grant swapped for a symlink, a remote `readOnlyHint` without opt-in, and a project
`mcp.json` override trying to set `trustAnnotations`.

## Residual risks (not fixed here)

- **A pre-existing hard link** inside the project to a credential is indistinguishable from
  the credential by path. The sandbox stops a command from *creating* one (separate mounts
  give `EXDEV`), but a workspace delivered outside git can already contain one.
- **Global git config** (`~/.gitconfig`, `~/.config/git/config`) is not parsed for
  `core.hooksPath`; the repository config and its includes are.
- **`PROTECTED_IN_HOME` is curated.** A desktop/terminal config outside the list (for example
  a status-bar config that can run commands) is writable by the file tools when the cwd is the
  home directory, with a prompt only for the listed ones. This is a list-maintenance risk, not
  a bypass of a check.
- **TOCTOU inside one file-tool call** (policy check, then open) remains a process-local race;
  a model-controlled writer cannot run concurrently with the tool call under bwrap, and grants
  re-resolve the current path. The suite pins the grant and per-command re-check behaviour.
