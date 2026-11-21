#!/usr/bin/env bash
# Assemble downloaded dev artifacts in dist/ before attestation and upload.
set -euo pipefail
file_version=${1:?dev file version}
mkdir dist
for arch in aarch64 x86_64; do
  source="combined/release-build-$arch"
  (cd "$source" && sha256sum -c SHA256SUMS)
  cp "$source/ox-llm-$file_version-linux-$arch.tar.gz" "$source/ox-llm-$file_version-linux-$arch.sbom.cdx.json" dist/
  for proof in REPRODUCIBILITY TOOLCHAIN NO-PYTHON-PROOF; do
    cp "$source/$proof.txt" "dist/ox-llm-$file_version-linux-$arch.$proof.txt"
  done
done
macos="ox-llm-$file_version-macos-arm64.tar.gz"
if [[ -f macos-dist/$macos ]]; then
  cp "macos-dist/$macos" dist/
else
  echo "::warning::no macOS arm64 asset for $file_version; publishing Linux only"
fi
# Every payload path is under usr/, and the native oxa is present.
for tarball in dist/*.tar.gz; do
  tar -tzf "$tarball" | awk '!/^usr\// || /(^|\/)\.\.(\/|$)/ { print "unsafe payload path: " $0; bad = 1 } END { exit bad }'
  tar -tzf "$tarball" | grep -x 'usr/bin/oxa' >/dev/null || { echo "missing native oxa in $tarball" >&2; exit 1; }
done
# Linux archives must hold ELF executables; only the -macos-arm64 archive holds Mach-O.
packaging/release/no-python.sh dist/*.tar.gz
(cd dist && sha256sum -- * > SHA256SUMS && sha256sum -c SHA256SUMS)
cp dist/SHA256SUMS "dist/SHA256SUMS-$file_version"
