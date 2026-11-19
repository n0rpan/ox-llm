# Known issues

Open items that affect users or contributors. Fixed items move to [CHANGELOG.md](CHANGELOG.md).

## Product

- **Images:** `oxi` is experimental and only covers the 512 px fast path. Other sizes use
  stable-diffusion.cpp.
- **Opt-in kernels are not qualified for default use.** `--pf-attn pfg`, `--pf-attn fa2|fa2x` and
  `--attn-out pb` are opt-in until their pre-registered gates pass on the test laptop. `--user-snapshots`
  is off by default until its hardware gate passes.
- **LAN share is not in the release binaries.** The v0.2.1 binaries do not include LAN sharing.
- **Panel is not packaged.** The omarchy panel source is in the repository but is not part of the
  v0.2.1 packages.
- **NPU:** parked. No NPU path is in the default build, and no public number uses the NPU.

## Release and build

- **macOS build in progress.** The native arm64 `oxa` build is non-required CI (`macos.yml`) and is
  expected to be red until the macOS sandbox slices land. No macOS release asset exists yet.
- **Release binaries need qualification.** Each release candidate's binaries need their own checksums and
  install gate pack run before publication.
- **Helper CI is functional only.** The aarch64 helper is not the target SoC. Its timings decide
  nothing. Timing, KLD and identity gates run on the Snapdragon laptop.
