#!/usr/bin/env bash
# Run as root in a fresh archlinux:latest container, with artifacts mounted at /artifacts.
set -euo pipefail
pacman -Syu --noconfirm --needed base-devel namcap python bubblewrap rust cmake ninja git
useradd -m builder
install -d -o builder -g builder /build
cp /artifacts/* /build/
chown -R builder:builder /build
cd /build
sha256sum -c SHA256SUMS
export SOURCE_DATE_EPOCH
export OX_SOURCE_COMMIT
export OX_SOURCE_SHA256=$(sha256sum "ox-llm-$OX_SOURCE_COMMIT.tar.gz" | cut -d' ' -f1)
# Use the recipes' selected bundle version; never silently rename another version.
pkgver=$(sed -n 's/^pkgver=//p' /recipes/ox-llm/PKGBUILD)
[[ $pkgver =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
[[ $VERSION == "v$pkgver" ]]
[[ $(sed -n 's/^pkgver=//p' /recipes/ox-llm-bin/PKGBUILD) == "$pkgver" ]]
cp "ox-llm-$VERSION-linux-$(uname -m).tar.gz" "ox-llm-$pkgver-linux-$(uname -m).tar.gz"
export OX_BINARY_SHA256=$(sha256sum "ox-llm-$pkgver-linux-$(uname -m).tar.gz" | cut -d' ' -f1)
# Verify the binary package and then a real source rebuild (including check()).
for variant in ox-llm-bin ox-llm; do
  install -d -o builder -g builder "/build/$variant"
  cp "/recipes/$variant/PKGBUILD" "/recipes/$variant/native.install" "/build/$variant/"
  cp /build/*.tar.gz "/build/$variant/"
  chown -R builder:builder "/build/$variant"
  cd "/build/$variant"
  runuser -u builder -- makepkg --noconfirm --cleanbuild
  package=$(find . -maxdepth 1 -name '*.pkg.tar.zst' -print)
  namcap PKGBUILD 2>&1 | tee /tmp/namcap.log
  namcap "$package" 2>&1 | tee -a /tmp/namcap.log
  # Warnings remain visible; any namcap error fails the job.
  if grep -qE '(^| )E: |^Error:' /tmp/namcap.log; then exit 1; fi
  cp "$package" /packages/
  cp /tmp/namcap.log "/packages/namcap-$variant.txt"
  pacman -U --noconfirm "$package"
  pacman -Ql "$variant" | awk '{print $2}' | while read -r path; do
    if [[ ! -d $path ]]; then echo "$path"; fi
  done > /tmp/installed-files
  # Installation must preserve the exact native bytes qualified from the payload.
  case $variant in
    ox-llm-bin) payload="$PWD/src/usr/bin" ;;
    ox-llm) payload="$PWD/src/payload/usr/bin" ;;
  esac
  for binary in oxa oxe-server; do cmp "$payload/$binary" "/usr/bin/$binary"; done
  python -B /smoke.py
  pacman -Qkk "$variant"
  pacman -Rns --noconfirm "$variant"
  while read -r path; do
    [[ ! -e $path && ! -L $path ]] || { echo "left behind: $path"; exit 1; }
  done < /tmp/installed-files
  for path in /usr/bin/oxc /usr/bin/oxa /usr/bin/oxe /usr/bin/oxe-server /usr/share/ox-llm /usr/share/licenses/ox-llm /usr/share/licenses/ox-llm-bin; do
    [[ ! -e $path && ! -L $path ]] || { echo "left behind: $path"; exit 1; }
  done
  cd /build
done

cd /packages
sha256sum -- *.pkg.tar.zst namcap-*.txt > SHA256SUMS
