# Dev builds

Every main push runs `dev-build.yml`, reusing the full two-architecture release recipe,
including two-build reproducibility and exact-artifact no-Python tests. Only successful,
native-only payloads are published to the rolling `dev` prerelease; it is never `latest`.
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
`ox-llm-<version>-macos-arm64.tar.gz` (`usr/bin/oxa` only, staged by
`packaging/macos/stage.sh`) to the same `dev` prerelease. It is covered by the same
`SHA256SUMS-<version>` and provenance. The job is best effort: if it fails, the Linux
assets still publish and the Mac asset is simply absent.

On an Apple Silicon Mac, with `gh` installed (`brew install gh`; used only to verify
provenance), run `bash tools/lab/dev-update-macos.sh` from a checkout. The install is at
`~/.local/state/oxc/dev-builds/default/bin/oxa`. There is no OS sandbox on macOS yet: oxa
says so, and runs commands with your full user rights.
