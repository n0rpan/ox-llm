#!/bin/sh
# Fetch a pinned model file and verify it: fetch_model.sh URL SHA256 OUT
# A file already at OUT (restored from the Actions cache) is kept when its sha256 matches; any
# other file is replaced. The download is verified before it is moved into place.
set -eu
url=${1:?usage: fetch_model.sh URL SHA256 OUT}
sha=${2:?usage: fetch_model.sh URL SHA256 OUT}
out=${3:?usage: fetch_model.sh URL SHA256 OUT}
if [ -f "$out" ] && echo "$sha  $out" | sha256sum -c --status -; then
    echo "model: $out (cached, sha256 ok)"
    exit 0
fi
mkdir -p "$(dirname "$out")"
start=$(date +%s)
curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 5 -o "$out.part" "$url"
echo "$sha  $out.part" | sha256sum -c -
mv "$out.part" "$out"
echo "model: $out downloaded and verified in $(( $(date +%s) - start )) s"
