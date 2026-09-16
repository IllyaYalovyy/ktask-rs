#!/usr/bin/env bash
# The single entry point for every mechanical check. CI-ready by construction:
# no hosting platform is involved, and nothing here depends on one.
#
#   scripts/quality.sh            all gates
#   scripts/quality.sh fmt test   only the named gates
set -uo pipefail
cd "$(dirname "$0")/.."

FAILED=()
run() {
  local name="$1"; shift
  printf '\n=== %s ===\n' "$name"
  if "$@"; then printf '%s\n' "--- $name: OK"
  else printf '%s\n' "--- $name: FAILED"; FAILED+=("$name"); fi
}

gate_fmt()    { cargo fmt --all --check; }
gate_build()  { cargo build --workspace --locked --all-targets; }
gate_test()   { cargo test --workspace --locked; }
gate_clippy() { cargo clippy --workspace --locked --all-targets -- -D warnings; }
# Offline-safe: advisories need the network and are checked separately.
gate_deny()   { cargo deny check bans licenses sources; }

GATES=("$@")
[[ ${#GATES[@]} -eq 0 ]] && GATES=(fmt build test clippy deny)

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
