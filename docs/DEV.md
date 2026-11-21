# Dev builds

Every main push runs `dev-build.yml`, reusing the release recipe: each architecture is built
once, then tested on the exact artifact (no-Python proof, SBOM, checksums). The two-build
reproducibility comparison runs on the nightly `dev-build` schedule and in the release lane,
not on every push; a dev push's `REPRODUCIBILITY.txt` says it was built once. The model-free
source tests are skipped only when the pushed commit already passed the merge queue and changed
no dependency cache key (`tools/ci/queue_covered.py`); a release and a nightly run always take
them. Only successful,
native-only payloads are published to the rolling `dev` prerelease; it is never `latest`, and
a nightly or dispatched run publishes nothing.
The dev version is a pre-release of the next version: the latest stable `vX.Y.Z` tag bumped to
`X.Y.(Z+1)`, plus `-dev.<N>+<sha7>` where N is the number of main commits since that stable tag
(`git rev-list --count <tag>..HEAD`; from the root without a stable tag), e.g. `v0.2.2-dev.137+6955919`.
SemVer compares the numeric pre-release N numerically (`dev.9 < dev.10 < dev.137 < 0.2.2`) and ignores
the `+<sha7>` build metadata. The version string keeps the `+`; in file and release-asset names it is
written `.` (`ox-llm-v0.2.2-dev.137.6955919-linux-x86_64.tar.gz`), because `+` is not safe in asset names.
The version is computed in one place, `tools/ci/dev_version.py` (`--file-name` prints the file form);
without a stable tag the workspace version is used. `dev-update*.sh` accept the older
`vX.Y.Z-dev.<sha7>` names, pick the highest semver (then highest N) for the `dev` release commit, and
record the `+` form in their receipts.
Dev binaries report that full version (`oxa --version` prints `oxa 0.2.2-dev.137+6955919`, likewise
`oxe` and `oxe-server`). `packaging/release/build.sh` and `portable-build.sh` export it as
`OX_BUILD_VERSION=${VERSION#v}`, the same value that names the release; a plain `cargo build` and
every test run leave it unset and report the package version. `release-build.yml` fails when the
extracted tarball's `--version` output differs from the release name (pull requests build as
`v<workspace>-pr+<sha7>` so this check also runs there). The engine fingerprint `oxe-<package
version>` that `oxa` compares for compatibility, and User-Agent values, stay on the package version.
Archives, SBOMs and proof receipts include the package version and seven-character source
SHA. SHA256SUMS and downloadable provenance accompany them. The workflow moves only the
`dev` tag; normal release tags and installs are untouched. Python controller payloads fail
publication until the canonical Rust-only packaging is present.

Linux dev assets are required for publication. The macOS arm64 twin,
`tools/lab/dev-update-macos.sh`, uses a best-effort asset which may be absent when its native build fails.

Run `tools/lab/dev-update.sh` (lab copy: `~/oxa-lab/dev-update.sh`). It requires Linux,
Bash, GNU coreutils/tar, getent, flock, curl, jq and a recent GitHub CLI. No login or saved GitHub
credentials are used. Checksums and provenance must match this repository's main commit
and dev workflow before activation. The default prefix is
`~/.local/state/oxc/dev-builds/default` under your account home. The sandbox denies
access to `~/.local/state/oxc`. An optional argument selects another empty or
already marked direct child of `dev-builds`. An existing prefix must be owned by
your user and have mode `0700`. Its path must use real directories owned by root
or your user. Ancestors writable by others must be sticky.
No stable prefix, service or PATH is changed. The executable is at
`~/.local/state/oxc/dev-builds/default/bin/oxa` and its receipt is at
`~/.local/state/oxc/dev-builds/default/current/receipt.json`. The current and one
previous build are retained.

## macOS arm64 dev build

`dev-build.yml` also builds `oxa` natively on `macos-15` and publishes
`ox-llm-<file-version>-macos-arm64.tar.gz` (`usr/bin/oxa` only, staged by
`packaging/macos/stage.sh`) to the same `dev` prerelease. It is covered by the same
`SHA256SUMS-<file-version>` and provenance. The job is best effort: if it fails, the Linux
assets still publish and the Mac asset is absent. A failed main-push build opens the p1 issue
"macOS dev build broken on main" (or comments on the open one); the next green main-push build
closes it. This is the only macOS build after a merge: PRs build macOS only for macOS-relevant
paths and the merge queue never does.

To qualify a branch without publishing, dispatch `dev-build.yml` on that branch with
`macos_stage_only=true`. Check that the run's head SHA matches the commit under test.
The job extracts the archive, checks that `oxa` is arm64, runs `oxa --version`, and
uploads that same archive as `macos-dist-arm64`. This dispatch does not create a release
or release provenance.

On an Apple Silicon Mac, with `gh` installed (`brew install gh`; used only to verify
provenance), run `bash tools/lab/dev-update-macos.sh` from a checkout. The install is at
`~/.local/state/oxc/dev-builds/default/bin/oxa`. There is no OS sandbox on macOS yet: oxa
says so, and runs commands with your full user rights.
## Guarded promotion onto existing N0 units

`tools/lab/dev-update.sh` keeps its stock behaviour: it acquires the rolling `dev`
prerelease, verifies checksums and provenance, and installs the two-build-reproducible
payload under the dev prefix. The optional `--promote` mode uses that same acquisition and
verification path to activate a verified build for existing units, and adds nothing else:
no second downloader, publisher, cron, service or host compile.

The mode takes the operator's own existing artifacts as input, so no private path or
identity is built in. The flags are a repository operator-script surface, not a shipped oxa
CLI command: they are declared here and in the script's usage text rather than in
`oxa/oxa-interactive/src/command_registry.rs`, which lists product commands only.

```sh
tools/lab/dev-update.sh [dev-prefix] --promote \
  --unit oxa-lead.service --board-unit <board-sampler-unit> \
  --binary <regular-board-binary> --manifest <its-checksum-manifest> \
  --install-lock <operator-artifact-install-lock> \
  --session <dedicated-session.jsonl> --receipt <operator-rollout-receipt>
```

The units run their own regular executable and checksum manifest, not a dev-prefix path.
The stock receipt's `sha256` is the archive digest and never describes the executable, so
the release also carries a digest computed from the extracted bytes
(`current/binary.sha256`); the promoted executable and its manifest both carry that value.

Before anything is downloaded the mode decides from the last successful receipt: the
source baseline is the digest-bound operator rollout receipt when it describes the binary
the units run, and only otherwise the stock install whose own executable digest is that
running binary; the digest hashed is the live process, not what the path holds now. The
running digest must also match the operator manifest. The candidate must be ahead of that baseline and on `main` (public
compare metadata, the same question the dev workflow asks — no attestation parser is
added). Unchanged, older or off-`main` sources deploy nothing, and so does an active shadow
board sampler unit or a busy Session; a torn board sample cannot prove idleness, so the
sampler's own systemd state is read instead.

The activation window is short and holds the operator artifact-install lock (separate from
the stock download lock) plus the native Session sidecar lock; neither is held across the
download. The exact previous executable and manifest are staged with their digest and
modes, the verified executable and the matching manifest are renamed into place together,
only the configured Lead unit is restarted, and the running digest, the manifest and a
plain `GET` on the chat endpoint (which must be refused) decide the result. Any failure
restores the staged pair and restarts the unit again; the receipt records the build that
is actually active.

The receipt is fixed, private text with source and binary digests only. Settings, durable
sessions, claims, worktrees, Tailscale routes and the shadow schedule are untouched. The
unattended path performs host canaries only: it never signs in, never reproduces a chat
turn and never impersonates the owner, whose own acceptance stays a separate owner step.
Wiring a user timer to this mode is likewise an operator step; the script installs and
enables nothing.

The N0 layout keeps the executable and its checksum apart: the units run the
regular `board-bin/oxa`, and `board/binary.sha256` names it as `../board-bin/oxa`.
Both live paths are operator input, each is validated in its own directory, and
each rename happens inside its own directory, so the split layout activates
unchanged and the manifest keeps its relative reference.

Wiring is the operator's. For the split-window guarantee the shadow sampler (its
verification step and its execution) must take the same artifact-install lock
shared, so it can never observe the window between the executable rename and the
manifest rename:

```text
flock --shared <operator-artifact-install-lock> <sampler command>
```

The activation takes that lock exclusively, so a sampler run in progress defers
the activation and an activation in progress defers the sampler.
