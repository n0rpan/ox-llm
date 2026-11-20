#!/usr/bin/env bash
# Build from a source archive without relying on the builder's CPU.
set -euo pipefail
source "$(dirname "$0")/environment.sh"
# Stamp the release name (no leading v) into every binary: `oxa --version` prints it. Only here, not
# in environment.sh, so check.sh and the tests compile unstamped and compare with the package version.
if [ -n "${VERSION:-}" ]; then export OX_BUILD_VERSION=${VERSION#v}; fi
(cd engine && cargo build --release --locked -p oxe-cli --bin oxe -p server --bin oxe-server)
(cd oxa && cargo build --release --locked -p oxa-cli --bin oxa -p oxc-memlat --bin local-ai-memlat-boot)
