#!/bin/sh
# Run the lab unit suite with a throwaway HOME and XDG roots (PATH kept), so no default path can
# reach the real ~/.config/local-ai, ~/.local/state/local-ai or the Omarchy panel. Extra
# arguments go to unittest (default: discover -s tests). LAB_TEST_SHARD=K/N (CI) runs shard K of N of
# the suite instead (tools/ci_test_shard.py; extra arguments are then ignored).
#
# Examples (python3 -m unittest runs from lab/, so module names need the tests. prefix):
#   sh tools/run_lab_tests.sh tests.test_exec_no_thp
#   sh tools/run_lab_tests.sh tests.test_x.Class.test_method
#   sh tools/run_lab_tests.sh discover -s tests -p 'test_x*.py'
# A bare module name is rewritten to its tests.<name> form when tests/<name>.py exists, so
#   sh tools/run_lab_tests.sh test_controller_behaviour
# also works.
set -eu
cd "$(dirname "$0")/.."
root=$(mktemp -d "${TMPDIR:-/tmp}/local-ai-lab-tests-XXXXXX")
trap 'rm -rf "$root"' EXIT
mkdir -m 700 "$root/home" "$root/config" "$root/state" "$root/data" "$root/cache" "$root/runtime"
if [ -n "${LAB_TEST_SHARD:-}" ]; then set -- tools/ci_test_shard.py run "${LAB_TEST_SHARD#*/}" "${LAB_TEST_SHARD%/*}"
else
  if [ "$#" -eq 0 ]; then set -- discover -s tests; fi
  # Rewrite each bare module name (no dot, no leading dash, not `discover`) to its tests.<name>
  # form when tests/<name>.py exists, so `sh tools/run_lab_tests.sh test_controller_behaviour`
  # works like `tests.test_controller_behaviour` (python3 -m unittest runs from lab/).
  n=$#
  while [ "$n" -gt 0 ]; do
    arg=$1; shift; n=$((n - 1))
    case "$arg" in
      -*|discover|*.*) ;;
      *)
        if [ -f "tests/$arg.py" ]; then
          echo "run_lab_tests: '$arg' -> 'tests.$arg'" >&2
          arg="tests.$arg"
        fi
        ;;
    esac
    # Rotate: every argument is re-appended unchanged (or rewritten), so quoting survives.
    set -- "$@" "$arg"
  done
  set -- -m unittest "$@"
fi
# LOCAL_AI_RUST_FIT_CHECK=off: profile tests do not depend on the helper's CPU (the x86 helper); test_engine_fit
# covers the check itself.
env -u LOCAL_LLM_CONFIG -u LOCAL_AI_LEASE LOCAL_AI_RUST_FIT_CHECK=off HOME="$root/home" XDG_CONFIG_HOME="$root/config" \
    XDG_STATE_HOME="$root/state" XDG_DATA_HOME="$root/data" XDG_CACHE_HOME="$root/cache" \
    XDG_RUNTIME_DIR="$root/runtime" PYTHONPATH=src nice -n 19 python3 "$@"
