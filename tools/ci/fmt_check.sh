#!/bin/sh
# `cargo fmt --check` for the workspace the caller is in, from the repository root or from any
# directory inside a checkout or a linked worktree.
#
# CI gets cargo from rustup's shims on PATH. Two local setups fail the declared check for reasons
# that say nothing about the source: no cargo directory on PATH ("cargo: command not found", exit
# 127), and a member or linked-worktree directory, where `cargo fmt` finds no targets or checks
# one package instead of the workspace. The check itself is unchanged: `cargo fmt --check` over
# the whole workspace, run from its root.
#
# The workspace is chosen from the caller's directory: the nearest Cargo.toml above it that
# declares `[workspace]`. From a directory with no workspace above it (the repository root, which
# holds several), the checkout that holds this helper names it: the sibling `oxa`, else the only
# workspace directly below that checkout.
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

is_workspace() {
    [ -f "$1/Cargo.toml" ] && grep -q '^\[workspace\]' "$1/Cargo.toml"
}

# The checkout this helper belongs to, including linked worktrees whose .git entry is a file.
helper=$0
case $helper in
    */*) ;;
    *) helper=$(command -v -- "$helper" 2>/dev/null || true); helper=${helper:-$0} ;;
esac
checkout=$(CDPATH= cd -- "$(dirname -- "$helper")/../.." 2>/dev/null && pwd) || checkout=

# Walk up from the caller: a member directory's Cargo.toml is not the workspace.
dir=$PWD
while :; do
    if is_workspace "$dir"; then
        cd "$dir"
        exec cargo fmt --check "$@"
    fi
    [ "$dir" = "/" ] && break
    dir=$(dirname -- "$dir")
done

# No workspace above the caller: the helper's checkout names it, by the conventional folder
# beside the helper or, when that does not exist, as the only workspace directly below it.
if [ -n "$checkout" ]; then
    if is_workspace "$checkout/oxa"; then
        cd "$checkout/oxa"
        exec cargo fmt --check "$@"
    fi
    only=
    ambiguous=
    for candidate in "$checkout"/*; do
        is_workspace "$candidate" || continue
        if [ -n "$only" ]; then
            ambiguous=1
        else
            only=$candidate
        fi
    done
    if [ -n "$only" ] && [ -z "$ambiguous" ]; then
        cd "$only"
        exec cargo fmt --check "$@"
    fi
fi
echo "fmt_check: no Cargo.toml declaring a workspace above $PWD, and none to fall back to beside this helper" >&2
exit 1
