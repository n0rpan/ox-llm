#!/usr/bin/env bash
# Stage the macOS arm64 dev or release archive from a built oxa binary (bash and BSD tools only).
# usage: stage.sh <file-version> <oxa-binary> <output-dir>
# Writes <output-dir>/ox-llm-<file-version>-macos-arm64.tar.gz holding exactly usr/, usr/bin/ and
# usr/bin/oxa: the layout tools/lab/dev-update-macos.sh extracts and checks.
set -euo pipefail
version=${1:?version}
binary=${2:?oxa binary}
out=${3:?output dir}
# A dev file version (vX.Y.Z...-dev.N.abcdef0) or a release tag (vX.Y.Z or vX.Y.Z-rc.1).
dev='^v[0-9]+\.[0-9]+\.[0-9]+[0-9A-Za-z.-]*-dev\.[0-9]+\.[0-9a-f]{7}$'
tag='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'
if [[ $version == *dev* ]]; then pattern=$dev; else pattern=$tag; fi
[[ $version =~ $pattern ]] || { echo "bad file version: $version" >&2; exit 1; }
test -f "$binary" && test -x "$binary" && test ! -L "$binary" || { echo "not an executable file: $binary" >&2; exit 1; }
# Mach-O 64-bit, little endian (arm64): cf fa ed fe.
magic=$(od -An -tx1 -N4 "$binary" | tr -d ' \n')
[[ $magic == cffaedfe ]] || { echo "not a Mach-O arm64 executable: $binary" >&2; exit 1; }
mkdir -p "$out"
stage=$(mktemp -d "${TMPDIR:-/tmp}/oxa-stage.XXXXXX")
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/usr/bin"
cp "$binary" "$stage/usr/bin/oxa"
chmod 0755 "$stage/usr" "$stage/usr/bin" "$stage/usr/bin/oxa"
archive="$out/ox-llm-$version-macos-arm64.tar.gz"
# Entries listed explicitly, no recursion, no extended attributes or resource forks, root owner.
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
  owner_args=(--owner=0 --group=0 --numeric-owner)
else
  # macOS supplies bsdtar, which spells the numeric ownership options this way.
  owner_args=(--uid 0 --gid 0)
fi
COPYFILE_DISABLE=1 tar -c -f - --no-recursion --no-xattrs "${owner_args[@]}" -C "$stage" \
  usr usr/bin usr/bin/oxa | gzip -n -9 > "$archive"
tar -tzf "$archive" | grep -qx 'usr/bin/oxa'
echo "$archive"
