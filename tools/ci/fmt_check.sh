#!/bin/sh
# `cargo fmt --check` for the oxa workspace, from the repository root or from any
# directory inside a checkout or a linked worktree.
#
# CI gets cargo from rustup's shims on PATH. Two local setups fail the declared
# check for reasons that say nothing about the source: no cargo directory on PATH
# ("cargo: command not found", exit 127), and a member or linked-worktree
# directory, where `cargo fmt` finds no targets or checks one package instead of
# the workspace. The check itself is unchanged: `cargo fmt --check` over the whole
# workspace, run from its root.
set -eu

add_cargo_dir() {
    [ -n "$1" ] && [ -x "$1/cargo" ] || return 1
    PATH=$1:$PATH
    export PATH
}

add_toolchain_bins() {
    [ -n "$1" ] && [ -d "$1" ] || return 1
    for bin in "$1"/*/bin; do
        add_cargo_dir "$bin" && return 0
    done
    return 1
}

if ! command -v cargo >/dev/null 2>&1; then
    # The account's home from the password database, for a sandbox whose HOME differs
    # from the account that ran rustup. Not a literal path.
    account=$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6)
    add_cargo_dir "${CARGO_HOME:+$CARGO_HOME/bin}" ||
        add_cargo_dir "${HOME:+$HOME/.cargo/bin}" ||
        add_cargo_dir "${account:+$account/.cargo/bin}" ||
        add_toolchain_bins "${RUSTUP_HOME:+$RUSTUP_HOME/toolchains}" ||
        add_toolchain_bins "${HOME:+$HOME/.rustup/toolchains}" ||
        add_toolchain_bins "${account:+$account/.rustup/toolchains}" || {
        echo "fmt_check: cargo not found on PATH, in CARGO_HOME or in a rustup toolchain" >&2
        exit 127
    }
fi

# The helper is in tools/ci; resolve the sibling oxa workspace from this checkout,
# including linked worktrees whose .git entry is a file. The caller's cwd is irrelevant.
workspace=$(CDPATH= cd -- "$(dirname -- "$0")/../../oxa" && pwd)
if [ ! -f "$workspace/Cargo.toml" ] || ! grep -q '^\[workspace\]' "$workspace/Cargo.toml"; then
    echo "fmt_check: oxa workspace manifest not found beside this helper" >&2
    exit 1
fi
cd "$workspace"
exec cargo fmt --check "$@"
