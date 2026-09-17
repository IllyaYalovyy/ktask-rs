#!/usr/bin/env bash
# Test review: do the tests written for THIS change actually constrain it?
#
#   scripts/review-tests.sh [base-ref]
#
# Coverage proves a line ran. This proves the tests would fail if that line
# were wrong. It mutates only the lines this change touched, so it is fast
# enough to run per task rather than per release.
#
# A surviving mutant means: something was changed to be wrong, every test
# still passed, and nobody noticed. Kill it with a real assertion, or record
# in the task report why it is unreachable. Deleting the mutated code is also
# a valid answer.
set -euo pipefail
cd "$(dirname "$0")/.."

BASE="${1:-}"
[[ $# -gt 0 ]] && shift          # remaining args pass through to cargo mutants
if [[ -z "$BASE" ]]; then
  BASE="$(git rev-parse --verify --quiet '@{upstream}' 2>/dev/null || true)"
  [[ -z "$BASE" ]] && BASE="$(git rev-parse --verify --quiet HEAD~1 || true)"
fi
[[ -n "$BASE" ]] || { echo "no base commit to compare against" >&2; exit 2; }

command -v cargo-mutants >/dev/null 2>&1 || cargo mutants --version >/dev/null 2>&1 || {
  echo "MISSING TOOL: cargo-mutants. run scripts/check-prereqs.sh" >&2; exit 1; }

DIFF="$(mktemp)"; trap 'rm -f "$DIFF"' EXIT
git diff "$BASE" -- '*.rs' > "$DIFF"
if [[ ! -s "$DIFF" ]]; then
  echo "No Rust changes since $BASE — nothing to review."
  exit 0
fi

echo "Reviewing test strength for changes since $BASE"
rm -rf target/mutants
set +e
cargo mutants --in-diff "$DIFF" --timeout "${KTASK_MUTANT_TIMEOUT:-120}" \
  --output target/mutants "$@"
RC=$?
set -e
# 0 = all caught, 2 = some survived. Anything else is a tool or build failure
# and must not be reported as a pass.
if [[ $RC -ne 0 && $RC -ne 2 ]]; then
  echo "cargo mutants failed (exit $RC) — test review did not run" >&2
  exit 1
fi

OUT=target/mutants/mutants.out
count() { [[ -s "$1" ]] && grep -c . "$1" || echo 0; }
CAUGHT=$(count "$OUT/caught.txt")
MISSED=$(count "$OUT/missed.txt")
UNVIABLE=$(count "$OUT/unviable.txt")
echo
echo "caught $CAUGHT | survived $MISSED | unviable $UNVIABLE"

if [[ "$MISSED" -gt "${KTASK_MAX_SURVIVORS:-0}" ]]; then
  echo
  echo "SURVIVING MUTANTS — these changes are not covered by an assertion:"
  cat "$OUT/missed.txt"
  echo
  echo "Add a test that fails when the code is wrong, or state in your report"
  echo "why each survivor is unreachable. Do not weaken a test to pass this."
  exit 1
fi
echo "All mutants in the changed lines were caught."
