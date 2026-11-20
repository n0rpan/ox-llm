#!/usr/bin/env bash
# Run as root in a fresh archlinux:latest container, with the inputs mounted at /artifacts.
#   arch-test.sh source   builds ox-llm from the source archive and runs its check()
#   arch-test.sh binary   repackages the release tarball as ox-llm-bin
# Each variant installs its package, verifies it, runs the native smoke test and removes it
# again. They are separate jobs so the source rebuild runs beside the release build instead of
# waiting for it.
set -euo pipefail
variant=${1:?usage: arch-test.sh source|binary}
pacman -Syu --noconfirm --needed base-devel namcap bubblewrap rust cmake ninja git
useradd -m builder
install -d -o builder -g builder /build
cp /artifacts/* /build/
chown -R builder:builder /build
cd /build
sha256sum -c SHA256SUMS
export SOURCE_DATE_EPOCH
case "$variant" in
  source)
    recipe=ox-llm
    export OX_SOURCE_COMMIT
    export OX_SOURCE_SHA256=$(sha256sum "ox-llm-$OX_SOURCE_COMMIT.tar.gz" | cut -d' ' -f1)
    ;;
  binary)
    recipe=ox-llm-bin
    # release-build names the archive with the tag (ox-llm-v0.2.0-...); the PKGBUILD uses pkgver.
    cp ox-llm-v*-linux-$(uname -m).tar.gz "ox-llm-0.2.1-linux-$(uname -m).tar.gz"
    export OX_BINARY_SHA256=$(sha256sum "ox-llm-0.2.1-linux-$(uname -m).tar.gz" | cut -d' ' -f1)
    ;;
  *)
    echo "unknown variant: $variant" >&2
    exit 2
    ;;
esac
install -d -o builder -g builder "/build/$recipe"
cp "/recipes/$recipe/PKGBUILD" "/build/$recipe/"
cp /build/*.tar.gz "/build/$recipe/"
chown -R builder:builder "/build/$recipe"
cd "/build/$recipe"
runuser -u builder -- makepkg --noconfirm --cleanbuild
package=$(find . -maxdepth 1 -name '*.pkg.tar.zst' -print)
bsdtar -xOf "$package" .PKGINFO | awk '
  /^(depend|makedepend|checkdepend) = python($|[-<>=:])/ { print "Unexpected Python dependency: " $0; failed = 1 }
  END { exit failed }
'
namcap PKGBUILD 2>&1 | tee /tmp/namcap.log
namcap "$package" 2>&1 | tee -a /tmp/namcap.log
# Warnings remain visible; any namcap error fails the job.
if grep -qE '(^| )E: |^Error:' /tmp/namcap.log; then exit 1; fi
cp "$package" /packages/
cp /tmp/namcap.log "/packages/namcap-$recipe.txt"
pacman -U --noconfirm "$package"
pacman -Ql "$recipe" | awk '{print $2}' | while read -r path; do
  if [[ ! -d $path ]]; then echo "$path"; fi
done > /tmp/installed-files
sh /no-python.sh --list /tmp/installed-files
bash /smoke.sh
pacman -Qkk "$recipe"
pacman -Rns --noconfirm "$recipe"
while read -r path; do
  [[ ! -e $path && ! -L $path ]] || { echo "left behind: $path"; exit 1; }
done < /tmp/installed-files
for path in /usr/bin/oxc /usr/bin/oxa /usr/bin/oxe /usr/bin/oxe-server /usr/share/ox-llm /usr/share/licenses/ox-llm /usr/share/licenses/ox-llm-bin; do
  [[ ! -e $path && ! -L $path ]] || { echo "left behind: $path"; exit 1; }
done

cd /packages
sha256sum -- *.pkg.tar.zst namcap-*.txt > SHA256SUMS
