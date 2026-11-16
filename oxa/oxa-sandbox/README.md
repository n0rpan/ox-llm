# oxa-sandbox

bwrap sandbox behind `oxa_proto::Sandbox`. Depends on `oxa-proto`, `libc`. Used by `oxa-core` (handed to tools through `ToolCtx`), `oxa-ext` (exec hooks, `exec` host function), `oxa-mcp` (stdio servers).

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | `Bwrap`, `NoSandbox` |
| `bwrap.rs`, `run.rs` | A9 (with `oxa-tools/src/bash.rs`) | `bwrap_args`, `run_argv`, `run_unsandboxed`, `Bwrap::detect`, `Bwrap::check` |

Contracts: see the crate docs. Tests that need a real bwrap skip when `Bwrap::detect` fails and print why (a failure under `OXA_REQUIRE_BWRAP=1`, see below); argv goldens always run.

Captured `gh issue view --comments` output follows gh's redirected-output contract:
comments only, so an issue with no comments produces no output. To get the issue
body **and** comments reliably, use the machine-readable form (without `--comments`):

```sh
gh issue view NUMBER -R OWNER/REPO --json title,body,comments,state
```

For a human-readable view, scope terminal formatting to that invocation only:

```sh
GH_FORCE_TTY=80 GH_PAGER=cat NO_COLOR=1 gh issue view NUMBER -R OWNER/REPO --comments
```

stdin/stdout remain pipes; this does not enable interactive input. The sandbox does
not set these three variables globally: `GH_FORCE_TTY` changes list/API formatting
and truncates long table cells, while `NO_COLOR` affects every tool that respects it,
not just gh. Ordinary `gh issue list | cut -f1` pipelines keep their raw tab-separated
output. The gh regression test requires gh on PATH and uses only a synthetic API
on a workspace Unix socket, with no network or real credentials.

## What bash sees of the machine (wave H lane `harden`, pre-flight B2 / B3 / B4)

`Bwrap::layout` (`src/layout.rs`, built by the CLI from the user's environment and the USER
settings only; a project file cannot widen the sandbox). The argv stays deterministic: binds and
masks are sorted by path.

| What | Inside the sandbox | Why |
|---|---|---|
| `/` | read-only | the system toolchains (`/usr/bin/cargo`, `python3`, ...) |
| `$HOME` | empty read-only tmpfs | nothing of the home is visible unless listed below; writes outside the exposed binds fail with `Read-only file system` (or a missing-path error) rather than silently disappearing |
| `/tmp` | the session directory (#457): `~/.cache/oxa/scratch/<pid>-<n>` on the host, mode 0700, created when a session starts or is reopened and bound at `/tmp` for every command of that session. It is deleted when the session ends (`/new`, `/resume`, fork, clone, any other session switch, oxa exit), so a resumed session starts with an empty `/tmp` and two sessions never share one; a directory left by a killed oxa is removed when a later oxa creates its next session directory. Hooks (no session) and a session whose directory cannot be created get a private tmpfs per command | scratch for cargo, git and compilers that survives between bash calls; `TMPDIR=/tmp` and `XDG_CACHE_HOME=/tmp/.cache` give every tool a writable cache home, and each read-write entry of the user's cache home (`$XDG_CACHE_HOME` when absolute, else `~/.cache`; sccache, pip, ...) is linked from `/tmp/.cache/<name>` so it stays shared. With a custom `XDG_CACHE_HOME`, `~/.cache` entries are not linked. Not a place to save deliverables. Use the workspace for persistent output. Startup and tool guidance explain this limit; bash results append a short reminder only when the command mentions `/tmp` or `TMPDIR` (case sensitive), or fails with a read-only/permission error. Commands' raw output (including hook protocols) stays unchanged. |
| `PATH` | the user's own, filtered to absolute directories that exist (order kept), plus `/usr/local/bin:/usr/bin:/bin` when missing | `uv`, `mise` shims, `~/.cargo/bin` tools resolve as in the user's shell |
| `PATH` entries under `$HOME` | read-only | the tools on the user's `PATH` run |
| `~/.cargo/bin`, `~/.cargo/config.toml`, `~/.cargo/config`, `~/.rustup` | read-only | rustup proxies, toolchains, cargo config |
| `~/.local/bin`, `~/.local/share/mise`, `~/.config/mise`, `~/.tool-versions`, `~/.asdf` | read-only | mise / asdf installs, shims and their configuration |
| `~/.nvm`, `~/.local/share/fnm`, `~/.fnm`, `~/.volta`, `~/.bun`, `~/.deno` | read-only | node / bun / deno toolchains |
| `~/go/bin`, `~/.local/share/pipx`, `~/.local/pipx`, `~/.local/share/uv`, `~/.pyenv`, `~/.rbenv` | read-only | go binaries, pipx / uv tools and pythons, pyenv / rbenv |
| `~/.gitconfig`, `~/.config/git` | read-only | the user's git identity for commits |
| `~/.cargo/registry`, `~/.cargo/git` | read-write | cargo must download and unpack crates; without it every call re-downloads into a throwaway home |
| `~/.cache/{pip,uv,go-build,sccache,yarn,pnpm,deno,node-gyp}`, `~/.npm`, `~/.bun/install/cache`, `~/go/pkg/mod` | read-write | the package caches of pip, uv, go, sccache (or `$SCCACHE_DIR` when set, judged by its resolved path: only if it lies under `~/.cache`, or entirely outside the home and not in a system directory such as `/`, `/etc`, `/usr` or `/var`, nor a parent of the home; anything else in the home, e.g. `~/.config/sccache`, is never made writable), yarn, pnpm, deno, node-gyp, npm, bun |
| `oxa.sandbox.readPaths` / `oxa.sandbox.writePaths` (user settings, `~/` expanded) | read-only / read-write | anything else a user's toolchain needs |
| Toolchain variables (`CARGO_HOME`, `RUSTUP_HOME`, `GOPATH`, `GOMODCACHE`, `NVM_DIR`, `npm_config_cache`, `UV_CACHE_DIR`, `MISE_*`, `XDG_*`, ...) | passed when set | they say where the binds above are |
| Any variable whose name holds `TOKEN`, `SECRET`, `KEY`, `PASSWORD`, `PASSWD`, `CREDENTIAL`, `AUTH`, `COOKIE` or `SESSION` | never passed | credentials stay outside |
| The cwd | read-write (read-only in plan mode) | the project |
| The cwd when it is `/` or a parent of `$HOME` | read-only, whatever the settings; a startup warning says so | never a sensible project root |
| The cwd when it is `$HOME` | read-only (a startup warning says so), unless the user confirmed the interactive start there or the user settings say `oxa.sandbox.allowHomeWrite: true` (O-199): then the home root, every dot-entry, symlinks and `~/bin` stay read-only, its plain folders and files are bound read-write, and a startup notice says "workspace = your home directory" | the workspace follows where oxa starts, while nothing that runs at login or holds keys can change |
| `.oxa`, `.agents/skills`, `.git/hooks`, `.git/config` inside a writable cwd (when they exist) | read-only | they run outside the sandbox later (MCP servers, extensions, hooks, git) |
| The deny-list: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.kube`, `~/.docker/config.json`, `~/.netrc`, `~/.git-credentials`, `~/.config/git/credentials`, `~/.config/gh`, `~/.config/hub`, `~/.config/oxc`, `~/.local/state/oxc`, the agent directory (`~/.oxa/agent`), `~/.pi/agent`, `~/.npmrc`, `~/.pypirc`, `~/.cargo/credentials{,.toml}`, `~/.vault-token`, `~/.terraform.d/credentials.tfrc.json`, `~/.password-store`, `~/.local/share/keyrings`, `~/.config/op`, `~/.config/Bitwarden`, `~/.pki`, browser profiles (`~/.mozilla`, `~/.thunderbird`, `~/.config/{google-chrome,chromium,BraveSoftware,microsoft-edge,vivaldi,opera}`) | masked after every bind: an empty read-only tmpfs over a directory, `/dev/null` over a file | keys, tokens, trust, other projects' sessions; neither readable nor writable whatever the cwd or the settings. The file tools obey the same list (`oxa-perm` `PathGuard`) |

A writable cache is shared with the user's own builds: what the model writes there (an unpacked
crate under `~/.cargo/registry/src`, a module under `~/go/pkg/mod`, a compiled object in `~/.cache/sccache`) is what the next build of
the user reads. Clear a cache after a session you do not trust; a setting that binds the caches
read-only is a follow-up.

Real-bwrap tests: with `OXA_REQUIRE_BWRAP=1` (CI sets it) an unusable bwrap fails them;
without it they are skipped with a notice that cargo shows (written straight to stderr) and a
summary test (`a9::bwrap_skips_are_reported`) counts them.
