#!/bin/sh
# The local Rust checks when the toolchain is installed but not on `PATH` (issue #1063).
#
# A rustup-style install keeps the real binaries in `<rustup home>/toolchains/<toolchain>/bin`.
# The `~/.cargo/bin` proxies that normally expose them can be missing or empty, and then the
# documented local checks fail with `cargo: not found` or cargo's `no such command: fmt`,
# although the toolchain works when called by its absolute path. This script finds a directory
# that ships `cargo` and the proxy the subcommand needs, puts it first on `PATH` and execs the
# command as written.
#
#   sh tools/ci/cargo.sh fmt --check
#   sh tools/ci/cargo.sh clippy -p oxa-core -- -D warnings
#
# Nothing is skipped or weakened: with no usable toolchain it exits non-zero and lists every
# directory it looked in, so a missing toolchain never reads as a passing check. Only shell
# builtins are used, so the search itself works on a `PATH` without coreutils.
#
# Lookup order: `RUSTUP_TOOLCHAIN`, the rustup default toolchain, `<cargo home>/bin`, a rustup
# home beside each `<...>/.cargo/bin` entry on `PATH` (rustup's own layout), an installed
# toolchain under the rustup home, then `PATH` itself. `RUSTUP_HOME` and `CARGO_HOME` hold.
set -eu

if [ $# -lt 1 ]; then
    echo "usage: $0 <cargo subcommand> [args...]" >&2
    exit 2
fi

case $1 in
    -*) need=cargo ;; # `--version`, `--list`: no subcommand proxy involved
    *) need=cargo-$1 ;;
esac

rustup_home=${RUSTUP_HOME:-${HOME:-/nonexistent}/.rustup}
cargo_home=${CARGO_HOME:-${HOME:-/nonexistent}/.cargo}
search_path=${PATH:-}

# Every candidate directory, one per line, in lookup order.
candidates() {
    if [ -n "${RUSTUP_TOOLCHAIN:-}" ]; then
        printf '%s\n' "$rustup_home/toolchains/$RUSTUP_TOOLCHAIN/bin"
    elif [ -f "$rustup_home/settings.toml" ]; then
        while IFS= read -r line; do
            case $line in
                default_toolchain*)
                    toolchain=${line#*\"}
                    printf '%s\n' "$rustup_home/toolchains/${toolchain%\"*}/bin"
                    ;;
            esac
        done <"$rustup_home/settings.toml"
    fi
    printf '%s\n' "$cargo_home/bin"
    # rustup keeps `~/.cargo` and `~/.rustup` side by side, so a `cargo` proxy directory on
    # `PATH` names the rustup home its toolchains live in, whatever the current `HOME` is.
    # IFS is a builtin assignment here, so no `tr` is needed to split `PATH`.
    saved=$IFS
    IFS=:
    for entry in $search_path; do
        case $entry in
            */.cargo/bin)
                sibling=${entry%/.cargo/bin}/.rustup
                if [ -d "$sibling/toolchains" ]; then
                    for dir in "$sibling"/toolchains/*/bin; do
                        if [ -d "$dir" ]; then
                            printf '%s\n' "$dir"
                        fi
                    done
                fi
                ;;
        esac
    done
    IFS=$saved
    if [ -d "$rustup_home/toolchains" ]; then
        for dir in "$rustup_home"/toolchains/*/bin; do
            if [ -d "$dir" ]; then
                printf '%s\n' "$dir"
            fi
        done
    fi
    saved=$IFS
    IFS=:
    for entry in $search_path; do
        if [ -n "$entry" ]; then
            printf '%s\n' "$entry"
        fi
    done
    IFS=$saved
}

found=
fallback=
while IFS= read -r dir; do
    if [ -z "$dir" ] || [ ! -d "$dir" ] || [ ! -x "$dir/cargo" ]; then
        continue
    fi
    if [ "$need" = cargo ] || [ -x "$dir/$need" ]; then
        found=$dir
        break
    fi
    if [ -z "$fallback" ]; then
        fallback=$dir
    fi
done <<EOF
$(candidates)
EOF

# `cargo` without its proxy is still better than nothing: cargo names the missing subcommand.
if [ -z "$found" ]; then
    found=$fallback
fi
if [ -z "$found" ]; then
    echo "$0: no Rust toolchain with '$need' found." >&2
    echo "Looked in:" >&2
    candidates | while IFS= read -r dir; do
        echo "  $dir" >&2
    done
    echo "  $rustup_home/toolchains/<toolchain>/bin" >&2
    echo "Put your rustup toolchain's bin directory first on PATH" >&2
    echo "(for example <rustup home>/toolchains/<toolchain>/bin) and run the command again." >&2
    exit 1
fi

PATH=$found:$PATH
export PATH
exec "$found/cargo" "$@"
