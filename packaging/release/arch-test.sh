#!/usr/bin/env bash
# Run as root in a fresh archlinux:latest container, with artifacts mounted at /artifacts.
set -euo pipefail
# Qualification must inherit the real policy required by the shipped native engine.
require() { if ! "$@"; then printf 'NATIVE-MEMORY-REFUSAL: %s\n' "$predicate" >&2; exit 1; fi; }
predicate='unified cgroup membership must be root'
require test "$(awk -F: '$1 == "0" && $2 == "" { print $3 }' /proc/self/cgroup)" = /
predicate='memory.swap.max must be 0'
require test "$(cat /sys/fs/cgroup/memory.swap.max)" = 0
predicate='memory.swap.current must be 0'
require test "$(cat /sys/fs/cgroup/memory.swap.current)" = 0
pacman -Syu --noconfirm --needed base-devel namcap python bubblewrap rust cmake ninja git strace
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
# The official slim image excludes usr/share/doc/*. Keep its other policies and
# allow only our required guide in this private CI config, never a user's config.
ci_pacman=/build/package-pacman.conf
cp /etc/pacman.conf "$ci_pacman"
cat >> "$ci_pacman" <<'POLICY'

[options]
NoExtract = !usr/share/doc !usr/share/doc/ !usr/share/doc/ox-llm !usr/share/doc/ox-llm/*
POLICY
# Rebuild first so both installed variants use the same immutable-source proof harness.
for variant in ox-llm ox-llm-bin; do
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
  pacman --config "$ci_pacman" -U --noconfirm "$package"
  pacman --config "$ci_pacman" -Ql "$variant" | awk '{print $2}' | while read -r path; do
    if [[ ! -d $path ]]; then echo "$path"; fi
  done > /tmp/installed-files
  # Installation must preserve the exact native bytes qualified from the payload.
  case $variant in
    ox-llm-bin) payload="$PWD/src/usr" ;;
    ox-llm) payload="$PWD/src/payload/usr" ;;
  esac
  for binary in oxa oxe-server local-ai-memlat-boot; do cmp "$payload/bin/$binary" "/usr/bin/$binary"; done
  cmp "$payload/share/doc/ox-llm/NATIVE-INSTALL.md" /usr/share/doc/ox-llm/NATIVE-INSTALL.md
  templates=(05-local-ai-memlat-fallback.hook 20-local-ai-memlat-fallback
             85-local-ai-memlat-rebuild 99-z-local-ai-memlat-rebuild.hook)
  data=/usr/share/ox-llm/memlat
  [[ $(find "$data" -mindepth 1 -maxdepth 1 | wc -l) -eq 4 ]]
  for template in "${templates[@]}"; do
    cmp "$payload/share/ox-llm/memlat/$template" "$data/$template"
    [[ $(stat -c %a "$data/$template") == 644 ]]
    for active in "/etc/pacman.d/hooks/$template" "/usr/share/libalpm/hooks/$template" "/etc/limine-entry-tool.d/$template"; do
      [[ ! -e $active && ! -L $active ]]
    done
  done
  sha256sum "$data"/* > "/packages/MEMLAT-DATA-$variant.txt"
  python -B /smoke.py
  # Reuse #682 on each actual installed ELF, including the independently rebuilt source package.
  source_root="/build/ox-llm/src/ox-llm-$OX_SOURCE_COMMIT"
  receipt="/packages/NO-PYTHON-PROOF-$variant.txt"
  { printf 'source=%s\n' "$OX_SOURCE_COMMIT"; sha256sum "$package" | cut -d' ' -f1;
    (cd /usr/bin && sha256sum oxa oxe-server); } > "$receipt"
  test -x /usr/bin/oxa
  test -x /usr/bin/oxe-server
  oxa_sha256=$(sha256sum /usr/bin/oxa | cut -d' ' -f1)
  oxe_sha256=$(sha256sum /usr/bin/oxe-server | cut -d' ' -f1)
  proof_home=$(mktemp -d)
  chown builder:builder "$proof_home"
  trap 'rm -rf "$proof_home"' EXIT
  runuser -u builder -- env -i PATH=/usr/bin:/bin HOME="$proof_home" \
    CARGO_HOME=/home/builder/.cargo RUSTUP_HOME=/home/builder/.rustup \
    SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" \
    OXA_TEST_RELEASE_OXA=/usr/bin/oxa OXA_TEST_RELEASE_OXE_SERVER=/usr/bin/oxe-server \
    OXA_TEST_RELEASE_OXA_SHA256="$oxa_sha256" OXA_TEST_RELEASE_OXE_SERVER_SHA256="$oxe_sha256" \
    OXA_TEST_REQUIRE_RELEASE_ARTIFACT=1 \
    bash -eu -c 'source "$1/packaging/release/environment.sh"; cd oxa; cargo fetch --locked;
      CARGO_NET_OFFLINE=true cargo test --release --locked -p oxa-cli --test no_python_in_path -- --nocapture' proof "$source_root" \
    2>&1 | tee -a "$receipt"
  rm -rf "$proof_home"
  trap - EXIT
  pacman --config "$ci_pacman" -Qkk "$variant"
  pacman --config "$ci_pacman" -Rns --noconfirm "$variant"
  while read -r path; do
    [[ ! -e $path && ! -L $path ]] || { echo "left behind: $path"; exit 1; }
  done < /tmp/installed-files
  for path in /usr/bin/local-ai-memlat-boot /usr/bin/oxc /usr/bin/oxa /usr/bin/oxe /usr/bin/oxe-server /usr/share/ox-llm /usr/share/doc/ox-llm /usr/share/licenses/ox-llm /usr/share/licenses/ox-llm-bin; do
    [[ ! -e $path && ! -L $path ]] || { echo "left behind: $path"; exit 1; }
  done
  cd /build
done

cd /packages
sha256sum -- *.pkg.tar.zst namcap-*.txt NO-PYTHON-PROOF-*.txt MEMLAT-DATA-*.txt > SHA256SUMS
