#!/usr/bin/env bash
# Canonical archive order, timestamps, ownership and gzip header.
set -euo pipefail
: "${SOURCE_DATE_EPOCH:?}"
tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
  -C "$1" -cf - usr | gzip -n -9 > "$2"
