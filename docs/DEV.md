# Dev builds

Every main push runs `dev-build.yml`, reusing the full two-architecture release recipe,
including two-build reproducibility and exact-artifact no-Python tests. Only successful,
native-only payloads are published to the rolling `dev` prerelease; it is never `latest`.
Archives, SBOMs and proof receipts include the package version and seven-character source
SHA. SHA256SUMS and downloadable provenance accompany them. The workflow moves only the
`dev` tag; normal release tags and installs are untouched. Python controller payloads fail
publication until the canonical Rust-only packaging is present.

Run `tools/lab/dev-update.sh` (lab copy: `~/oxa-lab/dev-update.sh`). It requires Linux,
Bash, GNU coreutils/tar, flock, curl, jq and a recent GitHub CLI. No login or saved GitHub
credentials are used. Checksums and provenance must match this repository's main commit
and dev workflow before activation. The default private prefix is `~/oxa-lab/dev`;
an optional argument selects another empty or already marked dev prefix. No stable
prefix, service or PATH is changed. Use `~/oxa-lab/dev/bin/oxa`; its receipt is
`~/oxa-lab/dev/current/receipt.json`. The current and one previous build are retained.
