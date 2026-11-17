#!/usr/bin/env bash
# Shared release build/test environment, sourced by the build and check scripts.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
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
export RUSTFLAGS="$cpu --remap-path-prefix=$HOME=/usr/src/build-home --remap-path-prefix=$root=/usr/src/ox-llm --remap-path-prefix=$CARGO_HOME=/usr/src/cargo --remap-path-prefix=$RUSTUP_HOME=/usr/src/rust"
export CFLAGS="-ffile-prefix-map=$HOME=/usr/src/build-home -ffile-prefix-map=$root=/usr/src/ox-llm -ffile-prefix-map=$CARGO_HOME=/usr/src/cargo -ffile-prefix-map=$RUSTUP_HOME=/usr/src/rust"
# The checkout mount differs from the two clean build roots in explicit Docker jobs.
if [[ -n ${GITHUB_WORKSPACE:-} ]]; then
  RUSTFLAGS+=" --remap-path-prefix=$GITHUB_WORKSPACE=/usr/src/checkout"
  CFLAGS+=" -ffile-prefix-map=$GITHUB_WORKSPACE=/usr/src/checkout"
fi
export CXXFLAGS="$CFLAGS"
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=symbols
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
