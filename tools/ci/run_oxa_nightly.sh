#!/usr/bin/env bash
# Full model-free workspace on a native ARM64 host; the report guard rejects runtime skips.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
report_dir=${1:?usage: run_oxa_nightly.sh REPORT_DIRECTORY}
mkdir -p "$report_dir"
report_dir=$(cd "$report_dir" && pwd)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
toolchain=${RUSTUP_TOOLCHAIN:-1.99.1}
target_dir=${CARGO_TARGET_DIR:-$repo_root/oxa/target}
test_root=$(mktemp -d)
trap 'rm -rf -- "$test_root"' EXIT
isolated() {
  env -i PATH="$PATH" CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" \
    RUSTUP_TOOLCHAIN="$toolchain" CARGO_TARGET_DIR="$target_dir" \
    CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" HOME="$test_root" \
    XDG_CONFIG_HOME="$test_root/c" XDG_STATE_HOME="$test_root/s" \
    XDG_DATA_HOME="$test_root/d" XDG_CACHE_HOME="$test_root/k" XDG_RUNTIME_DIR="$test_root/r" \
    OXA_REQUIRE_BWRAP=1 "$@"
}
cd "$repo_root/oxa"
ulimit -c 0
isolated cargo build --profile ci --locked -p oxa-cli -p oxa-fake
isolated cargo test --profile ci --locked --workspace --doc --no-fail-fast -- --nocapture \
  2>&1 | tee "$report_dir/doctests.log"
if grep -n '^skipped: ' "$report_dir/doctests.log"; then
  echo '::error::nightly doctest skipped at runtime'
  exit 1
fi
isolated cargo nextest list --profile ci --cargo-profile ci --locked --workspace \
  --message-format json > "$report_dir/inventory.json"
isolated cargo nextest run --profile ci --cargo-profile ci --locked --workspace \
  -E 'binary(dint_daily_driver)' --stress-count 10 --test-threads 4 --no-tests fail \
  2>&1 | tee "$report_dir/readiness-stress.log"
# A restored cache or the stress run's subset report is never evidence for this full run.
rm -f -- "$repo_root/oxa/target/nextest/ci/junit.xml"
isolated cargo nextest run --profile ci --cargo-profile ci --locked --workspace \
  --no-tests fail --test-threads 4 --success-output immediate \
  --failure-output immediate-final --no-output-indent
python3 -I "$repo_root/tools/ci/check_oxa_report.py" --expected "$report_dir/inventory.json" \
  "$repo_root/oxa/target/nextest/ci/junit.xml"
