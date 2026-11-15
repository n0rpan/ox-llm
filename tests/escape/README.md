# Escape suite: task 60, part B

The suite is adversarial: every test is a concrete attempt to leave the oxa bwrap sandbox or
to widen a permission mode, and every test states the threat, the attempt and the expected
refusal in its doc comment. A test that is green means the attempt failed.

Cargo only builds integration tests inside a package, so the runs live next to the crates they
exercise (each is a `tests/escape/` directory with a `main.rs` entry point):

| Crate | Command | What it covers |
|---|---|---|
| `oxa-sandbox` | `cargo test -p oxa-sandbox --test escape` | bwrap: symlinks, hard links, `..`, absolute paths, `/proc`, mounts and namespaces, git hooks/config, dotfiles, env, file names, host runtime IPC, offline bash |
| `oxa-perm` | `cargo test -p oxa-perm --test escape` | the file-tool policy: `..`, absolute paths, symlinks, git metadata, `.oxa`, grants vs a later symlink (TOCTOU), remote-MCP class, control characters |
| `oxa-mcp` | `cargo test -p oxa-mcp --test escape` | `--offline` at the MCP boundary (remote proxy, redirect, remote URL), loopback control, the remote `readOnlyHint` permission claim |

`tests/escape/FINDINGS.md` lists the bugs the suite found, each with its reproducer and the
fix. The CI job `oxa tests (bwrap sandbox required)` runs the whole workspace with
`OXA_REQUIRE_BWRAP=1`, so a missing sandbox cannot silently skip these tests.

## Threat → test

| Threat class | Attempt | Expected refusal | Test |
|---|---|---|---|
| Symlink out of the workspace | `cat link/id_test` where `link -> ~/.ssh` | home is a tmpfs/masked; `cat` fails | `oxa-sandbox` `credentials::symlinks_to_credentials_do_not_leave_the_workspace` |
| Hard link out of the workspace | `ln .git/config .git/config.alias`, `ln /etc/passwd` | protected files are separate mounts; `link` fails (`EXDEV`) | `oxa-sandbox` `fs_paths::hard_links_cannot_alias_a_protected_file` |
| `..` and absolute paths | `printf x > ../x`, `/etc/x`, `/tmp/x`, through a `/` symlink | host unchanged; `/etc` and `/` are read-only | `oxa-sandbox` `fs_paths::dotdot_and_absolute_writes_stay_out`; `oxa-perm` `dotdot_is_resolved_before_the_check`, `absolute_paths_are_outside_or_denied` |
| `/proc/self` and `/proc/<pid>` | `test -e /proc/<host pid>`, `/proc/<pid>/environ`, `/proc/1/root<home>/.ssh` | new PID namespace + private procfs | `oxa-sandbox` `namespace::host_processes_are_not_in_proc` |
| Git hooks/config (`hooksPath`, `fsmonitor`, `includeIf`) | write `.git/hooks`, `.git/config`, a configured `core.hooksPath`, an included hooks path, a symlinked `.git`, a `.git` file naming a writable gitdir | read-only/refused; a missing entry cannot be created | `oxa-sandbox` `git::*`; `oxa-perm` `git_metadata_is_never_written` |
| Dotfiles and credentials | read `~/.ssh`, `~/.aws`, `~/.config/oxc/cloud-secrets.json`, `~/.netrc`, the agent dir; write dotfiles in a home workspace | deny-list masks; write refused | `oxa-sandbox` `credentials::*`; `oxa-perm` `symlinks_to_secrets_are_denied`, `project_config_asks_and_plan_denies` |
| Env var leakage into tools | `printenv` a parent canary; build a layout with `MISE_GITHUB_TOKEN` | `--clearenv`; secret-shaped toolchain names dropped | `oxa-sandbox` `env::*` |
| TOCTOU between check and use | swap a granted directory for a symlink; swap `.git` after the sandbox is built | grants resolve the current path; the per-command re-check refuses | `oxa-perm` `a_grant_does_not_follow_a_later_symlink`; `oxa-sandbox` `git::a_git_swap_after_start_is_rechecked` |
| File names with newlines/control characters | read/write `line\nbreak`, `x; touch PWNED`, `-rf`, `\x07`; cwd with a newline | one argv element / one shell word; no injected command | `oxa-sandbox` `filenames::*`; `oxa-perm` `a_control_character_is_a_name_not_a_traversal` |
| Mount/namespace escapes | `mount -t tmpfs`, `mknod`, write `/proc/sys`, `/sys`, `/dev/mem` | `--cap-drop ALL`, read-only mounts, private `/dev` | `oxa-sandbox` `namespace::*` |
| `--offline` bypasses (DNS) | `resolvectl status`, `getent hosts`, raw TCP | host resolver and network are gone | `oxa-sandbox` `ipc::the_host_resolver_is_masked`, `offline::offline_bash_has_no_network`; `oxa-mcp` offline tests |
| `--offline` bypasses (proxies) | remote `httpProxy`/env proxy, a loopback URL redirected to a remote host | the proxy is refused; redirects are not followed offline | `oxa-mcp` `offline_refuses_a_remote_proxy`, `offline_refuses_a_redirect_to_a_remote_host` |
| `--offline` bypasses (MCP) | remote HTTP URL | one offline error before connect | `oxa-mcp` `offline_refuses_a_remote_mcp_url` |
| Host runtime IPC | user D-Bus, systemd-resolved, Docker/Podman sockets under `/run` | masked (these are not network sockets) | `oxa-sandbox` `ipc::*` |
| A remote MCP widening permissions | remote `readOnlyHint: true`; a project override trying `trustAnnotations` | remote hint is `Write`; a project override cannot trust it | `oxa-mcp` `a_remote_read_only_hint_is_not_trusted_by_default`, `a_project_override_cannot_trust_annotations`; `oxa-perm` `a_remote_mcp_write_is_not_allowed_in_plan` |

## Running locally

```console
cd oxa
cargo test -p oxa-sandbox --test escape
cargo test -p oxa-perm --test escape
cargo test -p oxa-mcp --test escape
```

On a machine where bwrap cannot create a user namespace the sandbox tests print
`escape SKIPPED: ...` and return; set `OXA_REQUIRE_BWRAP=1` to turn that into a failure.
