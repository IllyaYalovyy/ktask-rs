#!/usr/bin/env bash
# Install everything scripts/quality.sh needs, at the versions this project
# pins. Idempotent: safe to re-run, and re-run it after a version bump.
#
#   scripts/setup.sh           install and verify
#   scripts/setup.sh --check   verify only, install nothing (exit 1 on drift)
set -euo pipefail
cd "$(dirname "$0")/.."

# Pinned so every machine — and every contestant — runs the identical analysis.
# A newer clippy or cargo-deny reports different findings, which silently makes
# results incomparable.
CARGO_DENY_VERSION=0.20.2
CARGO_MACHETE_VERSION=0.9.2
TYPOS_VERSION=1.50.2
CARGO_NEXTEST_VERSION=0.9.109
CARGO_MUTANTS_VERSION=25.3.1
CARGO_LLVM_COV_VERSION=0.9.0

CHECK_ONLY=0
[[ "${1:-}" == "--check" ]] && CHECK_ONLY=1

have() { command -v "$1" >/dev/null 2>&1; }
ver()  { "$@" 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1; }

ensure() { # binary  wanted-version  crate-name  version-command...
  local bin="$1" want="$2" crate="$3"; shift 3
  local got; got="$(ver "$@")"
  if [[ "$got" == "$want" ]]; then
    printf '  %-16s %s\n' "$bin" "$got"
    return 0
  fi
  if [[ $CHECK_ONLY -eq 1 ]]; then
    printf '  %-16s MISMATCH want %s, got %s\n' "$bin" "$want" "${got:-<missing>}"
    return 1
  fi
  printf '  %-16s installing %s (had %s)\n' "$bin" "$want" "${got:-nothing}"
  cargo install --locked "$crate" --version "$want"
}

echo "== rust toolchain =="
if ! have rustup; then
  if [[ $CHECK_ONLY -eq 1 ]]; then echo "  rustup missing" >&2; exit 1; fi
  echo "  installing rustup"
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
# rust-toolchain.toml pins the version and components; this materializes them.
rustup show active-toolchain
rustup component add clippy rustfmt llvm-tools-preview >/dev/null 2>&1 || true
printf '  %-16s %s\n' rustc  "$(ver rustc --version)"
printf '  %-16s %s\n' clippy "$(ver cargo clippy --version)"
printf '  %-16s %s\n' rustfmt "$(ver cargo fmt --version)"

echo "== analysis tools =="
RC=0
ensure cargo-deny    "$CARGO_DENY_VERSION"    "cargo-deny"    cargo deny --version    || RC=1
ensure cargo-machete "$CARGO_MACHETE_VERSION" "cargo-machete" cargo machete --version || RC=1
ensure typos         "$TYPOS_VERSION"         "typos-cli"     typos --version         || RC=1

echo "== test and scoring tools =="
ensure cargo-nextest "$CARGO_NEXTEST_VERSION" "cargo-nextest" cargo nextest --version || RC=1
ensure cargo-mutants "$CARGO_MUTANTS_VERSION" "cargo-mutants" cargo mutants --version || RC=1
ensure cargo-llvm-cov "$CARGO_LLVM_COV_VERSION" "cargo-llvm-cov" cargo llvm-cov --version || RC=1

echo
if [[ $RC -ne 0 ]]; then
  echo "Tool versions do not match. Run scripts/setup.sh (without --check) to fix." >&2
  exit 1
fi
if [[ $CHECK_ONLY -eq 1 ]]; then
  echo "All tools present at pinned versions."
else
  echo "Setup complete. Verify the project with: ./scripts/quality.sh"
fi
