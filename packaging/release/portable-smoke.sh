#!/usr/bin/env bash
# Start the portable `oxa` in clean images and run one `-p` prompt against the fake engine.
# Usage: portable-smoke.sh DIR   (DIR holds oxa and oxa-fake from portable-build.sh)
set -euo pipefail
dir=$(cd "${1:?usage: portable-smoke.sh DIR}" && pwd)
work=$(mktemp -d)
holder=
cleanup() { [[ -n $holder ]] && kill "$holder" 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT
chmod +x "$dir/oxa" "$dir/oxa-fake"
for _ in 1 2 3 4 5 6; do echo '{"text":"portable ok\n"}'; done > "$work/script.jsonl"
# The fake serves until its stdin closes.
sleep 600 | "$dir/oxa-fake" --script "$work/script.jsonl" --model portable > "$work/fake.log" &
holder=$!
url=
for _ in $(seq 1 50); do
  url=$(sed -n 's/^listening //p' "$work/fake.log" | head -n1)
  [[ -n $url ]] && break
  sleep 0.2
done
[[ -n $url ]] || { echo "fake engine did not start" >&2; cat "$work/fake.log" >&2; exit 1; }
for image in debian:bullseye-slim ubuntu:22.04 python:3.12-slim; do
  echo "== $image"
  docker run --rm --network host -v "$dir:/opt/oxa:ro" \
    -e OXA_ENGINE_URL="$url" -e OXA_ENV_NOTE=0 -e OXA_SYSTEM_SKILLS_DIR= -e HOME=/tmp/home \
    "$image" sh -c '
      set -e
      mkdir -p /tmp/home /tmp/work && cd /tmp/work
      /opt/oxa/oxa --version
      out=$(/opt/oxa/oxa -p --no-session --no-tools --no-skills --model portable "reply briefly")
      echo "$out"
      case "$out" in *"portable ok"*) ;; *) echo "unexpected reply" >&2; exit 1;; esac
    '
done
echo "portable binary starts in three images"
