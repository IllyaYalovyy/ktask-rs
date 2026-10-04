# Claude stream-json fixtures

`test-fixtures/claude/` contains sanitized recordings from Claude Code 2.1.283:
success, a usage warning, an authentication failure, and a resumed session. Tests replay them
through the real `ktask-rs` binary; they never construct Claude stream JSON inline.

It also contains two verbatim, secret-free Claude Code 2.1.283 success recordings for the
subagent-tool contract: `claude-2.1.283-success-nodeny.jsonl` and
`claude-2.1.283-success-denylist.jsonl`. Unlike the other fixtures, their `system/init` tool
lists are intentionally retained: the former contains `Task`; the latter was recorded with
the `Agent` deny alias and does not. They document why `Agent` must not be in ktask-rs's
built-in Claude deny list.

Refresh the sanitized fixtures with `scripts/record-claude-fixtures.sh`. It makes one Haiku
call for the success recording, one resumed Haiku call, and one clean-home call that records
the real login failure. It removes session IDs, request IDs, timestamps, local paths, tool
lists, and thinking signatures before writing those committed files. The supplied tool-catalogue
recordings stay verbatim so their `system/init.tools` evidence remains available to tests.

The usage-limit fixture is never invented. The recorder extracts it only if Claude Code emits a
non-`allowed` event or reports utilization of at least 90%; otherwise it fails and leaves the
committed fixtures untouched. `ktask-rs` likewise pauses at 90% usage rather than waiting for a
hard rejection. The retained event uses `rate_limit_info.status` and `resetsAt` exactly as
emitted by Claude Code.
