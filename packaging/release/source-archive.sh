#!/usr/bin/env bash
# The immutable source archive every release gate builds from: the tracked HEAD tree, without
# the vendored engine sources and the Python tooling no native artifact ships.
#   packaging/release/source-archive.sh <output.tar.gz>
# One definition feeds the Fedora build, the Arch source package and the release lane, so the
# source package compiles exactly the tree the artifacts were built from.
set -euo pipefail
: "${1:?usage: source-archive.sh <output.tar.gz>}"
commit=$(git rev-parse HEAD)
mkdir -p "$(dirname "$1")"
git archive --format=tar --prefix="ox-llm-$commit/" HEAD -- . \
  ':(exclude)oxc/src' ':(exclude)oxc/tools' \
  ':(exclude)oxc/packaging/flash-qos/oxc-flash-qos' \
  ':(exclude,glob)oxc/**/*.py' | gzip -n -9 > "$1"
