# Scenario Format

End-to-end acceptance scenarios driven by the `dummy` provider. These are the
scenarios you can see; a hidden subset is used for scoring, so make the
implementation correct rather than fitted to these cases.

## Scenario TOML Format

A scenario is a TOML file containing a list of deterministic steps. Each step
defines a trigger condition and an outcome for the dummy provider to simulate.

### Step Structure

Each step in the `steps` array has:

- **Trigger** (exactly one required):
  - `on_task` (number): Trigger on a specific task ID (1-based)
  - `on_attempt` (number): Trigger on a specific attempt number (1-based)

- **Outcome** (required):
  - `"success"`: Successful completion (exit code 0)
  - `"failure"`: Task execution failed (exit code 1)
  - `"hang"`: The provider hangs indefinitely
  - `"limit"`: A provider rate or usage limit was reached
  - `"needs_input"`: The provider requires additional input (waiting_input state)

- **Optional Fields**:
  - `stdout` (string): Standard output to produce
  - `exit_code` (number): Specific exit code (overrides default for outcome)
  - `delay_ms` (number): Milliseconds to delay before returning outcome
  - `files` (table): Files to write to the working directory
    - Keys are file paths (relative to working directory)
    - Values are file contents

### Example Scenario

```toml
[[steps]]
on_task = 1
outcome = "success"
stdout = "Task 1 completed successfully"
exit_code = 0
delay_ms = 100

[[steps]]
on_attempt = 1
outcome = "failure"
stdout = "Build failed"
exit_code = 1
delay_ms = 50

[steps.files]
"error.log" = "Compilation error on line 42"
"debug.txt" = "Debug information"

[[steps]]
on_task = 2
outcome = "limit"

[[steps]]
on_task = 3
outcome = "needs_input"
stdout = "Waiting for user input"

[[steps]]
on_attempt = 2
outcome = "hang"
```

## Running Scenarios

The `dummy` provider uses scenarios for deterministic, repeatable testing:

```bash
cargo nextest run -p ktask-core -E 'test(/dummy_scenario_round_trips/)'
```

A scenario round-trips correctly through TOML serialization when all fields
preserve their values. Unknown outcome types will fail to deserialize with
a clear error message.
