#!/bin/sh
# `cargo fmt --check` for the oxa workspace, from the repository root or from any directory
# inside a checkout or a linked worktree.
#
# `cargo fmt` is an external subcommand: cargo runs the `cargo-fmt` binary it finds on PATH,
# never one beside itself. So the check needs both binaries reachable by name, which is not
# guaranteed when rustup's shims are not on PATH, or when cargo and the rustfmt component came
# from different installs (one toolchain's `bin` holds cargo, another holds cargo-fmt).
# `cargo` alone then fails with "no such command: `fmt`" or "cargo-fmt: not found", which says
# nothing about the source.
#
# The wrapper resolves each binary on PATH first (CI gets both from the rustup shims), then in
# CARGO_HOME/bin and the `bin` of every rustup toolchain under RUSTUP_HOME, $HOME and the
# account home from the passwd database (a sandbox's $HOME is not the account's), in that
# order; a binary that is missing is named in the error. It then walks up to the manifest that
# declares the workspace (a member directory's Cargo.toml is not it, and in a linked worktree
# there is no other way to name the workspace root) and runs the unchanged
# `cargo fmt --check` there.
set -eu

export PATH

add_dir() {
    case ":$PATH:" in
        *":$1:"*) ;;
        *) PATH=$1:$PATH ;;
    esac
}

# The account's home from the password database, for a sandbox whose HOME differs from the
# account that installed the toolchain. Not a literal path.
account=
if command -v getent >/dev/null 2>&1 && command -v id >/dev/null 2>&1; then
    account=$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6) || account=
fi

# Root and toolchain `bin` directories in search order (unset roots are skipped).
toolchain_dirs() {
    for root in "${CARGO_HOME:-}" "${RUSTUP_HOME:-}" "${HOME:-}" "$account"; do
        [ -n "$root" ] || continue
        printf '%s\n' "$root/bin"
        for bin in "$root"/.rustup/toolchains/*/bin "$root"/toolchains/*/bin; do
            [ -d "$bin" ] && printf '%s\n' "$bin"
        done
    done
    # The last glob test may be false; the list is still complete.
    return 0
}

# Name each binary on PATH unless PATH already resolves it: `cargo fmt` looks the subcommand up
# there, and a toolchain's `bin` in front keeps a toolchain that holds both together.
dirs=$(toolchain_dirs)
for name in cargo cargo-fmt; do
    command -v "$name" >/dev/null 2>&1 && continue
    found=
    while IFS= read -r dir; do
        if [ -x "$dir/$name" ]; then
            add_dir "$dir"
            found=$dir
            break
        fi
    done <<EOF
$dirs
EOF
    [ -n "$found" ] || {
        case "$name" in
            cargo-fmt)
                echo "fmt_check: cargo-fmt not found on PATH or in a rustup toolchain, so \`cargo fmt\` cannot run its external subcommand. Install the rustfmt component (rustup component add rustfmt) or put the directory holding cargo-fmt on PATH" >&2
                ;;
            *)
                echo "fmt_check: cargo not found on PATH, in CARGO_HOME or in a rustup toolchain" >&2
                ;;
        esac
        exit 127
    }
done

dir=$PWD
while :; do
    if [ -f "$dir/Cargo.toml" ] && grep -q '^\[workspace\]' "$dir/Cargo.toml"; then
        cd "$dir"
        exec cargo fmt --check "$@"
    fi
    if [ "$dir" = "/" ]; then
        break
    fi
    case "$dir" in
        */*) dir=${dir%/*} ;;
        *) dir=/ ;;
    esac
    [ -n "$dir" ] || dir=/
done
echo "fmt_check: no Cargo.toml declaring a workspace above $PWD" >&2
exit 1
