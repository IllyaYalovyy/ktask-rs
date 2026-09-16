#!/usr/bin/env bash
# The single entry point for every mechanical check. CI-ready by construction:
# no hosting platform is involved, and nothing here depends on one.
#
#   scripts/quality.sh            all gates
#   scripts/quality.sh fmt test   only the named gates
#
# Gates: fmt build test clippy doc deny unused typos
# Install everything they need with scripts/setup.sh.
set -uo pipefail
cd "$(dirname "$0")/.."

FAILED=()
run() {
  local name="$1"; shift
  printf '\n=== %s ===\n' "$name"
  if "$@"; then printf '%s\n' "--- $name: OK"
  else printf '%s\n' "--- $name: FAILED"; FAILED+=("$name"); fi
}

# A gate whose tool is absent has not passed: it is unverified, which is a
# failure. Say so clearly rather than leaving "no such command" to be decoded.
require() {
  command -v "$1" >/dev/null 2>&1 || cargo "${1#cargo-}" --version >/dev/null 2>&1 || {
    echo "MISSING TOOL: $1 is not installed, so this gate cannot be verified." >&2
    echo "  install with: cargo install --locked $1" >&2
    return 1
  }
}

gate_fmt()    { cargo fmt --all --check; }
gate_build()  { cargo build --workspace --locked --all-targets; }
gate_test()   { cargo test --workspace --locked; }
gate_clippy() { cargo clippy --workspace --locked --all-targets -- -D warnings; }
# Offline-safe: advisories need the network and are checked separately.
gate_deny()   { require cargo-deny && cargo deny check bans licenses sources; }
# Documentation is part of the build: a broken link or an undocumented public
# item fails here, not in someone's browser six months from now.
gate_doc()    { RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items; }
# A dependency nobody uses is a supply-chain and compile-time cost for nothing.
gate_unused() { require cargo-machete && cargo machete; }
# Spelling, in prose and identifiers alike.
gate_typos()  { require typos && typos; }

GATES=("$@")
[[ ${#GATES[@]} -eq 0 ]] && GATES=(fmt build test clippy doc deny unused typos)

for g in "${GATES[@]}"; do
  if declare -F "gate_$g" >/dev/null; then run "$g" "gate_$g"
  else echo "unknown gate: $g" >&2; exit 2; fi
done

printf '\n'
if [[ ${#FAILED[@]} -eq 0 ]]; then
  echo "ALL GATES PASSED"
else
  printf 'FAILED GATES: %s\n' "${FAILED[*]}"; exit 1
fi
