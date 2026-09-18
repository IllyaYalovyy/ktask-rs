# Prompt template — {{TASK}} is replaced with the task body

## Your task

{{TASK}}

## Before you start

Confirm the project is green: `./scripts/quality.sh`. If it is already red,
stop and report that — it is a finding, not something to work around.

## Finishing

1. Every gate in `./scripts/quality.sh` passes, including coverage.
2. Your work is committed in small, self-contained commits, each of them green,
   with messages saying what changed and why.
3. The branch is pushed, and the local tip matches the fetched remote tip.
4. Your report is written to the path named in the orchestrator context, and
   **its first line is the result line** — the literal first line, nothing
   above it, no `#`, `>`, `*` or backtick in front of it, nothing else on it:

       KTASK_RESULT: DONE

   Rejected, because a heading marker is not a result line:

       ## KTASK_RESULT: DONE

   The permitted values are `DONE`, `FAILED` and `NEEDS_INPUT`. A rejected
   report fails the task even when the work is complete and pushed.

If the task cannot be completed, stop and report `FAILED` with the evidence.
That is an acceptable outcome. Weakening a check, reporting an unsupported
success, or thrashing until the budget runs out are not.

State what you verified and how, not merely what you changed. If you left
something deliberately undone, say so. Use `NEEDS_INPUT` only for genuine
ambiguity a human must resolve — never for work that turned out to be hard.
