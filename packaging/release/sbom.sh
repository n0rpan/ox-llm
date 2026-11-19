#!/usr/bin/env bash
# CycloneDX 1.6 inventory from Cargo's locked resolver, plus shipped file hashes.
#   packaging/release/sbom.sh PAYLOAD_DIR OUTPUT.json     (VERSION, e.g. v0.2.1, names the product version)
# `cargo tree --workspace -e all` covers the full workspace resolution, including build and
# test-only components; the scope is recorded on every component rather than claiming a
# minimal runtime BOM.
set -euo pipefail
[ $# -eq 2 ] || { echo "usage: sbom.sh PAYLOAD_DIR OUTPUT.json" >&2; exit 2; }
export LC_ALL=C
root=$(cd "$(dirname "$0")/../.." && pwd)
payload=$(cd "$1" && pwd)
output=$2
version=${VERSION:-}
version=${version#v}
if [ -z "$version" ]; then
  version=$(awk '/^\[workspace\.package\]/{p=1;next} /^\[/{p=0} p&&/^version[ ]*=/{gsub(/[" ]/,"",$3);print $3;exit}' "$root/oxa/Cargo.toml")
fi
[ -n "$version" ] || { echo "sbom: cannot determine the version" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
esc() { sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

: > "$work/components"
scope='workspace resolution; includes build and test dependencies'
for workspace in engine oxa oxc oxc-rs; do
  [ -f "$root/$workspace/Cargo.toml" ] || continue
  # name<TAB>version<TAB>checksum for every locked package that has one
  awk '
    function flush() { if (name != "" && sum != "") print name "\t" ver "\t" sum; name = ver = sum = "" }
    /^\[\[package\]\]/ { flush(); next }
    /^name = / { gsub(/"/, "", $3); name = $3 }
    /^version = / { gsub(/"/, "", $3); ver = $3 }
    /^checksum = / { gsub(/"/, "", $3); sum = $3 }
    END { flush() }
  ' "$root/$workspace/Cargo.lock" > "$work/sums"
  (cd "$root/$workspace" && cargo tree --workspace --locked -e all --target all --prefix none \
      --format '{p}|{l}') | sed -e 's/ (\*)$//' |
    awk -F'|' -v sums="$work/sums" -v scope="$scope" '
      BEGIN {
        while ((getline line < sums) > 0) { split(line, f, "\t"); sum[f[1] " " f[2]] = f[3] }
        q = "\""
      }
      function esc(s) { gsub(/\\/, "\\\\", s); gsub(/"/, "\\\"", s); return s }
      {
        split($1, w, " "); name = w[1]; ver = substr(w[2], 2)
        if (name == "" || ver == "") next
        ref = "pkg:cargo/" name "@" ver
        item = "{\"type\": \"library\", \"bom-ref\": " q ref q ", \"name\": " q esc(name) q \
               ", \"version\": " q esc(ver) q ", \"purl\": " q ref q \
               ", \"properties\": [{\"name\": \"ox-llm:scope\", \"value\": " q scope q "}]"
        lic = $2
        if (lic != "") item = item ", \"licenses\": [{\"expression\": " q esc(lic) q "}]"
        s = sum[name " " ver]
        if (s != "") item = item ", \"hashes\": [{\"alg\": \"SHA-256\", \"content\": " q s q "}]"
        print ref "\t" item "}"
      }' >> "$work/components"
done
# One component per package identity, even when several workspaces resolve it.
sort -t "$(printf '\t')" -k1,1 -u "$work/components" > "$work/libraries"

(cd "$payload" && find . -type f -print | sed 's|^\./||' | sort) | while IFS= read -r relative; do
  hash=$(sha256sum "$payload/$relative" | cut -d' ' -f1)
  e=$(printf '%s' "$relative" | esc)
  printf '%s\t{"type": "file", "bom-ref": "%s", "name": "%s", "hashes": [{"alg": "SHA-256", "content": "%s"}]}\n' \
    "$relative" "$e" "$e" "$hash"
done > "$work/files"

{
  printf '{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1,\n'
  printf ' "metadata": {"component": {"type": "application", "name": "ox-llm", "version": "%s"}},\n' "$version"
  printf ' "components": [\n'
  cat "$work/libraries" "$work/files" | sort -t "$(printf '\t')" -k1,1 | cut -f2- | sed -e '$!s/$/,/' -e 's/^/  /'
  printf ' ]}\n'
} > "$output"
