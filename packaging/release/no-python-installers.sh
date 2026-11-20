#!/usr/bin/env bash
# Fail when a package recipe or release script needs or invokes Python.
#   packaging/release/no-python-installers.sh
# 1. packaging/arch/*/PKGBUILD names python in depends, makedepends, optdepends or checkdepends.
# 2. packaging/release/*.sh and tools/lab/dev-update*.sh invoke python (comment lines are ignored;
#    no-python*.sh files describe the rules and are skipped).
# The built .PKGINFO is checked for the same thing in packaging/release/arch-test.sh.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
fail=0
for recipe in packaging/arch/*/PKGBUILD; do
  awk -v file="$recipe" '
    /^[ \t]*(make|opt|check)?depends(_[a-z0-9_]+)?=\(/ { inside = 1 }
    inside && /["\x27]python[0-9.]*(["\x27<>=:])/ { print "no-python: " file ": " $0; bad = 1 }
    inside && /\)/ { inside = 0 }
    END { exit bad }
  ' "$recipe" || fail=1
done
for script in packaging/release/*.sh tools/lab/dev-update*.sh; do
  [ -f "$script" ] || continue
  case ${script##*/} in no-python*) continue;; esac
  if grep -nE '^[^#]*(^|[^[:alnum:]_./-])python3?([[:space:]]|$)|^[^#]*/usr/bin/(env )?python|^[^#]*\bpython3? -' "$script"; then
    echo "no-python: $script invokes python" >&2
    fail=1
  fi
done
[ "$fail" -eq 0 ] && echo "no-python-installers: PASS"
exit "$fail"
