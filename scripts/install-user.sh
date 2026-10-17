#!/usr/bin/env bash
# Install the production `ktask` supervisor for the current user.
#
# The executable is copied outside this checkout's build outputs so that
# `cargo build`/`cargo run` never replace it. A supervisor that is currently
# running from the install path is never overwritten.
#
# Usage: scripts/install-user.sh [--root DIR]
#   --root DIR   Install into DIR/bin/ktask (default: $CARGO_INSTALL_ROOT,
#                else $CARGO_HOME, else ~/.cargo).
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
root=""
while [ $# -gt 0 ]; do
    case "$1" in
        --root)
            [ $# -ge 2 ] || { echo "install: --root needs a directory" >&2; exit 2; }
            root=$2
            shift 2
            ;;
        -h | --help)
            sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "install: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done
if [ -z "$root" ]; then
    root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}
fi
case "$root" in
    /*) ;;
    *) root=$PWD/$root ;;
esac

fail() {
    echo "install: $*" >&2
    exit 1
}

for tool in cargo git; do
    command -v "$tool" > /dev/null 2>&1 || fail "$tool is required but not on PATH"
done
echo "Toolchain: $(rustc --version) / $(cargo --version)"
if command -v codex > /dev/null 2>&1; then
    echo "Codex CLI: $(command -v codex) ($(codex --version 2> /dev/null | head -n 1 || echo 'version unknown'))"
else
    echo "Warning: codex is not on PATH; the supervisor needs an authenticated Codex CLI to run AI work." >&2
fi

mkdir -p "$root/bin"
root=$(cd -- "$root" && pwd -P)
target=$root/bin/ktask
case "$target" in
    "$repo"/target/*) fail "refusing to install into this checkout's build outputs ($target)" ;;
esac

# Never replace a supervisor while it is running: a live run would keep the old
# inode, and detached guardians/workers rely on the exact executable path.
live=""
for proc in /proc/[0-9]*; do
    exe=$(readlink "$proc/exe" 2> /dev/null) || continue
    case "$exe" in
        "$target" | "$target (deleted)") live="$live ${proc#/proc/}" ;;
    esac
done
if [ -n "$live" ]; then
    fail "ktask is running from $target (pid$live); pause or finish its run before reinstalling"
fi

echo "Installing $repo -> $target"
cargo install --locked --path "$repo" --bin ktask --root "$root"

paths=$("$target" --paths) || fail "installed executable failed to report its paths"
instance=$(printf '%s\n' "$paths" | awk -F '\t' '$1 == "instance" { print $2 }')
executable=$(printf '%s\n' "$paths" | awk -F '\t' '$1 == "executable" { print $2 }')
[ "$instance" = "ktask" ] || fail "installed executable reports instance '$instance', expected ktask"
[ "$(readlink -f -- "$executable")" = "$(readlink -f -- "$target")" ] \
    || fail "installed executable reports $executable, expected $target"
echo
printf '%s\n' "$paths"
echo
case ":$PATH:" in
    *":$root/bin:"*) echo "Installed: ktask -> $target" ;;
    *) echo "Installed: $target (add $root/bin to PATH to run it as ktask)" ;;
esac
echo "Next: run ktask inside a Git project, or scripts/self-host.sh to supervise this checkout."
