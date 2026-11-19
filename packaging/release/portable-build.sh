#!/usr/bin/env bash
# Portable Linux x86_64 `oxa`: built against Debian 11 glibc 2.31, so it starts on every
# distribution at least that new (debian 11, ubuntu 22.04, python:3.12-slim, Arch, Fedora).
# Run inside debian:bullseye-slim with git-core, gcc, curl and ca-certificates installed.
# Writes dist-portable/oxa, dist-portable/oxa-fake (smoke only, not shipped) and the checksum.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
export CARGO_HOME=${CARGO_HOME:-/opt/cargo} RUSTUP_HOME=${RUSTUP_HOME:-/opt/rustup}
[[ $(uname -m) == x86_64 ]] || { echo "portable build is x86_64 only" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$CARGO_HOME" "$RUSTUP_HOME"
# Same pinned rustup and toolchain as packaging/release/setup.sh.
curl --proto '=https' --tlsv1.2 -fsSL -o "$tmp/rustup-init" \
  https://static.rust-lang.org/rustup/archive/1.29.2/x86_64-unknown-linux-gnu/rustup-init
echo "5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4  $tmp/rustup-init" | sha256sum -c -
chmod +x "$tmp/rustup-init"
RUSTUP_INIT_SKIP_PATH_CHECK=yes "$tmp/rustup-init" -y --no-modify-path --profile minimal --default-toolchain 1.99.1
export PATH="$CARGO_HOME/bin:$PATH"
export RUSTFLAGS="-C target-cpu=x86-64 --remap-path-prefix=$root=/usr/src/ox-llm --remap-path-prefix=$CARGO_HOME=/usr/src/cargo --remap-path-prefix=$RUSTUP_HOME=/usr/src/rust"
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=symbols CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
(cd oxa && cargo build --release --locked -p oxa-cli --bin oxa -p oxa-fake --bin oxa-fake)
out=$root/dist-portable
rm -rf "$out"
mkdir -p "$out"
install -m 0755 oxa/target/release/oxa oxa/target/release/oxa-fake "$out/"
# The portability promise: no symbol newer than glibc 2.31, and no libstdc++ or libgcc_s.
max=$(objdump -T "$out/oxa" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -n1)
echo "highest glibc symbol version required by oxa: ${max:-none}"
if [[ -n $max && $(printf '%s\n2.31\n' "$max" | sort -V | tail -n1) != 2.31 ]]; then
  echo "oxa needs glibc $max, newer than the 2.31 floor" >&2
  exit 1
fi
if objdump -p "$out/oxa" | grep -E 'NEEDED +(libstdc\+\+|libgcc_s)'; then
  echo "oxa must not depend on libstdc++ or libgcc_s" >&2
  exit 1
fi
(cd "$out" && sha256sum oxa > oxa.sha256)
