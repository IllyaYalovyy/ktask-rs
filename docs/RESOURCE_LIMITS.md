# Run storage limits

Every value below is a positive integer environment override, read by the runner and guardian. Invalid or zero values use the default. Restart or resume after changing a limit. Limits apply per attempt or worker unless stated otherwise.

| Variable | Default | Meaning |
| --- | ---: | --- |
| `KTASK_ARTIFACT_FILE_BYTES` | 8 MiB | Largest archived file |
| `KTASK_ARTIFACT_ATTEMPT_BYTES` | 64 MiB | Archived bytes per attempt, including reservations |
| `KTASK_ARTIFACT_ATTEMPT_FILES` | 128 | Archived files per attempt, including empty files and reservations |
| `KTASK_ARTIFACT_SCAN_ENTRIES` | 256 | Most top-level artifact entries examined per worker; extras make evidence incomplete |
| `KTASK_OUTPUT_BYTES` | 64 MiB | Persisted stdout, stderr and system text per attempt |
| `KTASK_OUTPUT_ROWS` | 100,000 | Persisted output rows per attempt, including empty rows |
| `KTASK_SCRATCH_BYTES` | 4 GiB | Logical file bytes or allocated blocks, whichever is larger, including directory blocks, in one managed worker scratch directory |
| `KTASK_SCRATCH_ENTRIES` | 200,000 | Files and directories in one managed worker scratch directory |
| `KTASK_RUNNER_LOG_BYTES` | 8 MiB | One detached runner log segment; one previous segment is retained |

Output and artifact admission reject a write before it enters state storage. Output bytes and rows have transactional per-attempt counters; an upgraded attempt initializes its counters from retained output once, including empty rows and UTF-8 bytes. Already saved output, files, checks and history remain available. An exceeded limit pauses the attempt as a resource condition; raising the limit or freeing capacity permits resume without a coding repair round. A check transcript that cannot be archived is incomplete evidence and cannot pass verification.

The guardian samples managed scratch every 250 ms throughout the producer's lifetime, including after stdout and stderr close, and checks again after the producer exits. Output delivery uses a bounded in-memory queue; if the supervisor cannot accept a message for two seconds, the guardian stops the producer and reports incomplete evidence. Delivery waits do not suspend scratch monitoring. If the final message cannot be delivered, dedicated guardian exit statuses preserve scratch, artifact, or delivery resource classification; the attempt pauses with an explicit incomplete-evidence reason. Resume requires fresh verification. The guardian stops and reaps the process group before archiving and cleanup. Scratch overshoot is the bytes and entries a producer can create during one 250 ms sampling interval, the directory scan, and process-group termination; the amount depends on the producer's write rate. This is a monitoring limit, not a filesystem quota. The scan short-circuits when either limit is exceeded. A detached logger process rotates stderr at a write boundary, so a segment can exceed its limit by one 4096-byte read plus the rotation marker. Existing segments are compacted to a lower configured limit at logger startup, retaining their most recent bytes and a marker. No excess output is sent to a secondary spool.
