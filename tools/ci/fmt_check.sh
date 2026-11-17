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

# Walk up to the manifest that declares the workspace: a member directory's
# Cargo.toml is not it, and in a linked worktree there is no other way to name the
# workspace root.
dir=$PWD
while :; do
    if [ -f "$dir/Cargo.toml" ] && grep -q '^\[workspace\]' "$dir/Cargo.toml"; then
        cd "$dir"
        exec cargo fmt --check "$@"
    fi
    [ "$dir" = "/" ] && break
    dir=$(dirname "$dir")
done
echo "fmt_check: no Cargo.toml declaring a workspace above $PWD" >&2
exit 1
