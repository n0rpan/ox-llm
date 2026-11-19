#!/usr/bin/env bash
# Exercise installed CLIs and the real loopback stub engine, without a model.
# Needs only bash, coreutils and curl. SMOKE_VERSION overrides the expected version.
set -euo pipefail
export LC_ALL=C
expected=${SMOKE_VERSION:-0.2.0}
sandbox=$(mktemp -d)
fixtures=$(mktemp -d)
server=
cleanup() {
  [ -z "$server" ] || { kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; }
  rm -rf "$sandbox" "$fixtures"
}
trap cleanup EXIT
fail() { echo "smoke: $*" >&2; exit 1; }

# Little-endian integers and length-prefixed strings, as raw bytes on stdout.
le() { # $1 = value, $2 = byte count
  local v=$1 i
  for ((i = 0; i < $2; i++)); do printf "\\x$(printf %02x $((v & 255)))"; v=$((v >> 8)); done
}
str() { le "${#1}" 8; printf '%s' "$1"; }
utf8() { # one code point below 2048
  if (($1 < 128)); then printf "\\x$(printf %02x "$1")"
  else printf "\\x$(printf %02x $((192 | $1 >> 6)))\\x$(printf %02x $((128 | $1 & 63)))"; fi
}

tokenizer_fixture() {
  # GGUF metadata only: a synthetic byte vocabulary, no tensors or weights.
  local out=$1 b extra=0 cp ntok=261 special
  local template="{% for message in messages %}<|im_start|>{{ message['role'] }}
{{ message['content'] }}<|im_end|>
{% endfor %}{% if add_generation_prompt %}<|im_start|>assistant
{% endif %}"
  {
    printf 'GGUF'; le 3 4; le 0 8; le 6 8
    str tokenizer.ggml.model; le 8 4; str gpt2
    str tokenizer.ggml.pre; le 8 4; str qwen2
    str tokenizer.chat_template; le 8 4; str "$template"
    str tokenizer.ggml.tokens; le 9 4; le 8 4; le "$ntok" 8
    for b in $(seq 0 255); do
      if ((b >= 33 && b <= 126)) || ((b >= 161 && b <= 172)) || ((b >= 174 && b <= 255)); then cp=$b
      else cp=$((256 + extra)); extra=$((extra + 1)); fi
      le $((cp < 128 ? 1 : 2)) 8; utf8 "$cp"
    done
    for special in '<|im_start|>' '<|im_end|>' '<|endoftext|>' '<think>' '</think>'; do str "$special"; done
    str tokenizer.ggml.merges; le 9 4; le 8 4; le 0 8
    str tokenizer.ggml.eos_token_id; le 4 4; le 257 4
  } > "$out"
}

model=$fixtures/tokenizer-only.gguf
tokenizer_fixture "$model"
run() { env -i PATH=/usr/bin:/bin HOME="$sandbox" XDG_CONFIG_HOME="$sandbox/config" \
  XDG_STATE_HOME="$sandbox/state" XDG_CACHE_HOME="$sandbox/cache" XDG_DATA_HOME="$sandbox/data" \
  XDG_RUNTIME_DIR="$sandbox/runtime" "$@"; }

for retired in /usr/share/ox-llm/oxc /usr/bin/oxc /usr/bin/oxc-diagnostics; do
  [ ! -e "$retired" ] && [ ! -L "$retired" ] || fail "Legacy Python payload installed: $retired"
done
names=$(ls -A /usr/share/ox-llm/memlat | sort | tr '\n' ' ')
[ "$names" = '05-local-ai-memlat-fallback.hook 20-local-ai-memlat-fallback 85-local-ai-memlat-rebuild 99-z-local-ai-memlat-rebuild.hook ' ] \
  || fail "unexpected memlat templates: $names"
for path in /usr/share/ox-llm/memlat/*; do
  [ -f "$path" ] && [ ! -L "$path" ] && [ "$(stat -c %a "$path")" = 644 ] || fail "bad template: $path"
done
for binary in oxa oxe; do
  out=$(run "$binary" --version)
  case $out in *"$expected"*) ;; *) fail "$binary --version: $out";; esac
  echo "$out"
done
for binary in oxa oxe oxe-server local-ai-memlat-boot; do
  [ "$(head -c 4 "/usr/bin/$binary" | od -An -tx1 | tr -d ' \n')" = 7f454c46 ] \
    || fail "Expected a native Linux executable: $binary"
done
for binary in oxa oxe-server local-ai-memlat-boot; do run "$binary" --help > /dev/null; done
# This validates argument parsing without loading model weights.
run oxe gen --model absent.gguf --tokens 1 --check-args

log=$fixtures/server.log
base=
for attempt in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  run oxe-server --engine stub --model "$model" --alias synthetic --stub-reply ok --reasoning off \
    --host 127.0.0.1 --port "$port" > "$log" 2>&1 &
  server=$!
  for _ in $(seq 1 200); do
    kill -0 "$server" 2>/dev/null || break
    if models=$(curl -fsS --noproxy '*' -m 1 "http://127.0.0.1:$port/v1/models" 2>/dev/null); then
      base=http://127.0.0.1:$port; break 2
    fi
    sleep 0.1
  done
  kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; server=
done
[ -n "$base" ] || { cat "$log" >&2; fail "oxe-server did not answer on loopback"; }
id=$(printf '%s' "$models" | tr -d '\n' | sed -n 's/.*"data" *: *\[ *{[^}]*"id" *: *"\([^"]*\)".*/\1/p')
[ -n "$id" ] || fail "no model listed: $models"
completion=$(curl -fsS --noproxy '*' -m 10 -H 'Content-Type: application/json' \
  -d "{\"model\": \"$id\", \"messages\": [{\"role\": \"user\", \"content\": \"hello\"}], \"max_tokens\": 4}" \
  "$base/v1/chat/completions") || { cat "$log" >&2; fail "chat completion failed"; }
printf '%s' "$completion" | grep -Eq '"choices" *: *\[ *\{' || fail "no choices: $completion"
kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; server=
[ -z "$(ls -A "$sandbox")" ] || fail "smoke commands wrote unexpected user state"
echo "installed model-free smoke passed"
