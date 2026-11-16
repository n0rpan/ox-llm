# Known issues

Open items that affect users or contributors. Fixed items move to [CHANGELOG.md](CHANGELOG.md).

## Product

- **OpenCode compaction can swallow a prompt (legacy client, both modes).** OpenCode 2.2.7 compacts a
  session when a prompt is submitted above the preset's compaction trigger. It can fold that prompt
  into the compacted history as "historical context", so the model never sees a live user turn.
  Workaround: send the request again after the compaction has finished. oxa avoids this by design:
  compaction policy belongs to the client, and the next prompt is always a live turn. The engine and
  controller stay client-neutral and carry no OpenCode-specific fix.
- **Unknown client versions** run with generic defaults. There is no version pin and no per-client
  template in the core.
- **Opt-in kernels are not qualified for default use.** `--pf-attn pfg`, `--pf-attn fa2|fa2x` and
  `--attn-out pb` are opt-in until their pre-registered gates pass on the test laptop. `--user-snapshots`
  is off by default until its hardware gate passes.
- **Images:** `oxi` is experimental and only covers the 512 px fast path. Other sizes use
  stable-diffusion.cpp.
- **NPU:** parked. No NPU path is in the default build, and no public number uses the NPU.

## Release and build

- **New binaries need qualification.** The repository layout changed in v0.2.0, so the engine binaries'
  SHA-256 changes even when the code does not. The install gate pack runs once on the release
  candidate.
- **Package and bootstrap manifests still name the previous public repositories.** The bootstrap
  fetches pinned engine sources from them. They are updated when the public repository name is chosen
  (ROADMAP, release tooling).
- **Helper CI is functional only.** The aarch64 helper is not the target SoC. Its timings decide
  nothing. Timing, KLD and identity gates run on the Snapdragon laptop.
