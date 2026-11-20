#!/usr/bin/env bash
# Regression test for Cargo license expressions in the release SBOM.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/payload"
printf 'fixture\n' > "$work/payload/file"
VERSION=0.0.0 "$root/packaging/release/sbom.sh" "$work/payload" "$work/sbom.json"

jq -e '
  any(.components[];
    .name == "serde" and
    any(.licenses[]?; .expression == "MIT OR Apache-2.0")
  )
' "$work/sbom.json" >/dev/null
