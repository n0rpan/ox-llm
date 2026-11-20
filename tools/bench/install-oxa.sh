#!/bin/sh
# Fetch or copy the portable Linux x86_64 oxa binary and verify its checksum on the host.
#
#   install-oxa.sh SOURCE [DEST_DIR]
#
# SOURCE is a local path (the `oxa` file, or the extracted artifact directory holding `oxa` and
# `oxa.sha256`, the layout of the `oxa-portable-linux-x86_64` workflow artifact) or an https URL
# of the `oxa` file (its checksum is read from `<URL>.sha256` or from OXA_SHA256). The verified
# binary is written to DEST_DIR/oxa (default ./oxa-bench-bin) and its path printed. Nothing runs
# oxa and nothing needs a key.
set -eu

src=${1:?usage: install-oxa.sh SOURCE [DEST_DIR]}
dest=${2:-./oxa-bench-bin}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

case $src in
  https://*)
    curl -fsSL --proto '=https' -o "$tmp/oxa" "$src"
    if [ -z "${OXA_SHA256:-}" ]; then
      curl -fsSL --proto '=https' -o "$tmp/oxa.sha256" "$src.sha256"
    fi
    ;;
  http://*) echo "install-oxa: refusing a plain http URL" >&2; exit 2 ;;
  *)
    if [ -d "$src" ]; then src=$src/oxa; fi
    [ -f "$src" ] || { echo "install-oxa: no such file: $src" >&2; exit 2; }
    cp "$src" "$tmp/oxa"
    if [ -f "$(dirname "$src")/oxa.sha256" ]; then cp "$(dirname "$src")/oxa.sha256" "$tmp/oxa.sha256"; fi
    ;;
esac

actual=$(sha256sum "$tmp/oxa" | cut -d' ' -f1)
if [ -n "${OXA_SHA256:-}" ]; then
  expected=$OXA_SHA256
elif [ -f "$tmp/oxa.sha256" ]; then
  expected=$(cut -d' ' -f1 < "$tmp/oxa.sha256")
else
  echo "install-oxa: no checksum (no oxa.sha256 next to the binary and OXA_SHA256 unset)" >&2
  exit 3
fi
if [ "$expected" != "$actual" ]; then
  echo "install-oxa: checksum mismatch: expected $expected, got $actual" >&2
  exit 3
fi

mkdir -p "$dest"
install -m 0755 "$tmp/oxa" "$dest/oxa"
printf '%s  oxa\n' "$actual" > "$dest/oxa.sha256"
echo "$dest/oxa"
