#!/usr/bin/env bash
# The single entry point for every mechanical check. CI-ready by construction:
# no hosting platform is involved, and nothing here depends on one.
#
#   scripts/quality.sh            all gates
#   scripts/quality.sh fmt test   only the named gates
#
# Gates: fmt build test clippy doc deny unused typos coverage
# scripts/check-prereqs.sh reports anything missing.
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
    echo "  run ./scripts/check-prereqs.sh for the full list and how to install it" >&2
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
# Coverage is a floor, not a target. It proves code is exercised; it says
# nothing about whether the tests would notice if that code were wrong.
# scripts/review-tests.sh answers that question.
gate_coverage() {
  require cargo-llvm-cov || return 1
  # Without rustup there is no llvm-tools-preview component, so point
  # cargo-llvm-cov at the distro's LLVM tools when they are on PATH.
  if [[ -z "${LLVM_COV:-}" ]] && command -v llvm-cov >/dev/null 2>&1; then
    export LLVM_COV="$(command -v llvm-cov)"
  fi
  if [[ -z "${LLVM_PROFDATA:-}" ]] && command -v llvm-profdata >/dev/null 2>&1; then
    export LLVM_PROFDATA="$(command -v llvm-profdata)"
  fi
  cargo llvm-cov --workspace --summary-only \
    --ignore-filename-regex 'main\.rs$' \
    --fail-under-lines "${KTASK_MIN_COVERAGE:-80}"
}

GATES=("$@")
[[ ${#GATES[@]} -eq 0 ]] && GATES=(fmt build test clippy doc deny unused typos coverage)

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
