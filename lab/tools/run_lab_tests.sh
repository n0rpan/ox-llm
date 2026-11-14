#!/bin/sh
# Run the lab unit suite with a throwaway HOME and XDG roots (PATH kept), so no default path can
# reach the real ~/.config/local-ai, ~/.local/state/local-ai or the Omarchy panel. Extra
# arguments go to unittest (default: discover -s tests). LAB_TEST_SHARD=K/N (CI) runs shard K of N of
# the suite instead (tools/ci_test_shard.py; extra arguments are then ignored).
set -eu
cd "$(dirname "$0")/.."
root=$(mktemp -d "${TMPDIR:-/tmp}/local-ai-lab-tests-XXXXXX")
trap 'rm -rf "$root"' EXIT
mkdir -m 700 "$root/home" "$root/config" "$root/state" "$root/data" "$root/cache" "$root/runtime"
if [ -n "${LAB_TEST_SHARD:-}" ]; then set -- tools/ci_test_shard.py run "${LAB_TEST_SHARD#*/}" "${LAB_TEST_SHARD%/*}"
else
  if [ "$#" -eq 0 ]; then set -- discover -s tests; fi
  set -- -m unittest "$@"
fi
# LOCAL_AI_RUST_FIT_CHECK=off: profile tests do not depend on the helper's CPU (the x86 helper); test_engine_fit
# covers the check itself.
env -u LOCAL_LLM_CONFIG -u LOCAL_AI_LEASE LOCAL_AI_RUST_FIT_CHECK=off HOME="$root/home" XDG_CONFIG_HOME="$root/config" \
    XDG_STATE_HOME="$root/state" XDG_DATA_HOME="$root/data" XDG_CACHE_HOME="$root/cache" \
    XDG_RUNTIME_DIR="$root/runtime" PYTHONPATH=src nice -n 19 python3 "$@"
