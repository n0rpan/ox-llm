#!/usr/bin/env bash
# Model-free source tests. The full sandbox/parity suite remains in checks.yml.
set -euo pipefail
source "$(dirname "$0")/environment.sh"
ulimit -c 0
sandbox=$(mktemp -d)
trap 'rm -rf "$sandbox"' EXIT
export CARGO_HOME=${CARGO_HOME:-$HOME/.cargo} RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
export HOME=$sandbox XDG_CONFIG_HOME=$sandbox/config XDG_STATE_HOME=$sandbox/state
export XDG_CACHE_HOME=$sandbox/cache XDG_DATA_HOME=$sandbox/data XDG_RUNTIME_DIR=$sandbox/runtime
isolated() {
  env -i PATH="$PATH" HOME="$HOME" CARGO_HOME="$CARGO_HOME" RUSTUP_HOME="$RUSTUP_HOME" \
    XDG_CONFIG_HOME="$XDG_CONFIG_HOME" XDG_STATE_HOME="$XDG_STATE_HOME" \
    XDG_CACHE_HOME="$XDG_CACHE_HOME" XDG_DATA_HOME="$XDG_DATA_HOME" XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    RUSTFLAGS="$RUSTFLAGS" CFLAGS="$CFLAGS" CXXFLAGS="$CXXFLAGS" SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" \
    CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=symbols CARGO_INCREMENTAL=0 \
    CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS" "$@"
}
(cd engine && isolated cargo test --release --locked -p oxe-cli --test version)
(cd engine && isolated cargo test --release --locked -p server --lib)
(cd oxa && isolated cargo test --release --locked -p oxa-cli --lib)
(cd lab && env -i PATH="$PATH" HOME="$sandbox" PYTHONPATH=src python3 -B -m unittest \
  tests.test_runtime_version tests.test_controller_contract tests.test_bootstrap)
