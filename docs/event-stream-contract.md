# Interactive Event Stream Contract

Codex interactive mode supports an explicit JSONL event stream contract intended for machine consumers.

## CLI Surface

- `--event-stream <path-or-stdout>`
  - Writes newline-delimited JSON records to a file path.
  - Use `-` to emit records to stdio:
    - non-TTY stdout: stream writes to stdout
    - interactive TTY stdout: stream writes to stderr to avoid mixing JSONL with TUI control output
- `--event-schema-version <int>`
  - Pins schema parsing/serialization behavior to a supported version.
  - Currently supported: `1`, `2`.
  - Current default if omitted: `2`.

## Record Shape

All schema versions include the following required top-level fields:

- `schema_version` (number)
- `ts` (RFC3339 timestamp string, UTC)
- `session_id` (string)
- `event_type` (string)
- `payload` (object)

`event_type` and `payload` are derived from protocol `EventMsg` values. Payload values keep parity with protocol event field names and types.

## Version Details

### Schema `1`

```json
{
  "schema_version": 1,
  "ts": "2026-03-01T00:00:00.000Z",
  "session_id": "01957117-d96d-7f17-af57-af28ec451245",
  "event_type": "turn_started",
  "payload": {
    "turn_id": "turn_001",
    "model_context_window": 128000,
    "collaboration_mode_kind": "none"
  }
}
```

### Schema `2` (current)

Schema `2` adds additive ordering metadata:

- `seq` (monotonic per-process stream sequence)
- `session_epoch` (stream epoch, currently `1`)

```json
{
  "schema_version": 2,
  "ts": "2026-03-01T00:00:00.000Z",
  "session_id": "01957117-d96d-7f17-af57-af28ec451245",
  "seq": 42,
  "session_epoch": 1,
  "event_type": "turn_started",
  "payload": {
    "turn_id": "turn_001",
    "model_context_window": 128000,
    "collaboration_mode_kind": "none"
  }
}
```

## Compatibility Policy

- Additive-only field additions are allowed within a schema version.
- Field removals, renames, or type changes require a schema version bump.
- Consumers should pin `--event-schema-version` to guarantee deterministic parsing.
- Compatibility tests cover at least previous and current schema fixtures.
