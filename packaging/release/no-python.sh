#!/bin/sh
# Fail when a release payload contains or calls Python.
#   no-python.sh ARCHIVE.tar.gz|DIR ...   scan release archives or unpacked trees
#   no-python.sh --list FILE              scan the regular files named in FILE (one path per line)
# Rules: no .py/.pyc/.pyo or __pycache__; no python shebang; every file under bin/ or usr/bin/
# is an ELF binary; no controller payload paths; no python invocation in non-ELF scripts,
# units and hooks (licence, notice, readme, manifest and json files are documentation).
set -eu
fail=0
note() { printf 'no-python: %s\n' "$1" >&2; fail=1; }

scan_list() { # $1 = file with one path per line
  while IFS= read -r f; do
    base=${f##*/}
    case $f in
      *.py|*.pyc|*.pyo|*/__pycache__|*/__pycache__/*) note "python file: $f" ;;
    esac
    case $f in
      */usr/share/ox-llm/oxc|*/usr/share/ox-llm/oxc/*|*/usr/bin/oxc|*/usr/bin/oxc-diagnostics|*/usr/bin/local-ai)
        note "controller payload: $f" ;;
    esac
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    magic=$(head -c 4 "$f" | od -An -tx1 | tr -d ' \n')
    first=$(head -n 1 "$f" | tr -d '\000' | head -c 200)
    case $first in
      '#!'*python*) note "python shebang: $f" ;;
    esac
    case $f in
      */usr/bin/*|*/bin/*)
        [ "$magic" = 7f454c46 ] || note "non-native executable: $f" ;;
    esac
    [ "$magic" = 7f454c46 ] && continue
    case $f in
      */licenses/*) continue ;;
    esac
    case $base in
      LICENSE*|COPYING*|NOTICE*|THIRD_PARTY*|README*|*MANIFEST*|*.json|*.txt|*.md) continue ;;
    esac
    if grep -qaE 'python3|/usr/bin/python|/usr/bin/env python' "$f"; then
      note "python invocation: $f"
    fi
  done < "$1"
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
list=$work/files

if [ "${1:-}" = "--list" ]; then
  [ $# -eq 2 ] || { echo "usage: no-python.sh --list FILE" >&2; exit 2; }
  scan_list "$2"
else
  [ $# -ge 1 ] || { echo "usage: no-python.sh ARCHIVE_OR_DIR..." >&2; exit 2; }
  for target in "$@"; do
    if [ -d "$target" ]; then
      root=$target
    else
      [ -f "$target" ] || { echo "no-python: missing $target" >&2; exit 2; }
      root=$work/x; rm -rf "$root"; mkdir "$root"
      tar -tzf "$target" > "$work/names"
      if grep -qE '(^|/)\.\.(/|$)|^/' "$work/names"; then note "unsafe path in $target"; continue; fi
      tar -xzf "$target" -C "$root"
    fi
    find "$root" \( -type f -o -type l -o -type d \) > "$list"
    scan_list "$list"
    echo "no-python: scanned $target"
  done
fi
[ "$fail" -eq 0 ] && echo "no-python: PASS"
exit "$fail"
