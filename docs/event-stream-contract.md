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
- `--control-socket <absolute-path>`
  - Binds a local Unix domain socket for programmatic control.
  - Existing socket files at that path are removed only when they are actual sockets.
  - Socket file permissions are set to `0600`.

## Control Socket Contract

The control socket accepts newline-delimited JSON request objects and returns one newline-delimited JSON response per request.

### Request Envelope

All requests include:

- `request_id` (string, required): caller-supplied correlation/idempotency key.
- `expected_epoch` (string, optional): if provided and mismatched, request is rejected with `stale_epoch`.
- `command` (string, required): one of:
  - `get_epoch`
  - `submit_message`
  - `submit_approval`
  - `submit_user_input`
  - `shutdown`

### Command Payloads

- `submit_message`
  - `message` (string, required)
  - `thread_id` (UUID string, optional)
- `submit_approval`
  - `id` (string, required)
  - `decision` (string, required): `approved | approved_for_session | denied | abort`
  - `approval_kind` (string, optional): `exec` (default) or `patch`
  - `thread_id` (UUID string, optional)
  - `turn_id` (string, optional, exec approvals only)
- `submit_user_input`
  - `id` (string, required)
  - `response` (object, required): `RequestUserInputResponse` shape
  - `thread_id` (UUID string, optional)
- `shutdown`
  - `immediate` (bool, optional, default `false`)

### Response Envelope

All responses include:

- `request_id` (string)
- `ok` (bool)
- `epoch` (string)
- `result` (object, present when `ok=true`)
- `error` (object, present when `ok=false`)
  - `code` (string)
  - `message` (string)

### Idempotency and Epoch Semantics

- Duplicate `request_id` values return the cached original response.
- `expected_epoch` can be used to detect reconnect/restart races.
- Epoch values are process-scoped and change when a new interactive process starts.

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
