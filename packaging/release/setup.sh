#!/usr/bin/env bash
# Fedora 44 supplies GCC 16, matching the documented CPU kernel compiler.
set -euo pipefail
dnf -y -q install --setopt=install_weak_deps=False gcc gcc-c++ binutils make cmake ninja-build \
  git-core strace tar gzip findutils which diffutils curl ca-certificates jq
case $(uname -m) in
  aarch64) checksum=a9c168dbea81a84671c6b7a5c1795cb0eb699020110d107ddfca427bd797e697;;
  x86_64) checksum=5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4;;
  *) exit 1;;
esac
export CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
mkdir -p "$CARGO_HOME" "$RUSTUP_HOME"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL -o "$tmp/rustup-init" \
  "https://static.rust-lang.org/rustup/archive/1.29.2/$(uname -m)-unknown-linux-gnu/rustup-init"
echo "$checksum  $tmp/rustup-init" | sha256sum -c -
chmod +x "$tmp/rustup-init"
RUSTUP_INIT_SKIP_PATH_CHECK=yes "$tmp/rustup-init" -y --no-modify-path --profile minimal --default-toolchain 1.99.1
echo "$CARGO_HOME/bin" >> "${GITHUB_PATH:-/dev/null}"
