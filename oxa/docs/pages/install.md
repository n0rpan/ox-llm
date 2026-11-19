# Install

## The portable Linux x86_64 binary

The `release-build` workflow also builds one `oxa` binary for Linux x86_64 that does not depend on the distribution's newest C library. It is built on Debian 11 and needs glibc 2.31 or newer, so the same file starts on Debian 11 and newer, Ubuntu 22.04 and newer, the `python:3.12-slim` image, Arch and Fedora. It needs no Python, no controller and no other runtime library than glibc.

The workflow artifact is named `oxa-portable-linux-x86_64`. It holds `oxa` and `oxa.sha256`. Check the file before use:

```text
sha256sum -c oxa.sha256
```

Then put `oxa` anywhere on `PATH`, or run it by its full path, and check it:

```text
./oxa --version
```

The portable binary is the agent only, for hosts that talk to an engine or a cloud model that is already configured.

### What CI proves

The job `portable_binary_starts_in_three_images` starts the binary in `debian:bullseye-slim`, `ubuntu:22.04` and `python:3.12-slim`. In each image it runs `oxa --version` and one `-p` prompt against the fake engine, and the reply must come back. The build step also fails when the binary needs a glibc symbol newer than 2.31 or links `libstdc++`.

### Limits

- x86_64 only. ARM uses the release archive or the Arch packages above.
- Not statically linked: a system with a glibc older than 2.31, or a musl distribution such as Alpine, is not covered.
- The sandbox still needs `bwrap` from the host when you use the default permission mode.

The ox suite ships as one release: `oxa` (the agent), and `oxe` with `oxe-server` (the engine for local models). There is no Python, no service and no daemon to install. This page covers the stable release, the dev build, a build from source, and updating or removing an install.

## What you need

- Linux on ARM64 or x86_64, with glibc.
- `bubblewrap` (`bwrap`) and unprivileged user namespaces, for the bash sandbox.
- For local models: a Snapdragon X laptop under Linux. The x86_64 build carries the agent and an engine that runs no model, so use cloud models there.
- macOS (Apple silicon) support is in progress: cloud models only, no OS sandbox, and no published build yet. See [macOS](#macos) below.

## Stable release

Each release on the project's Releases page attaches, for each architecture:

- `ox-llm-<version>-linux-aarch64.tar.gz` and `ox-llm-<version>-linux-x86_64.tar.gz`;
- `SHA256SUMS`, an SBOM (`.sbom.cdx.json`) and build provenance.

Download the archive for your machine and `SHA256SUMS` from the same release, then check them:

```console
$ sha256sum -c SHA256SUMS --ignore-missing
$ gh attestation verify ox-llm-<version>-linux-aarch64.tar.gz --repo oxstack/ox-llm
```

The checksum shows that the download is intact. The attestation shows that this repository's release workflow built it.

The archive holds one `usr/` tree. Extract it into a fresh directory of its own:

```console
$ mkdir ~/ox-llm-<version>
$ tar -xzf ox-llm-<version>-linux-aarch64.tar.gz -C ~/ox-llm-<version>
$ ~/ox-llm-<version>/usr/bin/oxa --version
$ ~/ox-llm-<version>/usr/bin/oxa --help
```

Put `~/ox-llm-<version>/usr/bin` on your `PATH`, or link `oxa` from a directory that is already on it. Keep `oxa`, `oxe` and `oxe-server` together in that `bin` directory: oxa looks for the engine beside itself, does not search `PATH` for it, and refuses an engine from another build.

Installing writes no settings, starts nothing and downloads no model. Go on with [quickstart.md](quickstart.md).

### Arch Linux

There is no published Arch package. The recipes are in `packaging/arch/`: `ox-llm-bin` wraps the release archive, and `ox-llm` builds from the source archive. Put the `ox-llm-bin` recipe beside the archive you verified, then build and install the package:

```console
$ export OX_BINARY_SHA256=$(sha256sum ox-llm-<version>-linux-$(uname -m).tar.gz | cut -d' ' -f1)
$ makepkg --cleanbuild
$ sudo pacman -U ./*.pkg.tar.zst
```

The package depends on `bubblewrap`. It creates no user, starts no service and downloads no model. [RELEASE.md](../../../docs/RELEASE.md#build-arch-packages-locally) has the full steps for both recipes.

## Dev build

Every push to `main` publishes a rolling `dev` prerelease, built with the same recipe as a release. It is never marked as the latest release.

```console
$ tools/lab/dev-update.sh
$ ~/.local/state/oxc/dev-builds/default/bin/oxa --version
```

The script needs Linux, Bash, `curl`, `jq`, `tar` and the GitHub CLI (`gh`). It uses no login. It checks the sums and the provenance against this repository's `main` and its dev workflow before it switches to the new build. It keeps the current build and the one before it, and it changes no stable install, no service and no `PATH`. The details are in [DEV.md](../../../docs/DEV.md).

Use the dev build to try what is on `main`. Use the stable release for everything else.

### macOS

`tools/lab/dev-update-macos.sh` is the same installer for macOS on Apple silicon. It needs `gh` (`brew install gh`) and installs to the same place under your home directory. No macOS build is published yet, and `main` does not build on macOS yet, so the script has nothing to install until the dev asset exists.

On macOS there is no local engine and no OS sandbox. Read [modes.md](modes.md#macos) before the first run.

## From source

You need a Rust toolchain; the hosted checks pin Rust 1.99.1. From the `oxa/` directory of a checkout:

```console
$ cargo build --release --locked -p oxa-cli
$ ./target/release/oxa --version
```

The engine builds from the `engine/` directory:

```console
$ cargo build --release --locked -p oxe-cli --bin oxe -p server --bin oxe-server
```

For local models, copy `oxe` and `oxe-server` next to `oxa`, or name their directory with `oxa local add --engine-dir <dir>`. All three must come from the same commit.

## Update

oxa does not update itself. Install the new release beside the old one and switch your `PATH`, or run `tools/lab/dev-update.sh` again for the dev build. Your settings, sessions and keys are not part of an install and stay where they are.

`oxa update` is a different thing: it updates the packages (extensions and skills) that you installed with `oxa install`.

## Upgrade from v0.2.0

v0.2.0 ran a Python controller. v0.2.1 does not, and it does not use the controller's address. Stop the old runtime first, install v0.2.1 as above, and start `oxa` directly. Your provider keys and your ChatGPT sign-in are reused where they are; sessions and caches may start cold. The step-by-step guide is [NATIVE-INSTALL.md](../../../docs/NATIVE-INSTALL.md).

## Remove

Delete the directory you extracted, or remove the package with `sudo pacman -Rns ox-llm-bin`. The dev build is the directory `~/.local/state/oxc/dev-builds/`.

Your own data is separate, and removing an install does not touch it:

| Path | What |
|---|---|
| `~/.oxa/agent/` | settings, sessions, skills, the cloud ledger |
| `~/.config/oxc/llm/` | provider keys and the ChatGPT token |
| `<project>/.oxa/` | project settings and resources |

`oxa wipe` removes stored sessions; see [sessions.md](sessions.md#what-oxa-stores-and-how-to-remove-it).
