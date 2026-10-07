#!/bin/sh
# Install the `user` channel of ktask-rs: ./install-user.sh [DESTINATION]
# Refuses a dirty tree, so what is on PATH always equals a commit. Deploys nothing else.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
destination=${1:-$HOME/.local/bin/ktask-rs}

dirty=$(git -C "$root" status --porcelain)
if [ -n "$dirty" ]; then
    echo "install-user.sh: the tree is dirty; commit or stash these first:" >&2
    echo "$dirty" >&2
    exit 1
fi

# Its own target directory keeps target/release/ free of a `user` binary.
KTASK_RS_CHANNEL=user cargo build --release --locked \
    --manifest-path "$root/Cargo.toml" --package ktask-cli \
    --target-dir "$root/target/install" >&2

mkdir -p "$(dirname -- "$destination")"
install -m 755 "$root/target/install/release/ktask-rs" "$destination"
"$destination" --version
