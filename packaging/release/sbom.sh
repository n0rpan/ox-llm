#!/usr/bin/env bash
# CycloneDX 1.6 inventory from Cargo's locked resolver, plus shipped file hashes.
#   packaging/release/sbom.sh PAYLOAD_DIR OUTPUT.json     (VERSION, e.g. v0.2.1, names the product version)
# `cargo metadata` covers the full locked workspace resolution, including build and test-only
# components. Cargo's exact source identity is retained so equal name/version pairs do not collide.
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

: > "$work/components"
scope='workspace resolution; includes build and test dependencies'
for workspace in engine oxa oxc oxc-rs; do
  [ -f "$root/$workspace/Cargo.toml" ] || continue
  # Cargo metadata identifies packages and their sources but omits registry checksums. Key the
  # Cargo.lock values by the same full identity; name/version alone is not unique.
  awk '
    function flush() {
      if (name != "" && sum != "") print name "\t" ver "\t" source "\t" sum
      name = ver = source = sum = ""
    }
    /^\[\[package\]\]/ { flush(); next }
    /^name = / { gsub(/"/, "", $3); name = $3 }
    /^version = / { gsub(/"/, "", $3); ver = $3 }
    /^source = / { sub(/^[^=]*= "/, ""); sub(/"$/, ""); source = $0 }
    /^checksum = / { gsub(/"/, "", $3); sum = $3 }
    END { flush() }
  ' "$root/$workspace/Cargo.lock" > "$work/checksums"
  (cd "$root/$workspace" && cargo metadata --locked --format-version 1) |
    jq -c --arg root "$root/" --arg scope "$scope" --rawfile sums "$work/checksums" '
      (reduce ($sums | split("\n")[] | select(length > 0) | split("\t")) as $row
        ({}; .[$row[0] + "\u0000" + $row[1] + "\u0000" + $row[2]] = $row[3])) as $checksums |
      .packages[] |
      (.manifest_path | sub("/Cargo.toml$"; "")) as $directory |
      (if .source != null then .source
       elif $directory | startswith($root) then "path+workspace/" + ($directory | ltrimstr($root))
       else "path+" + $directory end) as $source |
      ($checksums[.name + "\u0000" + .version + "\u0000" + (.source // "")] // "") as $checksum |
      (.license // "") as $license |
      ("pkg:cargo/" + (.name | @uri) + "@" + (.version | @uri) +
       "?source=" + ($source | @uri)) as $ref |
      {type: "library", "bom-ref": $ref, name, version, purl: $ref,
       properties: [
         {name: "ox-llm:scope", value: $scope},
         {name: "ox-llm:cargo-source", value: $source}
       ]} |
      if $license != "" then .licenses = [{expression: $license}] else . end |
      if $checksum != "" then .hashes = [{alg: "SHA-256", content: $checksum}] else . end
    ' >> "$work/components"
done

# Hash through stdin: unlike `sha256sum FILE`, this never adds a backslash marker for unusual names.
(cd "$payload" && find . -type f -print0 | sort -z) |
  while IFS= read -r -d '' path; do
    relative=${path#./}
    hash=$(sha256sum < "$payload/$relative" | awk '{print $1}')
    jq -cn --arg name "$relative" --arg hash "$hash" \
      '{type: "file", "bom-ref": $name, name: $name,
        hashes: [{alg: "SHA-256", content: $hash}]}'
  done >> "$work/components"

# `unique_by` merges the same exact Cargo PackageId reached from multiple workspaces. Parsing and
# rewriting with jq also validates that every generated component and the final document are JSON.
jq -s --arg version "$version" '
  {bomFormat: "CycloneDX", specVersion: "1.6", version: 1,
   metadata: {component: {type: "application", name: "ox-llm", version: $version}},
   components: (sort_by(."bom-ref") | unique_by(."bom-ref"))}
' "$work/components" > "$output"
jq -e . "$output" > /dev/null
