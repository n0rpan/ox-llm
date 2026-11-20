#!/usr/bin/env bash
# Stage the native /usr payload: compiled binaries, MEMLAT templates and licence texts.
#   packaging/release/stage.sh DESTINATION     (DESTINATION must not exist)
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: stage.sh DESTINATION" >&2; exit 2; }
root=$(cd "$(dirname "$0")/../.." && pwd)
[ ! -e "$1" ] || { echo "destination must be fresh" >&2; exit 1; }
mkdir -p "$1"
dest=$(cd "$1" && pwd)
mkdir -p "$dest/usr/bin"
for entry in engine/oxe engine/oxe-server oxa/oxa oxa/local-ai-memlat-boot; do
  cp -p "$root/${entry%%/*}/target/release/${entry#*/}" "$dest/usr/bin/${entry#*/}"
done
# Ship the owner-approved MEMLAT templates as data outside active hook paths.
templates=$dest/usr/share/ox-llm/memlat
mkdir -p "$templates"
for name in 05-local-ai-memlat-fallback.hook 20-local-ai-memlat-fallback \
            85-local-ai-memlat-rebuild 99-z-local-ai-memlat-rebuild.hook; do
  cp -p "$root/oxc/packaging/memlat/$name" "$templates/$name"
done
licenses=$dest/usr/share/licenses/ox-llm
mkdir -p "$licenses"
for product in engine oxa oxc; do
  for name in LICENSE NOTICE THIRD_PARTY_NOTICES LICENSE-QuickJS.txt LICENSE-TLS.txt LICENSE-syntect.txt; do
    if [ -f "$root/$product/$name" ]; then cp -p "$root/$product/$name" "$licenses/$product-$name"; fi
  done
  if [ -d "$root/$product/third_party" ]; then cp -rp "$root/$product/third_party" "$licenses/$product-third_party"; fi
done
cp -p "$root/LICENSE" "$licenses/LICENSE"
# The repository's licence collector, for every linked Rust closure.
collector=$root/engine/tools/ci/crate_licenses.sh
for entry in engine/oxe-cli engine/server oxa/oxa-cli oxa/oxc-memlat; do
  (cd "$root/${entry%%/*}" && bash "$collector" "${entry#*/}" "$licenses/crates/${entry#*/}")
done
cp -p "$root/engine/vendor/ggml-subset/LICENSE" "$licenses/ggml-subset-LICENSE"
find "$dest" -type d -exec chmod 755 {} +
find "$dest" -type f ! -type l -exec chmod 644 {} +
chmod 755 "$dest"/usr/bin/*
