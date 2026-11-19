# Dev builds

Every main push runs `dev-build.yml`, reusing the full two-architecture release recipe,
including two-build reproducibility and exact-artifact no-Python tests. Only successful,
native-only payloads are published to the rolling `dev` prerelease; it is never `latest`.
The dev version is a pre-release of the next version: the latest stable `vX.Y.Z` tag bumped to
`X.Y.(Z+1)`, plus `-dev.<sha7>` (so `v0.2.1-dev.<sha7>` after `v0.2.0`, `v0.2.2-dev.<sha7>` once
`v0.2.1` is tagged). It is computed in one place, `tools/ci/dev_version.py`; without a stable tag
the workspace version is used. `dev-update*.sh` accept the older `vX.Y.Z-dev.<sha7>` names and pick
the highest semver for the `dev` release commit.
Archives, SBOMs and proof receipts include the package version and seven-character source
SHA. SHA256SUMS and downloadable provenance accompany them. The workflow moves only the
`dev` tag; normal release tags and installs are untouched. Python controller payloads fail
publication until the canonical Rust-only packaging is present.

Linux is the only supported dev-update target today. A macOS arm64 twin,
`tools/lab/dev-update-macos.sh`, exists, but its dev asset is not published yet: the macOS build is still
in progress (see [KNOWN-ISSUES.md](../KNOWN-ISSUES.md)).

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
