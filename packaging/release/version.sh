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
# A pull-request run gets a name of its own, so its build proves the stamp (`oxa --version`
# prints it) instead of the package-version fallback; releases and dev builds pass the name in.
if [ "${GITHUB_EVENT_NAME:-}" = pull_request ] && [ -n "${GITHUB_SHA:-}" ]; then
    printf 'v%s-pr+%s\n' "$version" "$(printf %s "$GITHUB_SHA" | cut -c1-7)"
    exit 0
fi
printf 'v%s\n' "$version"
