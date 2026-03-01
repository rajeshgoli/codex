# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## After-tool-use hook

Codex can run an external hook after each tool call completes. Configure:

```toml
after_tool_use = ["python3", "/path/to/hook.py"]
after_tool_use_failure_behavior = "continue" # or "abort"
```

Codex appends one JSON argument containing the hook payload. The payload uses the same stable `HookPayload` envelope shape as other hooks, with:

1. `hook_event.event_type = "after_tool_use"`
2. `hook_event.turn_id`
3. `hook_event.call_id`
4. `hook_event.tool_name`
5. `hook_event.tool_kind`
6. `hook_event.tool_input`
7. `hook_event.executed`
8. `hook_event.success`
9. `hook_event.duration_ms`
10. `hook_event.mutating`
11. `hook_event.sandbox`
12. `hook_event.sandbox_policy`
13. `hook_event.output_preview`

Failure behavior is deterministic:

1. `continue` (default): hook failure is recorded and operation continues.
2. `abort`: hook failure aborts the operation immediately.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key) or the
`CODEX_SQLITE_HOME` environment variable. When unset, WorkspaceWrite sandbox
sessions default to a temp directory; other modes default to `CODEX_HOME`.

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
