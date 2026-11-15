#!/usr/bin/env bash
# Shared release build/test environment, sourced by the build and check scripts.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
: "${SOURCE_DATE_EPOCH:?set SOURCE_DATE_EPOCH to the source commit timestamp}"
: "${CARGO_HOME:=$HOME/.cargo}"
: "${RUSTUP_HOME:=$HOME/.rustup}"
export CARGO_HOME RUSTUP_HOME
export TZ=UTC LC_ALL=C
arch=$(uname -m)
case "$arch" in
  aarch64) cpu='-C target-cpu=oryon-1';;
  x86_64) cpu='-C target-cpu=x86-64';;
  *) echo "unsupported architecture: $arch" >&2; exit 1;;
esac
# RUSTFLAGS replaces .cargo/config.toml rustflags; explicitly retain Oryon on ARM.
export RUSTFLAGS="$cpu --remap-path-prefix=$root=/usr/src/ox-llm --remap-path-prefix=$CARGO_HOME=/usr/src/cargo --remap-path-prefix=$RUSTUP_HOME=/usr/src/rust"
export CFLAGS="-ffile-prefix-map=$root=/usr/src/ox-llm -ffile-prefix-map=$CARGO_HOME=/usr/src/cargo"
export CXXFLAGS="$CFLAGS"
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=symbols
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
