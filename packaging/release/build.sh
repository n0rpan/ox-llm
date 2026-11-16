#!/usr/bin/env bash
# Build from a source archive without relying on the builder's CPU.
set -euo pipefail
source "$(dirname "$0")/environment.sh"
(cd engine && cargo build --release --locked -p server --bin oxe-server)
(cd oxa && cargo build --release --locked -p oxa-cli --bin oxa -p oxc-memlat --bin local-ai-memlat-boot)
# The default install is oxa plus the native engine; controller workspaces are not packaged.
