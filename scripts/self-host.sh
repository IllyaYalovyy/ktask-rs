#!/usr/bin/env bash
# Configure an installed `ktask` to supervise its own source checkout.
#
# Registers the project, sets the `make check` project check, and sets model
# defaults for new tasks. Existing tasks keep their pinned models. Preferences
# (mouse, run mode, planner approval) are TUI-only: Configuration -> Preferences.
# Rerunning is safe; it only updates the check and defaults to the values below.
#
# Usage: scripts/self-host.sh [--ktask PATH] [--project DIR] [--instance NAME]
#   --ktask PATH     Supervisor executable (default: ktask on PATH; the production
#                    instance must not live under this checkout's target/).
#   --project DIR    Git repository root to register (default: this checkout).
#   --instance NAME  Expected instance reported by --paths (default: ktask).
# Environment overrides:
#   KTASK_PROVIDER             (codex)
#   KTASK_EXECUTION_MODEL      KTASK_EXECUTION_EFFORT      (gpt-6-sol, high)
#   KTASK_PLANNING_MODEL       KTASK_PLANNING_EFFORT       (gpt-6-sol, high)
#   KTASK_REVIEW_MODEL         KTASK_REVIEW_EFFORT         (gpt-6-astra, high)
#   KTASK_VALIDATION_MODEL     KTASK_VALIDATION_EFFORT     (gpt-6-astra, high)
#   KTASK_REMEDIATION_MODEL    KTASK_REMEDIATION_EFFORT    (gpt-6-astra, high)
#   KTASK_CHECK_COMMAND        KTASK_CHECK_TIMEOUT_MS      (make check, 600000)
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
ktask=""
project=$repo
instance=ktask
while [ $# -gt 0 ]; do
    case "$1" in
        --ktask | --project | --instance)
            [ $# -ge 2 ] || { echo "self-host: $1 needs a value" >&2; exit 2; }
            case "$1" in
                --ktask) ktask=$2 ;;
                --project) project=$2 ;;
                --instance) instance=$2 ;;
            esac
            shift 2
            ;;
        -h | --help)
            sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "self-host: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

fail() {
    echo "self-host: $*" >&2
    exit 1
}

provider=${KTASK_PROVIDER:-codex}
check_command=${KTASK_CHECK_COMMAND:-make check}
check_timeout=${KTASK_CHECK_TIMEOUT_MS:-600000}

if [ -z "$ktask" ]; then
    ktask=$(command -v ktask 2> /dev/null) || fail "ktask is not on PATH; run scripts/install-user.sh first or pass --ktask PATH"
fi
[ -x "$ktask" ] || fail "$ktask is not executable"
ktask=$(readlink -f -- "$ktask")
if [ "$instance" = ktask ]; then
    case "$ktask" in
        "$repo"/target/*) fail "$ktask is a build output; install the supervisor with scripts/install-user.sh" ;;
    esac
fi
paths=$("$ktask" --paths) || fail "$ktask failed to report its paths"
reported=$(printf '%s\n' "$paths" | awk -F '\t' '$1 == "instance" { print $2 }')
[ "$reported" = "$instance" ] || fail "$ktask is instance '$reported', expected '$instance' (pass --instance to override)"

[ -d "$project" ] || fail "project directory $project does not exist"
project=$(cd -- "$project" && pwd -P)
top=$(git -C "$project" rev-parse --show-toplevel 2> /dev/null) || fail "$project is not inside a Git repository"
[ "$(readlink -f -- "$top")" = "$project" ] || fail "$project must be the Git repository root ($top)"
git -C "$project" rev-parse --verify HEAD > /dev/null 2>&1 \
    || fail "$project needs an initial commit; execution requires a Git baseline"
if [ -n "$(git -C "$project" status --porcelain)" ]; then
    echo "Note: $project has uncommitted changes; new tasks need a clean baseline before they can start." >&2
fi
case "$check_command" in
    make*) command -v make > /dev/null 2>&1 || fail "make is required by the '$check_command' check" ;;
esac

run() {
    "$ktask" --project "$project" "$@"
}

echo "Supervisor: $ktask ($reported)"
echo "Project:    $project"
run check-set build --command "$check_command" --timeout-ms "$check_timeout"
set_default() {
    local role=$1 model=$2 effort=$3
    if [ -n "$effort" ]; then
        run model-default "$role" --provider "$provider" --model "$model" --effort "$effort"
    else
        run model-default "$role" --provider "$provider" --model "$model"
    fi
}
set_default execution "${KTASK_EXECUTION_MODEL:-gpt-6-sol}" "${KTASK_EXECUTION_EFFORT-high}"
set_default planning "${KTASK_PLANNING_MODEL:-gpt-6-sol}" "${KTASK_PLANNING_EFFORT-high}"
set_default review "${KTASK_REVIEW_MODEL:-gpt-6-astra}" "${KTASK_REVIEW_EFFORT-high}"
set_default validation "${KTASK_VALIDATION_MODEL:-gpt-6-astra}" "${KTASK_VALIDATION_EFFORT-high}"
set_default remediation "${KTASK_REMEDIATION_MODEL:-gpt-6-astra}" "${KTASK_REMEDIATION_EFFORT-high}"

echo
echo "Checks:"
run checks
echo
echo "Current task:"
run current
echo
echo "Next: cd $project && $ktask   (Configuration -> Preferences for mouse/run mode;"
echo "      Current -> Run, or: $ktask --project $project run --background)"
