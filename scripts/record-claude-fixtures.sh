#!/bin/sh
# Refresh the sanitized Claude Code stream-json recordings with low-cost real calls.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
fixtures="$root/test-fixtures/claude"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/ktask-claude-recordings.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

command -v claude >/dev/null || {
    echo "claude is required to record fixtures" >&2
    exit 1
}
command -v jq >/dev/null || {
    echo "jq is required to sanitize fixtures" >&2
    exit 1
}

claude --print --output-format stream-json --verbose \
    --permission-mode bypassPermissions --model claude-haiku-4-5 \
    'Reply with exactly: KTASK_RECORDING_SUCCESS' >"$temporary/success.jsonl"

session=$(jq -er 'select(.type == "system") | .session_id' "$temporary/success.jsonl" | head -n 1)
claude --print --output-format stream-json --verbose \
    --permission-mode bypassPermissions --model claude-haiku-4-5 --resume "$session" \
    'Reply with exactly: KTASK_RECORDING_RESUMED' >"$temporary/resumed.jsonl"

mkdir "$temporary/empty-home"
if env -i HOME="$temporary/empty-home" PATH="$PATH" LANG=C LC_ALL=C \
    claude --print --bare --output-format stream-json --verbose --model claude-haiku-4-5 \
    'Reply with exactly: KTASK_RECORDING_AUTH' >"$temporary/authentication-failure.jsonl"; then
    echo "the clean-home Claude invocation unexpectedly authenticated" >&2
    exit 1
fi

sanitize() {
    jq -c '
        if .session_id then .session_id = "recorded-session" else . end
        | del(
            .uuid, .timestamp, .request_id, .cwd, .tools, .mcp_servers,
            .slash_commands, .terminal_slash_commands, .agents, .skills, .plugins,
            .capabilities, .memory_paths, .messaging_socket_path, .message.id,
            .message.content[]?.signature
        )
    ' "$1"
}

sanitize "$temporary/success.jsonl" | jq -c 'select(.type != "rate_limit_event")' >"$fixtures/success.jsonl"
sanitize "$temporary/resumed.jsonl" | jq -c 'select(.type != "rate_limit_event")' >"$fixtures/resumed.jsonl"
sanitize "$temporary/authentication-failure.jsonl" \
    | sed 's/recorded-session/recorded-auth-session/g' >"$fixtures/authentication-failure.jsonl"

# Do not manufacture quota events. This succeeds only when Claude Code itself emitted the
# warning/rejection event; it is the real warning that causes ktask-rs to pause at >=90% usage.
jq -ce '
    select(.type == "rate_limit_event")
    | select(.rate_limit_info.status != "allowed" or .rate_limit_info.utilization >= 0.9)
    | .session_id = "recorded-session"
    | del(.uuid)
' "$temporary/success.jsonl" >"$fixtures/usage-limit.jsonl" || {
    echo "Claude Code did not emit a real non-allowed or >=90% rate-limit event; fixtures unchanged" >&2
    exit 1
}
