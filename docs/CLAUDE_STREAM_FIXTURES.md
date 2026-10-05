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

The usage-warning fixture is never invented. The recorder extracts it only if Claude Code emits
an `allowed_warning` event; otherwise it fails and leaves the committed fixtures untouched.
`ktask-rs` records that warning on the completed step, but waits only for a hard `rejected`
event or a result error that names the limit. The retained event uses
`rate_limit_info.status` and `resetsAt` exactly as emitted by Claude Code.

`claude-2.1.283-derived-rejected.jsonl` covers refusals that cannot be safely recorded from
the owner's exhausted account. Its first line labels its provenance: the real nodeny recording's
rate-limit event changes only `status` from `allowed_warning` to `rejected`; its real result
event changes `is_error` and `result` to Claude's documented usage-limit result. No other event
shape or fixture content is invented. Replay tests retain the fixture's event shape and move its
already-expired reset timestamp only at runtime, so the real binary can be observed waiting.
