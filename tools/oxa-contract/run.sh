#!/usr/bin/env bash
# oxa <-> real server contract job (v0.2.0 fit review F5).
#
# Runs oxa's client code (oxa-engine, oxa-session, oxa-fake from an oxa checkout) against the REAL
# `oxe-server --engine stub` (no weights: a synthetic GGUF with the committed Qwen3.6 tokenizer
# fixture) and against the real lab controller in front of it with OXC_QUEUE=1. The scenarios
# are in stub_contract.rs (next to this script); it is copied into a scratch copy of the oxa
# checkout as oxa-core/tests/zz_stub_contract.rs, so the oxa checkout itself is never modified.
#
# usage: tools/oxa-contract/run.sh --oxa DIR --mode engine|controller|both [--server BIN] [--work DIR]
#                                  [--xfail FILE | --strict] [--keep]
#   --mode        engine: direct to oxe-server; controller: through the lab controller with the queue
#                 ON (OXC_QUEUE=1); both: every scenario against both
#   --strict      no expected failures at all (the final import sha must pass this way)
#   (default xfail list: xfail-<oxa sha>.<mode>.txt next to this script when --oxa is a git checkout
#   and the file exists; else none)
#   --oxa DIR     an oxa checkout (the oxa-bootstrap layout: DIR/oxa-core, DIR/oxa-engine, ...; or
#                 the monorepo's oxa/ once it is imported, same layout under oxa/)
#   --server BIN  a built oxe-server (default: cargo build --release --locked -p server in engine/)
#   --work DIR    scratch directory (default: mktemp); removed at the end unless --keep
#   --xfail FILE  test names expected to FAIL at this oxa revision (one per line, # comments). Then
#                 the job passes when exactly those fail: an unexpected failure fails it, and so does
#                 an expected failure that now passes (update the list). Without it every failure
#                 fails the job. Always fail-closed (evaluate.py): a nonzero exit other than cargo's
#                 failed-tests 101, a signal/abort, a missing completion marker, or an executed set
#                 that differs from tests.txt (missing, extra, ignored) is FAIL.
# Prints one line 'OXA CONTRACT RESULT: PASS n/n' or 'OXA CONTRACT RESULT: FAIL ...' (+ step summary).
# Model-free, no SDK, no network beyond 127.0.0.1 (cargo may fetch crates for oxa's lock file).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
oxa= server= work= xfail= keep=0 mode= strict=0
while [ $# -gt 0 ]; do
  case "$1" in
    --oxa) oxa=$2; shift 2 ;;
    --server) server=$2; shift 2 ;;
    --work) work=$2; shift 2 ;;
    --xfail) xfail=$2; shift 2 ;;
    --keep) keep=1; shift ;;
    --mode) mode=$2; shift 2 ;;
    --strict) strict=1; shift ;;
    -h|--help) sed -n 2,22p "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$oxa" ] || { echo "--oxa DIR is required" >&2; exit 2; }
oxa=$(cd "$oxa" && pwd)
[ -f "$oxa/oxa-core/Cargo.toml" ] && [ -f "$oxa/oxa-engine/Cargo.toml" ] && [ -f "$oxa/Cargo.lock" ] || {
  echo "$oxa is not an oxa workspace (oxa-core/, oxa-engine/, Cargo.lock expected)" >&2; exit 2; }
case "$mode" in engine|controller|both) ;; *) echo "--mode engine|controller|both is required" >&2; exit 2 ;; esac
if [ "$strict" = 1 ]; then
  [ -z "$xfail" ] || { echo "--strict and --xfail exclude each other" >&2; exit 2; }
elif [ -z "$xfail" ] && sha=$(git -C "$oxa" rev-parse HEAD 2>/dev/null) && [ -f "$here/xfail-$sha.$mode.txt" ]; then
  xfail=$here/xfail-$sha.$mode.txt
fi
[ -z "$xfail" ] || xfail=$(cd "$(dirname "$xfail")" && pwd)/$(basename "$xfail")
echo "oxa contract: mode $mode, expected failures: ${xfail:-none (strict)}"

if [ -z "$work" ]; then work=$(mktemp -d "${TMPDIR:-/tmp}/oxa-contract.XXXXXX"); fi
mkdir -p "$work"; work=$(cd "$work" && pwd)
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in "${pids[@]}"; do wait "$p" 2>/dev/null || true; done
  if [ "$keep" = 0 ]; then rm -rf "$work"; else echo "kept $work"; fi
}
trap cleanup EXIT
trap 'exit 143' INT TERM

if [ -z "$server" ]; then
  (cd "$repo/engine" && cargo build --release --locked -p server --bin oxe-server)
  server="${CARGO_TARGET_DIR:-$repo/engine/target}/release/oxe-server"
fi
[ -x "$server" ] || { echo "no oxe-server at $server" >&2; exit 2; }

free_port() { python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
wait_http() { # url, seconds
  local i
  for ((i = 0; i < $2 * 10; i++)); do
    if python3 -I -c 'import sys,urllib.request; urllib.request.urlopen(sys.argv[1], timeout=2)' "$1" 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  echo "not ready: $1" >&2; return 1
}

N_CTX=32768
ALIAS=ornith-35b-a3b-local  # the controller stack's selected alias (oxc/tests/openai_api_stack.py)
mkdir -p "$work/home" "$work/logs"
python3 -I "$repo/engine/tools/ci/synth_qwen36_model.py" --fixture "$repo/engine/tests/fixtures/qwen36-tokenizer.json.gz" \
  --out "$work/synth-qwen36.gguf" >"$work/logs/synth.log" 2>&1

eport=$(free_port)
HOME="$work/home" XDG_CACHE_HOME="$work/home/.cache" OXE_SSE_KEEPALIVE_SECS=0.2 \
  "$server" --engine stub --model "$work/synth-qwen36.gguf" --alias "$ALIAS" --ctx-size "$N_CTX" --reasoning off \
  --stub-prefill-us 150 --port "$eport" >"$work/logs/engine.log" 2>&1 &
pids+=($!)
wait_http "http://127.0.0.1:$eport/health" 60

cport=
if [ "$mode" != engine ]; then
cport=$(free_port)
HOME="$work/home" OXC_QUEUE=1 OXC_LEDGER_DIR="$work/ledger" PYTHONPATH="$repo/oxc/src:$repo/oxc/tests" \
  python3 "$repo/oxc/tests/openai_api_stack.py" --backend-port "$eport" --port "$cport" >"$work/logs/controller.log" 2>&1 &
pids+=($!)
wait_http "http://127.0.0.1:$cport/v1/models" 60
fi

# scratch copy of oxa (the checkout is never written) + the contract test
mkdir -p "$work/oxa"
(cd "$oxa" && tar --exclude=./target --exclude=./.git -cf - .) | (cd "$work/oxa" && tar -xmf -)  # -m: fresh mtimes, so a shared target dir never reuses another copy's build
extra=""
if grep -Eq 'pub preserve_thinking: bool' "$work/oxa/oxa-proto/src/engine.rs" 2>/dev/null; then extra="preserve_thinking: true,"; fi
sed "s|/\*OXA_CHAT_REQUEST_EXTRA\*/|$extra|" "$here/stub_contract.rs" >"$work/oxa/oxa-core/tests/zz_stub_contract.rs"

set +e
(cd "$work/oxa" && CARGO_TARGET_DIR="${OXA_CARGO_TARGET_DIR:-$work/oxa-target}" \
  OXA_CONTRACT_ENGINE_URL="http://127.0.0.1:$eport" OXA_CONTRACT_CTL_URL="${cport:+http://127.0.0.1:$cport}" OXA_CONTRACT_MODE="$mode" OXA_CONTRACT_N_CTX="$N_CTX" \
  cargo test --locked -p oxa-core --test zz_stub_contract -- --test-threads=1) 2>&1 | tee "$work/logs/test.log"
rc=${PIPESTATUS[0]}
set -e

# Fail-closed verdict (evaluate.py): exit code, completion marker, the executed set against the
# pinned list tests.txt, and the failed set against the xfail list.
set +e
line=$(python3 -I "$here/evaluate.py" --log "$work/logs/test.log" --rc "$rc" --expected "$here/tests.txt" ${xfail:+--xfail "$xfail"})
verdict=$?
set -e
[ -n "$line" ] || { line="OXA CONTRACT RESULT: FAIL the verdict script failed"; verdict=1; }
echo "$line"
[ -z "${GITHUB_STEP_SUMMARY:-}" ] || echo "$line" >>"$GITHUB_STEP_SUMMARY"
exit "$verdict"
