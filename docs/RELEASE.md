# v0.2.0 release qualification

`release-build.yml` only builds workflow artifacts. It does not create tags,
GitHub releases, registry entries or AUR uploads. It needs no secrets and has
`contents: read`. Its only triggers are manual dispatch and PR changes to this
workflow, `packaging/**` or this document. Draft PRs skip the artifact builds while
changes are iterated locally. Marking a matching PR ready, or a manual dispatch,
enables them. Ordinary PR checks remain independent.

The build workflow, Arch recipes and model-free version reporting are part of
`main`. Follow-up PRs stay as drafts during v0.2.0 release mode: validate locally
and use the cheap draft checks. Do not wait for a review or mark a PR ready to
trigger full CI; the owner coordinates qualification of the release commit.

## What the build checks

Native GitHub-hosted `ubuntu-24.04-arm` and `ubuntu-latest` runners build the CPU
`oxe`, `oxe-server` and `oxa` release binaries, with Fedora 44/GCC 16 and pinned
Rust 1.99.1. ARM uses the documented `target-cpu=oryon-1` and the unchanged vendored
kernel flags `armv8.6-a+dotprod+i8mm+fp16+bf16+nosve+nosme`. x86_64 uses the baseline
`x86-64` CPU target; its model engine is unsupported, but its server stub works.
No model weights, NPU device features, Flash external libraries or vendor SDK
are included. These builds make no inference speed or hardware qualification claim.

The native-default payload ships `oxa`, `oxe` and `oxe-server`, with `oxc` as a
small compatibility launcher for oxa. It does not ship the Python controller,
Python-backed units, panel payload, diagnostics launcher or experimental controller
parser binaries. See [native installation](NATIVE-INSTALL.md) and its activation gate.
The v0.2.0 release remains the legacy install; the Python source is retained in the
repository for a separate retirement-deletion review. Runtime Rust dependency
license texts and the vendored ggml license are still collected by the existing
license collector. Package installation starts no service, configures no user
account, reads no credential and downloads no model.

Each architecture produces a tarball containing a standard `usr/` tree, its exact
source archive, `SHA256SUMS`, `SBOM.cdx.json`, `TOOLCHAIN.txt`, and
`REPRODUCIBILITY.txt`, retained as workflow artifacts for seven days.
The CycloneDX 1.6 SBOM inventories Cargo's locked workspace resolution (including
build/test dependencies and registry source checksums), and every shipped file's SHA-256. It is an inventory, not a claim of
vulnerability clearance or a minimal runtime dependency graph.

Two fresh builds use different source and target directories. Their complete
payload tarballs must compare byte-for-byte. Rust and C/C++ source paths are
remapped, incremental compilation is off, release debug info is omitted, and
symbols are stripped. Tar order, ownership, timestamps and gzip headers are fixed;
`SOURCE_DATE_EPOCH` is the source commit timestamp. A byte scan rejects builder
source/toolchain/home prefixes in installed commands. No builder identity is
inserted in package files. Repeatability is demonstrated within one workflow run;
Fedora/Arch package mirrors and `archlinux:latest` move over time, so the saved
toolchain versions are essential and reproducibility across dates is not promised.

The fresh x86_64 `archlinux:latest` job builds both the binary package and the
source package, runs the source package's model-free test subset, invokes namcap,
installs each package, checks that Python payload/units are absent and legacy features return their disabled notice, runs native `--version`
commands and CLI help, validates
`oxe --check-args`, and sends a real loopback completion through `oxe-server`'s
stub engine. It checks package integrity, uninstalls and verifies every package
file and package-specific directory is gone. The two Arch packages, their
checksums and namcap logs are retained as a separate seven-day workflow artifact.
Python is a source-package build dependency, not a runtime dependency. Panel and
Python controller modules are absent from the default payload. Bubblewrap is
invoked dynamically, so namcap may report its declared runtime dependency as unused. Errors fail CI. Tests use a fresh HOME and an
allowlisted environment with no cloud credentials. The official Arch Linux Docker
image currently has no ARM64 variant. ARM binaries are natively built and their
model-free source tests run on ARM; a clean ARM Arch install remains unavailable
until an appropriate image is supplied and separately reviewed. No emulation or
maintainer laptop is needed for the existing gates.

## Run and inspect, without publishing

When the owner requests release qualification, dispatch **release-build** on the
selected branch and record the run's immutable source commit. The workflow is
already on `main`; the Actions UI and CLI can dispatch it there. A qualification
run can also target a specific follow-up branch without changing PR draft state.
For example, to qualify current `main`:

```sh
gh workflow run release-build.yml --repo oxstack/ox-llm --ref main
```

The path-filtered PR trigger exercises the same build and install gates.
Do not bypass a required maintainer approval.

Download each architecture's artifact from that run, into separate directories.
Check the workflow conclusion, source commit, and both platform jobs. In each
artifact directory run:

```sh
sha256sum -c SHA256SUMS
```

Verify the manifest came from the intended workflow run and commit. Checksums
detect corruption; an unsigned checksum beside a downloaded file does not by
itself authenticate its publisher. Inspect the SBOM, toolchain record and
reproducibility result as well as the tarball's file list before installing.

## Build Arch packages locally

Run makepkg as an unprivileged user in a fresh directory. Install the PKGBUILD's
build dependencies first. Copy the source archive from the verified artifact and
`packaging/arch/ox-llm/PKGBUILD` into that directory. Set these to the inspected
workflow's source identity and commit timestamp (use literal values):

```sh
export OX_SOURCE_COMMIT=FULL_SOURCE_COMMIT
export SOURCE_DATE_EPOCH=SOURCE_COMMIT_UNIX_TIMESTAMP
export OX_SOURCE_SHA256=$(sha256sum "ox-llm-$OX_SOURCE_COMMIT.tar.gz" | cut -d' ' -f1)
makepkg --cleanbuild
namcap PKGBUILD ./*.pkg.tar.zst
```

The source checksum must first have passed `SHA256SUMS` verification. Recipes have all-zero checksum/commit defaults solely so namcap can parse them
with its sanitized environment; those defaults cannot validate any real input.
No recipe uses `SKIP` checksums, a mutable branch archive, or a network installer. The local
source artifact is mandatory; there is intentionally no release URL before the
owner publishes a release. On ARM install GCC 16 or newer and the pinned Rust
version before building; do not replace the documented CPU flags with `native`.

For the binary variant use `packaging/arch/ox-llm-bin/PKGBUILD` beside the verified
`ox-llm-0.2.0-linux-ARCH.tar.gz` artifact:

```sh
export OX_BINARY_SHA256=$(sha256sum "ox-llm-0.2.0-linux-$(uname -m).tar.gz" | cut -d' ' -f1)
makepkg --cleanbuild
namcap PKGBUILD ./*.pkg.tar.zst
```

Install the resulting package with `pacman -U`; remove it with `pacman -Rns`.
Use pacman to remove package files. The product's user-runtime uninstaller manages
its separately staged user install and is not a system package uninstaller.
Persistent user data created during actual use is not removed by pacman.

## Owner-only real release, later

1. Select the release commit from `main` and inspect its integration checks. Keep
   follow-up work in draft PRs during release mode; this guide does not require
   those PRs to be reviewed or merged before qualifying the selected commit.
2. Dispatch the artifact workflow for that commit. Require both architecture
   artifact gates and the Arch clean-install gate to pass; inspect namcap
   warnings as well as errors. Save the run URL, commit, artifacts and checksums
   before their retention expires. Confirm all three product versions are 0.2.0.
3. Perform any desired Snapdragon hardware/model qualification separately.
4. Inspect the existing tag-triggered `release.yml` and its protected `release`
   environment before creating a tag: creating a `v*` tag currently invokes that
   publishing workflow, whose asset set differs from this CPU bundle. Decide and
   review which publishing workflow/assets are intended before proceeding.
5. Only the owner, in a separate explicitly approved publishing session, creates
   the version tag and real release or uploads a package. Require the protected
   release environment's human approval. Never substitute an unreviewed rebuild
   for verified bytes; verify downloaded published assets against the saved sums.

None of these publishing steps are performed by the artifact workflow or by this
release-build task. Do not put a release token or SDK material into CI.
