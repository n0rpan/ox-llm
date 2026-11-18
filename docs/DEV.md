# Dev builds

Every main push runs `dev-build.yml`, reusing the full two-architecture release recipe,
including two-build reproducibility and exact-artifact no-Python tests. Only successful,
native-only payloads are published to the rolling `dev` prerelease; it is never `latest`.
Archives, SBOMs and proof receipts include the package version and seven-character source
SHA. SHA256SUMS and downloadable provenance accompany them. The workflow moves only the
`dev` tag; normal release tags and installs are untouched. Python controller payloads fail
publication until the canonical Rust-only packaging is present.

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
