#!/usr/bin/env bash
# Gate T1.5 "tiny-real" (gate ladder: between T1 model-free and T2 one-depth numerics).
# Real Qwen3.5-2B (Apache-2.0, unsloth/Qwen3.5-2B-MTP-GGUF Q4_K_M) rewritten as qwen35moe by
# lab/tools/tiny_real/convert_dense_to_qwen35moe.py; see lab/docs/TINY-REAL.md. ~3-10 min.
#
#   1. identity: both GGUFs match their pinned sha256 (no silent model drift)
#   2. rewrite vs dense, both in llama.cpp: mean KLD on 3 prompts <= 2x llama.cpp's own
#      batched-vs-single noise on the dense file (the quality-gate rule)
#   3. oxe vs llama.cpp on the rewritten file: teacher-forced verify, every logit row
#      bit-identical and the greedy continuation identical (3 prompts); MTP depths 1-3 identical
#   4. oxe-server (temporary port + state dir): tool call, ECHO/C3 appends, SIGTERM, restart on
#      the same state store, resume from disk with a C3 hit
#   5. oxa loop (lab/tools/tiny_real/oxa_tiny_loop.py: read/edit/bash in a scratch repo, forced
#      compaction, post-compaction prefix reuse, a subagent, cancel mid-stream, C3 on every
#      append, no prefix rebase): runs when OXA_BIN (or a custom OXA_TINY_LOOP) is set;
#      otherwise SKIP with a message (not a PASS of that step)
#
# Never touches production: no port 8093, no inference lease, no ~/.config/local-ai; a private
# HOME / XDG tree and weight cache under a temporary directory, removed at exit together with
# every process it started. Run in a DEV/QUALITY window while the hardware is idle.
#
# Env: TINY_DIR       dir with the two GGUFs (default /var/lib/local-ai-models/tiny, else
#                     ~/.local/share/local-ai-models/tiny)
#      OXE_BIN_DIR    dir with release `oxe` and `oxe-server` built from this tree
#                     (default engine/target/release)
#      DUMP_REF       llama.cpp oracle (engine/tools/oracle/build.sh; required)
#      PORT           temporary server port (default 18793; 8093 is refused)
#      THREADS        llama.cpp oracle threads (default 4)
#      OXA_BIN        oxa binary (oxa/: cargo build --release -p oxa-cli); enables step 5
#      OXA_TINY_LOOP  optional other driver: OXA_TINY_LOOP BASE_URL MODEL WORKDIR, exit 0 = PASS
#      HOP_PROBE=1    optional hop breakdown (tools/gates/hop_probe.py): the server moves to PORT+1, a
#                     logging proxy takes PORT, oxa's RPC events are kept (oxa_rpc_tee.py). Prints the
#                     receipt to HOP_PROBE_OUT (default: stdout) and never runs otherwise
#      HOP_PROBE_DUMP=1  with HOP_PROBE: also write transcript.jsonl (request + reply content) to
#                     HOP_PROBE_OUT_DIR (default: the gate's temporary tree, removed at exit)
# Output: one line per step and a final `TINY-REAL T1.5: PASS|FAIL|PASS (oxa SKIP)`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOLS="$ROOT/lab/tools/tiny_real"
GATES="$ROOT/tools/gates"
SRC_NAME=Qwen3.5-2B-Q4_K_M.gguf
MOE_NAME=Qwen3.5-2B-Q4_K_M-qwen35moe-8of16.gguf
SRC_SHA=18c92228fc1498a02506f64593f7cc8cc02f406f732a217c6ba99ad1254d4f0b   # HF rev e05864f8066d874d5f85aaff007ae57a2a7d1efe
MOE_SHA=5e56dc636d7e3af306c5c04d3ac4be7460e6f1ad3d88f44bf3584f38e698e745   # converter v1, --experts 8 (16 slots)
PORT="${PORT:-18793}"
HOP_PROBE="${HOP_PROBE:-0}"
SRV_PORT="$PORT"
[ "$HOP_PROBE" = 1 ] && SRV_PORT=$((PORT + 1))
PROXY_PID=""
THREADS="${THREADS:-4}"
OXE_BIN_DIR="${OXE_BIN_DIR:-$ROOT/engine/target/release}"
if [ -z "${TINY_DIR:-}" ]; then
    TINY_DIR=/var/lib/local-ai-models/tiny
    [ -f "$TINY_DIR/$MOE_NAME" ] || TINY_DIR="$HOME/.local/share/local-ai-models/tiny"
fi
SRC="$TINY_DIR/src/$SRC_NAME"
MOE="$TINY_DIR/$MOE_NAME"
PY="nice -n 19 python3 -I"

fail() { echo "TINY-REAL T1.5: FAIL $*"; exit 1; }
for p in "$PORT" "$SRV_PORT"; do   # proxy and engine ports: never a production port
    case "$p" in 8092|8093|8094) fail "port $p is production (8092 controller, 8093/8094 engines)" ;; esac
done
[ -x "${DUMP_REF:-}" ] || fail "DUMP_REF (llama.cpp oracle, engine/tools/oracle/build.sh) not set or not executable"
[ -x "$OXE_BIN_DIR/oxe" ] && [ -x "$OXE_BIN_DIR/oxe-server" ] || fail "no oxe / oxe-server in $OXE_BIN_DIR (cargo build --release -p oxe-cli -p server)"
[ -f "$SRC" ] && [ -f "$MOE" ] || fail "models missing in $TINY_DIR (see lab/docs/TINY-REAL.md: download + convert)"
if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then fail "port $PORT is in use"; fi
if [ "$HOP_PROBE" = 1 ] && (exec 3<>"/dev/tcp/127.0.0.1/$SRV_PORT") 2>/dev/null; then fail "port $SRV_PORT (hop probe server) is in use"; fi
case "$HOP_PROBE" in 0|1) ;; *) fail "HOP_PROBE must be 0 or 1" ;; esac

W="$(mktemp -d "${TMPDIR:-/tmp}/tiny-real-XXXXXX")"
SRV_PID=""
cleanup() {
    if [ -n "$PROXY_PID" ] && kill -0 "$PROXY_PID" 2>/dev/null; then kill -TERM "$PROXY_PID" 2>/dev/null || true; fi
    if [ -n "$SRV_PID" ] && kill -0 "$SRV_PID" 2>/dev/null; then
        kill -TERM -- "-$SRV_PID" 2>/dev/null || kill -TERM "$SRV_PID" 2>/dev/null || true
        for _ in $(seq 30); do kill -0 "$SRV_PID" 2>/dev/null || break; sleep 1; done
        kill -KILL -- "-$SRV_PID" 2>/dev/null || true
    fi
    rm -rf "$W"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
mkdir -p "$W/home/state" "$W/home/.config" "$W/home/.local/state" "$W/home/run" "$W/cache"
chmod 700 "$W/home/run"
export HOME="$W/home" XDG_CONFIG_HOME="$W/home/.config" XDG_STATE_HOME="$W/home/.local/state" \
       XDG_RUNTIME_DIR="$W/home/run" XDG_CACHE_HOME="$W/cache"
unset LOCAL_LLM_CONFIG LOCAL_AI_LEASE OXE_API_KEY_FILE BRAVE_SEARCH_API_KEY || true  # O170: never inherited, never read
t0=$(date +%s)

# 1. identity
for pair in "$SRC:$SRC_SHA" "$MOE:$MOE_SHA"; do
    f=${pair%%:*}; want=${pair##*:}
    got=$(nice -n 19 sha256sum "$f" | cut -d' ' -f1)
    [ "$got" = "$want" ] || fail "sha256 of $(basename "$f") is $got, pinned $want"
done
echo "step 1 identity: PASS (source + rewrite sha256 pinned)"

# prompts: chat-formatted code / arithmetic, plus a raw completion
printf '<|im_start|>user\nWrite a Python function that returns the n-th Fibonacci number iteratively.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n' > "$W/p1.txt"
printf '<|im_start|>user\nWhat is 17 * 23? Explain step by step.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n' > "$W/p2.txt"
printf 'The capital of Norway is Oslo. The capital of Sweden is' > "$W/p3.txt"
ref() {  # ref MODEL OUT PROMPT [--batch]
    nice -n 19 "$DUMP_REF" --model "$1" --out "$2" --prompt-file "$3" --logits --gen 24 --threads "$THREADS" ${4:-} \
        > "$2.log" 2>&1 || fail "llama.cpp oracle failed on $(basename "$1") ($(tail -1 "$2.log"))"
}

# 2. rewrite vs dense (llama.cpp), bounded by llama.cpp's own batch noise
for p in p1 p2 p3; do
    ref "$SRC" "$W/dense-$p" "$W/$p.txt"
    ref "$SRC" "$W/denseb-$p" "$W/$p.txt" --batch
    ref "$MOE" "$W/moe-$p" "$W/$p.txt"
    nz=$($PY "$TOOLS/compare_logits.py" "$W/dense-$p" "$W/denseb-$p" --json --max-mean-kld 1e9) \
        || fail "step 2 $p: llama.cpp noise reference not comparable ($nz)"
    noise=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["mean_kld"])' "$nz")
    bound=$(python3 -c "print(max(2*$noise, 1e-6))")
    out=$($PY "$TOOLS/compare_logits.py" "$W/dense-$p" "$W/moe-$p" --json --max-mean-kld "$bound") \
        || fail "step 2 $p: rewrite vs dense mean KLD above 2x llama.cpp batch noise ($out)"
    echo "step 2 $p rewrite-vs-dense: PASS $(python3 -c "import json,sys; r=json.loads(sys.argv[1]); print('mean_kld=%.3g max_kld=%.3g top1=%.3f bound=%s' % (r['mean_kld'], r['max_kld'], r['top1_agree'], sys.argv[2]))" "$out" "$bound")"
done

# 3. oxe vs llama.cpp on the rewrite: bit-identical logits, identical greedy; MTP identical
for p in p1 p2 p3; do
    v=$(nice -n 19 "$OXE_BIN_DIR/oxe" verify --model "$MOE" --ref "$W/moe-$p" 2> "$W/verify-$p.log") \
        || fail "step 3 $p: oxe verify crashed ($(tail -1 "$W/verify-$p.log"))"
    python3 -c '
import json, sys
r = json.loads(sys.argv[1])
ok = r["greedy_identical"] and r["bit_exact_logit_rows"] == r["positions_with_logits"] and r["max_kld"] == 0.0
print("step 3 %s oxe-vs-llama.cpp: %s rows %d/%d bit-identical, greedy_identical %s" % (sys.argv[2], "PASS" if ok else "FAIL",
      r["bit_exact_logit_rows"], r["positions_with_logits"], r["greedy_identical"]))
sys.exit(0 if ok else 1)' "$v" "$p" || fail "step 3 $p: oxe differs from llama.cpp on the rewritten file"
done
m=$(nice -n 19 "$OXE_BIN_DIR/oxe" mtp-check --model "$MOE" --refs "$W/moe-p1,$W/moe-p2,$W/moe-p3" --n 64 --depths 1,2,3 2> "$W/mtp.log") \
    || fail "step 3 mtp-check crashed ($(tail -1 "$W/mtp.log"))"
python3 -c 'import json,sys; sys.exit(0 if json.loads(sys.argv[1]).get("all_identical") else 1)' "$m" \
    || fail "step 3 MTP: speculative output differs from plain greedy"
echo "step 3 MTP depths 1-3: PASS (ids + logits identical to plain greedy)"

# 4. oxe-server: tool call, ECHO / C3 appends, restart, resume from disk
start_server() {
    setsid nice -n 10 "$OXE_BIN_DIR/oxe-server" --engine oxe --model "$MOE" --alias tiny --host 127.0.0.1 --port "$SRV_PORT" \
        --ctx-size 16384 --state-cache "$W/home/state" --state-min-tokens 64 --spec-type draft-mtp --spec-draft-n-max 2 \
        --reasoning-format deepseek --prefix-cache "$W/home/prefix" --prefix-cache-min-tokens 256 \
        > "$W/server-$1.log" 2>&1 < /dev/null &
    SRV_PID=$!
    for _ in $(seq 120); do
        kill -0 "$SRV_PID" 2>/dev/null || fail "step 4: server exited at start ($(tail -1 "$W/server-$1.log"))"
        if curl -sf "http://127.0.0.1:$SRV_PORT/health" > /dev/null; then return 0; fi
        sleep 1
    done
    fail "step 4: server not ready in 120 s"
}
stop_server() {
    kill -TERM "$SRV_PID"
    for _ in $(seq 60); do kill -0 "$SRV_PID" 2>/dev/null || { SRV_PID=""; return 0; }; sleep 1; done
    fail "step 4: server did not exit on SIGTERM within 60 s"
}
# hop probe: a server restart / client start resets the proxy's chat->chat gap chain (no fake client time)
hop_epoch() { if [ "$HOP_PROBE" = 1 ]; then curl -sf -X POST "http://127.0.0.1:$PORT/_hop_probe/epoch" > /dev/null || true; fi; }
start_server 1
if [ "$HOP_PROBE" = 1 ]; then
    mkdir -p "$W/hops"
    setsid nice -n 10 $PY "$GATES/hop_probe.py" proxy --listen "$PORT" --upstream "$SRV_PORT" --dir "$W/hops" --transcript \
        > "$W/hops/proxy.log" 2>&1 < /dev/null &
    PROXY_PID=$!
    for _ in $(seq 20); do curl -sf "http://127.0.0.1:$PORT/health" > /dev/null && break; sleep 0.5; done
    curl -sf "http://127.0.0.1:$PORT/health" > /dev/null || fail "step 4: hop probe proxy not up on $PORT"
    echo "hop probe: proxy on $PORT -> server on $SRV_PORT"
fi
$PY "$TOOLS/echo_restart_loop.py" --base-url "http://127.0.0.1:$PORT" --state "$W/loop.json" --phase pre || fail "step 4 (pre-restart loop)"
stop_server
start_server 2
hop_epoch
$PY "$TOOLS/echo_restart_loop.py" --base-url "http://127.0.0.1:$PORT" --state "$W/loop.json" --phase post || fail "step 4 (post-restart resume)"
echo "step 4 server loop + restart: PASS"

# 5. oxa loop (optional until oxa ships): OXA_BIN alone selects the in-tree driver
oxa=SKIP
if [ -z "${OXA_TINY_LOOP:-}" ] && [ -n "${OXA_BIN:-}" ]; then OXA_TINY_LOOP="$TOOLS/oxa_tiny_loop.py"; fi
if [ -n "${OXA_TINY_LOOP:-}" ]; then
    [ -x "$OXA_TINY_LOOP" ] || fail "OXA_TINY_LOOP=$OXA_TINY_LOOP is not executable"
    mkdir -p "$W/oxa"
    oxa_bin="${OXA_BIN:-}"
    if [ "$HOP_PROBE" = 1 ] && [ -n "$oxa_bin" ]; then
        mkdir -p "$W/hops"
        export OXA_REAL="$oxa_bin" HOP_RPC_OUT="$W/hops/oxa-rpc.jsonl"
        oxa_bin="$GATES/oxa_rpc_tee.py"
        export OXA_BIN="$oxa_bin"
    fi
    hop_epoch
    nice -n 10 "$OXA_TINY_LOOP" "http://127.0.0.1:$PORT/v1" tiny "$W/oxa" || fail "step 5: oxa loop"
    oxa=PASS
    echo "step 5 oxa loop: PASS"
else
    echo "step 5 oxa loop: SKIP (neither OXA_BIN nor OXA_TINY_LOOP set; this gate run does NOT cover oxa)"
fi
stop_server
if [ "$HOP_PROBE" = 1 ]; then
    [ -n "$PROXY_PID" ] && kill -TERM "$PROXY_PID" 2>/dev/null || true
    PROXY_PID=""
    out="${HOP_PROBE_OUT:-/dev/stdout}"
    logs=(); for f in "$W"/server-*.log; do [ -f "$f" ] && logs+=(--server-log "$f"); done
    nice -n 19 python3 -I "$GATES/hop_probe.py" summarize --dir "$W/hops" "${logs[@]}" --rpc "$W/hops/oxa-rpc.jsonl" --out "$out" \
        || echo "hop probe: summary failed (gate result unaffected)"
    if [ "${HOP_PROBE_DUMP:-0}" = 1 ] && [ -f "$W/hops/transcript.jsonl" ]; then
        dump="${HOP_PROBE_OUT_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/hop-probe-dump-XXXXXX")}"
        mkdir -p "$dump" && chmod 700 "$dump" && cp "$W/hops/transcript.jsonl" "$dump/transcript.jsonl" && chmod 600 "$dump/transcript.jsonl"
        echo "hop probe: transcript copied to $dump/transcript.jsonl (gate content; keep it private)"
    fi
fi
echo "elapsed $(( $(date +%s) - t0 )) s"
if [ "$oxa" = PASS ]; then echo "TINY-REAL T1.5: PASS"; else echo "TINY-REAL T1.5: PASS (oxa SKIP)"; fi
