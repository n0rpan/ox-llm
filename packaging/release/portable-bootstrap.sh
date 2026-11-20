#!/usr/bin/env bash
# Prepare debian:bullseye-slim for the portable build and run it.
#
# The step runs inside the container (`docker run debian:bullseye-slim bash
# packaging/release/portable-bootstrap.sh`), not before `actions/checkout`, so the workflow keeps
# no apt command of its own: the repository checks that every workflow installs packages through
# the retrying `tools/ci/apt_install.sh`, and that script targets Ubuntu runners with `sudo`,
# while this image is Debian 11 (end of life) whose packages live on archive.debian.org.
#
# `HOST_UID`/`HOST_GID` (the runner user, exported by the workflow) make the build output readable
# by the upload step: the container runs as root.
set -euo pipefail

# Debian 11 is end of life: its packages live on archive.debian.org.
printf '%s\n' \
  'deb http://archive.debian.org/debian bullseye main' \
  'deb http://archive.debian.org/debian-security bullseye-security main' \
  > /etc/apt/sources.list
rm -f /etc/apt/sources.list.d/*
apt-get -o Acquire::Check-Valid-Until=false update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
  ca-certificates curl gcc libc6-dev binutils make git

root=${PWD}
# The workspace is owned by the runner user, this process is root: cargo and git both refuse an
# "unowned" checkout without this.
git config --global --add safe.directory "$root"

packaging/release/portable-build.sh

if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ]; then
  chown -R "$HOST_UID:$HOST_GID" "$root/dist-portable"
fi
