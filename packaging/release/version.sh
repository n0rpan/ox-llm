#!/bin/sh
set -eu

version=${1-}
manifest=${2-oxa/Cargo.toml}

if [ -n "$version" ]; then
    printf '%s\n' "$version"
    exit 0
fi

version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$manifest" | head -1)
if [ -z "$version" ]; then
    echo "no workspace version in $manifest" >&2
    exit 1
fi
printf 'v%s\n' "$version"
