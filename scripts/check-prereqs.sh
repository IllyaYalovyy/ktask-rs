#!/usr/bin/env bash
# Check everything a task needs in order to run. Installs nothing: it reports
# what is missing and the command that installs it, and exits non-zero if any
# required item is absent.
#
#   scripts/check-prereqs.sh
set -uo pipefail
cd "$(dirname "$0")/.."

MSRV_MAJOR=1; MSRV_MINOR=97
MISSING_DNF=(); MISSING_CARGO=(); NOTES=(); FAIL=0

ok()   { printf '  \033[32mok\033[0m       %-16s %s\n' "$1" "${2:-}"; }
miss() { printf '  \033[31mMISSING\033[0m  %-16s %s\n' "$1" "${2:-}"; FAIL=1; }
warn() { printf '  \033[33mwarn\033[0m     %-16s %s\n' "$1" "${2:-}"; }

ver() { "$@" 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1; }

echo "System packages"
# --- rust ------------------------------------------------------------------
RUSTC="$(ver rustc --version)"
if [[ -z "$RUSTC" ]]; then
  miss rustc "not found"; MISSING_DNF+=(rust cargo)
else
  MJ="${RUSTC%%.*}"; R="${RUSTC#*.}"; MN="${R%%.*}"
  if (( MJ > MSRV_MAJOR || (MJ == MSRV_MAJOR && MN >= MSRV_MINOR) )); then
    ok rustc "$RUSTC  (need >= ${MSRV_MAJOR}.${MSRV_MINOR})"
  else
    miss rustc "$RUSTC is older than ${MSRV_MAJOR}.${MSRV_MINOR}"; MISSING_DNF+=(rust)
  fi
fi
command -v cargo >/dev/null && ok cargo "$(ver cargo --version)" || { miss cargo "not found"; MISSING_DNF+=(cargo); }

# Two rust installations on one PATH is the most common way this goes wrong.
if command -v rustup >/dev/null 2>&1 && [[ -x /usr/bin/rustc ]]; then
  warn rust "both a distro rustc (/usr/bin) and rustup are installed"
  NOTES+=("Two Rust installations are on this machine. Whichever comes first on PATH wins, which makes gate results depend on shell setup. Keep one.")
fi

cargo clippy --version >/dev/null 2>&1 && ok clippy "$(ver cargo clippy --version)" \
  || { miss clippy "needed by the clippy gate"; MISSING_DNF+=(clippy); }
cargo fmt --version >/dev/null 2>&1 && ok rustfmt "$(ver cargo fmt --version)" \
  || { miss rustfmt "needed by the fmt gate"; MISSING_DNF+=(rustfmt); }

# --- build tools -----------------------------------------------------------
command -v cc >/dev/null && ok cc "$(cc --version 2>/dev/null | head -1)" \
  || { miss cc "rusqlite compiles SQLite from C"; MISSING_DNF+=(gcc); }
command -v git >/dev/null && ok git "$(ver git --version)" \
  || { miss git "the supervisor drives git directly"; MISSING_DNF+=(git); }

# --- llvm-profdata: the coverage gate cannot run without it ----------------
PROFDATA=""
command -v llvm-profdata >/dev/null 2>&1 && PROFDATA="$(command -v llvm-profdata)"
if [[ -z "$PROFDATA" ]] && command -v rustc >/dev/null; then
  SYSROOT="$(rustc --print sysroot 2>/dev/null)"
  [[ -n "$SYSROOT" ]] && PROFDATA="$(find "$SYSROOT" -name llvm-profdata -type f 2>/dev/null | head -1)"
fi
if [[ -n "$PROFDATA" ]]; then ok llvm-profdata "$PROFDATA"
else
  miss llvm-profdata "the coverage gate cannot run without it"
  MISSING_DNF+=(llvm)
  NOTES+=("llvm-profdata comes either from rustup's llvm-tools component or from the distro llvm package. Without it scripts/quality.sh fails at the coverage gate.")
fi

# --- cargo-installed tools -------------------------------------------------
echo
echo "Cargo tools"
check_cargo() { # display-name  crate  version-command...
  local name="$1" crate="$2"; shift 2
  if "$@" >/dev/null 2>&1; then ok "$name" "$(ver "$@")"
  else miss "$name" "needed by scripts/quality.sh"; MISSING_CARGO+=("$crate"); fi
}
check_cargo cargo-deny    cargo-deny    cargo deny --version
check_cargo cargo-machete cargo-machete cargo machete --version
check_cargo typos         typos-cli     typos --version
check_cargo cargo-nextest cargo-nextest cargo nextest --version
check_cargo cargo-mutants cargo-mutants cargo mutants --version
check_cargo cargo-llvm-cov cargo-llvm-cov cargo llvm-cov --version

# --- environment -----------------------------------------------------------
echo
echo "Environment"
case ":$PATH:" in
  *":$HOME/.cargo/bin:"*) ok PATH "\$HOME/.cargo/bin is on PATH" ;;
  *) warn PATH "\$HOME/.cargo/bin is not on PATH"
     NOTES+=("cargo install puts binaries in \$HOME/.cargo/bin. Add it to PATH in your shell profile or the gates will not find them.") ;;
esac
if timeout 8 bash -c '</dev/tcp/static.crates.io/443' 2>/dev/null; then
  ok network "crates.io reachable"
else
  warn network "crates.io not reachable"
  NOTES+=("The first build downloads dependencies. Without network access it will fail; after that the cache is enough.")
fi
[[ -f rust-toolchain.toml ]] && {
  warn toolchain "rust-toolchain.toml is present"
  NOTES+=("rust-toolchain.toml pins an exact toolchain and will make rustup download it. This project does not ship one; delete it, or re-clone.")
}

# --- report ----------------------------------------------------------------
echo
if (( ${#MISSING_DNF[@]} )); then
  echo "To install the missing system packages:"
  printf '    sudo dnf install %s\n' "$(printf '%s\n' "${MISSING_DNF[@]}" | sort -u | tr '\n' ' ')"
  echo
fi
if (( ${#MISSING_CARGO[@]} )); then
  echo "To install the missing cargo tools:"
  printf '    cargo install --locked %s\n' "$(printf '%s\n' "${MISSING_CARGO[@]}" | sort -u | tr '\n' ' ')"
  echo
fi
if (( ${#NOTES[@]} )); then
  echo "Notes:"
  for n in "${NOTES[@]}"; do printf '  - %s\n' "$n"; done
  echo
fi
if (( FAIL )); then
  echo "NOT READY — install the items above, then run this again."
else
  echo "READY — every prerequisite is present. Verify the project with: ./scripts/quality.sh"
fi
exit $FAIL
