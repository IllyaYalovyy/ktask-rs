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
# Use the Rust that is already on the machine. Every contestant runs on the
# same box, so that toolchain is the constant the comparison needs — there is
# nothing to gain by installing a second one, and a distro Rust plus a rustup
# Rust on one PATH is a trap.
MSRV_MAJOR=1
MSRV_MINOR=97
rust_ver() { rustc --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1; }
RUSTC_VERSION="$(rust_ver)"

if [[ -z "$RUSTC_VERSION" ]]; then
  echo "  rustc not found." >&2
  echo "  Install it with your package manager, for example:" >&2
  echo "      sudo dnf install rust cargo clippy rustfmt" >&2
  echo "  or install rustup from https://rustup.rs if you prefer." >&2
  exit 1
fi

MAJOR="${RUSTC_VERSION%%.*}"; REST="${RUSTC_VERSION#*.}"; MINOR="${REST%%.*}"
if (( MAJOR < MSRV_MAJOR || (MAJOR == MSRV_MAJOR && MINOR < MSRV_MINOR) )); then
  echo "  rustc $RUSTC_VERSION is older than the minimum ${MSRV_MAJOR}.${MSRV_MINOR}" >&2
  exit 1
fi
printf '  %-16s %s\n' rustc "$RUSTC_VERSION (>= ${MSRV_MAJOR}.${MSRV_MINOR})"

missing=()
cargo clippy --version >/dev/null 2>&1 || missing+=(clippy)
cargo fmt --version    >/dev/null 2>&1 || missing+=(rustfmt)
cargo llvm-cov --version >/dev/null 2>&1 || true   # installed below
if (( ${#missing[@]} )); then
  if command -v rustup >/dev/null 2>&1 && rustup show active-toolchain >/dev/null 2>&1; then
    echo "  adding components: ${missing[*]}"
    [[ $CHECK_ONLY -eq 1 ]] && { echo "  missing: ${missing[*]}" >&2; exit 1; }
    rustup component add "${missing[@]}" llvm-tools-preview
  else
    echo "  missing: ${missing[*]}" >&2
    echo "      sudo dnf install ${missing[*]}" >&2
    exit 1
  fi
else
  printf '  %-16s %s\n' clippy  "$(cargo clippy --version 2>/dev/null)"
  printf '  %-16s %s\n' rustfmt "$(cargo fmt --version 2>/dev/null)"
fi

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
