# Install

## The portable Linux x86_64 binary

The `release-build` workflow builds one `oxa` binary for Linux x86_64 that does not depend on the distribution's newest C library. It is built on Debian 11 and needs glibc 2.31 or newer, so the same file starts on Debian 11 and newer, Ubuntu 22.04 and newer, the `python:3.12-slim` image, Arch and Fedora. It needs no Python, no controller and no other runtime library than glibc.

The workflow artifact is named `oxa-portable-linux-x86_64`. It holds `oxa` and `oxa.sha256`. Check the file before use:

```text
sha256sum -c oxa.sha256
```

Then put `oxa` anywhere on `PATH`, or run it by its full path, and check it:

```text
./oxa --version
```

oxa does not install a service, start a daemon or change files outside `~/.oxa`. Use the system packages described in the native install guide when you want `oxe-server` and the local engine as well; the portable binary is the agent only, for hosts that talk to an engine or a cloud model that is already configured.

## What CI proves

The job `portable_binary_starts_in_three_images` starts the binary in `debian:bullseye-slim`, `ubuntu:22.04` and `python:3.12-slim`. In each image it runs `oxa --version` and one `-p` prompt against the fake engine, and the reply must come back. The build step also fails when the binary needs a glibc symbol newer than 2.31 or links `libstdc++` or `libgcc_s`.

## Limits

- x86_64 only. ARM uses the Arch packages or a source build.
- Not statically linked: a system with a glibc older than 2.31, or a musl distribution such as Alpine, is not covered.
- The sandbox still needs `bwrap` from the host when you use the default permission mode.
