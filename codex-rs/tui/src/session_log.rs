use std::fs::File;
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_core::config::Config;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::Op;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::Cli;
use crate::app_event::AppEvent;

const EVENT_SCHEMA_V1: u32 = 1;
const EVENT_SCHEMA_V2: u32 = 2;
const EVENT_SCHEMA_CURRENT: u32 = EVENT_SCHEMA_V2;

static LOGGER: LazyLock<SessionLogger> = LazyLock::new(SessionLogger::new);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogMode {
    Legacy,
    EventStream,
}

#[derive(Debug)]
struct EventStreamState {
    schema_version: u32,
    seq: u64,
    session_epoch: u64,
    session_id: Option<String>,
}

struct SessionLogger {
    writer: OnceLock<Mutex<Box<dyn Write + Send>>>,
    mode: OnceLock<LogMode>,
    event_stream_state: OnceLock<Mutex<EventStreamState>>,
}

impl SessionLogger {
    fn new() -> Self {
        Self {
            writer: OnceLock::new(),
            mode: OnceLock::new(),
            event_stream_state: OnceLock::new(),
        }
    }

    fn open_legacy(&self, path: PathBuf) -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        let file = opts.open(path)?;
        self.writer.get_or_init(|| Mutex::new(Box::new(file)));
        self.mode.get_or_init(|| LogMode::Legacy);
        Ok(())
    }

    fn open_event_stream(&self, target: &str, schema_version: u32) -> std::io::Result<()> {
        validate_schema_version(schema_version)?;

        if target == "-" {
            // Interactive TUI rendering owns stdout when attached to a TTY.
            // Route stream records to stderr in that mode to avoid mixing
            // JSONL with terminal control output.
            let stream: Box<dyn Write + Send> = if std::io::stdout().is_terminal() {
                Box::new(std::io::stderr())
            } else {
                Box::new(std::io::stdout())
            };
            self.writer.get_or_init(|| Mutex::new(stream));
        } else {
            let mut opts = OpenOptions::new();
            opts.create(true).truncate(true).write(true);

            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }

            let file = opts.open(PathBuf::from(target))?;
            self.writer.get_or_init(|| Mutex::new(Box::new(file)));
        }

        self.mode.get_or_init(|| LogMode::EventStream);
        self.event_stream_state.get_or_init(|| {
            Mutex::new(EventStreamState {
                schema_version,
                seq: 0,
                session_epoch: 1,
                session_id: None,
            })
        });

        Ok(())
    }

    fn mode(&self) -> Option<LogMode> {
        self.mode.get().copied()
    }

    fn write_json_line(&self, value: &Value) {
        let Some(mutex) = self.writer.get() else {
            return;
        };

        let mut guard = match mutex.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        match serde_json::to_string(value) {
            Ok(serialized) => {
                if let Err(err) = guard.write_all(serialized.as_bytes()) {
                    tracing::warn!("session log write error: {err}");
                    return;
                }
                if let Err(err) = guard.write_all(b"\n") {
                    tracing::warn!("session log write error: {err}");
                    return;
                }
                if let Err(err) = guard.flush() {
                    tracing::warn!("session log flush error: {err}");
                }
            }
            Err(err) => tracing::warn!("session log serialize error: {err}"),
        }
    }

    fn write_event_stream_record(
        &self,
        event_type: &str,
        payload: Value,
        session_id_override: Option<String>,
    ) {
        let Some(state_mutex) = self.event_stream_state.get() else {
            return;
        };

        let mut state = match state_mutex.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        if event_type == "session_configured"
            && let Some(session_id) = payload.get("session_id").and_then(Value::as_str)
        {
            state.session_id = Some(session_id.to_string());
        }

        state.seq += 1;
        let session_id = session_id_override
            .or_else(|| state.session_id.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let record = match state.schema_version {
            EVENT_SCHEMA_V1 => json!({
                "schema_version": EVENT_SCHEMA_V1,
                "ts": now_ts(),
                "session_id": session_id,
                "event_type": event_type,
                "payload": payload,
            }),
            EVENT_SCHEMA_V2 => json!({
                "schema_version": EVENT_SCHEMA_V2,
                "ts": now_ts(),
                "session_id": session_id,
                "seq": state.seq,
                "session_epoch": state.session_epoch,
                "event_type": event_type,
                "payload": payload,
            }),
            _ => return,
        };

        self.write_json_line(&record);
    }

    fn write_protocol_event(&self, event: &Event, thread_id_override: Option<&ThreadId>) {
        let mut payload = match serde_json::to_value(&event.msg) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!("event stream serialize error: {err}");
                return;
            }
        };

        let event_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let event_type = normalize_contract_event_type(&event_type);

        if let Some(obj) = payload.as_object_mut() {
            obj.remove("type");
        }

        self.write_event_stream_record(
            &event_type,
            payload,
            thread_id_override.map(ToString::to_string),
        );
    }

    fn is_enabled(&self) -> bool {
        self.writer.get().is_some()
    }
}

fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn validate_schema_version(schema_version: u32) -> std::io::Result<()> {
    if matches!(schema_version, EVENT_SCHEMA_V1 | EVENT_SCHEMA_V2) {
        return Ok(());
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "unsupported event schema version `{schema_version}`; supported versions are {EVENT_SCHEMA_V1} and {EVENT_SCHEMA_V2}"
        ),
    ))
}

fn normalize_contract_event_type(event_type: &str) -> String {
    match event_type {
        "task_started" => "turn_started".to_string(),
        "task_complete" => "turn_complete".to_string(),
        _ => event_type.to_string(),
    }
}

pub(crate) fn maybe_init(config: &Config, cli: &Cli) -> std::io::Result<()> {
    if cli.event_stream.is_some() || cli.event_schema_version.is_some() {
        let target = cli.event_stream.clone().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--event-schema-version requires --event-stream",
            )
        })?;

        let schema_version = cli.event_schema_version.unwrap_or(EVENT_SCHEMA_CURRENT);
        LOGGER.open_event_stream(&target, schema_version)?;

        LOGGER.write_event_stream_record(
            "session_start",
            json!({
                "cwd": config.cwd,
                "model": config.model,
                "model_provider_id": config.model_provider_id,
                "model_provider_name": config.model_provider.name,
            }),
            None,
        );
        return Ok(());
    }

    let enabled = std::env::var("CODEX_TUI_RECORD_SESSION")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    if !enabled {
        return Ok(());
    }

    let path = if let Ok(path) = std::env::var("CODEX_TUI_SESSION_LOG_PATH") {
        PathBuf::from(path)
    } else {
        let mut p = match codex_core::config::log_dir(config) {
            Ok(dir) => dir,
            Err(_) => std::env::temp_dir(),
        };
        let filename = format!(
            "session-{}.jsonl",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        );
        p.push(filename);
        p
    };

    if let Err(err) = LOGGER.open_legacy(path.clone()) {
        tracing::error!("failed to open session log {:?}: {}", path, err);
        return Ok(());
    }

    LOGGER.write_json_line(&json!({
        "ts": now_ts(),
        "dir": "meta",
        "kind": "session_start",
        "cwd": config.cwd,
        "model": config.model,
        "model_provider_id": config.model_provider_id,
        "model_provider_name": config.model_provider.name,
    }));

    Ok(())
}

pub(crate) fn log_inbound_app_event(event: &AppEvent) {
    if !LOGGER.is_enabled() {
        return;
    }

    match LOGGER.mode() {
        Some(LogMode::EventStream) => match event {
            AppEvent::CodexEvent(ev) => LOGGER.write_protocol_event(ev, None),
            AppEvent::ThreadEvent { thread_id, event } => {
                LOGGER.write_protocol_event(event, Some(thread_id))
            }
            _ => {}
        },
        Some(LogMode::Legacy) => match event {
            AppEvent::CodexEvent(ev) => {
                write_legacy_record("to_tui", "codex_event", ev);
            }
            AppEvent::NewSession => {
                LOGGER.write_json_line(&json!({
                    "ts": now_ts(),
                    "dir": "to_tui",
                    "kind": "new_session",
                }));
            }
            AppEvent::ClearUi => {
                LOGGER.write_json_line(&json!({
                    "ts": now_ts(),
                    "dir": "to_tui",
                    "kind": "clear_ui",
                }));
            }
            AppEvent::InsertHistoryCell(cell) => {
                LOGGER.write_json_line(&json!({
                    "ts": now_ts(),
                    "dir": "to_tui",
                    "kind": "insert_history_cell",
                    "lines": cell.transcript_lines(u16::MAX).len(),
                }));
            }
            AppEvent::StartFileSearch(query) => {
                LOGGER.write_json_line(&json!({
                    "ts": now_ts(),
                    "dir": "to_tui",
                    "kind": "file_search_start",
                    "query": query,
                }));
            }
            AppEvent::FileSearchResult { query, matches } => {
                LOGGER.write_json_line(&json!({
                    "ts": now_ts(),
                    "dir": "to_tui",
                    "kind": "file_search_result",
                    "query": query,
                    "matches": matches.len(),
                }));
            }
            other => {
                LOGGER.write_json_line(&json!({
                    "ts": now_ts(),
                    "dir": "to_tui",
                    "kind": "app_event",
                    "variant": format!("{other:?}").split('(').next().unwrap_or("app_event"),
                }));
            }
        },
        None => {}
    }
}

pub(crate) fn log_outbound_op(op: &Op, thread_id_override: Option<&ThreadId>) {
    if !LOGGER.is_enabled() {
        return;
    }

    match LOGGER.mode() {
        Some(LogMode::EventStream) => {
            let payload = match serde_json::to_value(op) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!("event stream serialize error: {err}");
                    return;
                }
            };
            LOGGER.write_event_stream_record(
                "op_submitted",
                payload,
                thread_id_override.map(ToString::to_string),
            );
        }
        Some(LogMode::Legacy) => write_legacy_record("from_tui", "op", op),
        None => {}
    }
}

pub(crate) fn log_session_end() {
    if !LOGGER.is_enabled() {
        return;
    }

    match LOGGER.mode() {
        Some(LogMode::EventStream) => {
            LOGGER.write_event_stream_record("session_end", json!({}), None);
        }
        Some(LogMode::Legacy) => {
            LOGGER.write_json_line(&json!({
                "ts": now_ts(),
                "dir": "meta",
                "kind": "session_end",
            }));
        }
        None => {}
    }
}

fn write_legacy_record<T>(dir: &str, kind: &str, obj: &T)
where
    T: Serialize,
{
    LOGGER.write_json_line(&json!({
        "ts": now_ts(),
        "dir": dir,
        "kind": kind,
        "payload": obj,
    }));
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::EVENT_SCHEMA_CURRENT;
    use super::EVENT_SCHEMA_V1;
    use super::EVENT_SCHEMA_V2;
    use super::normalize_contract_event_type;
    use super::validate_schema_version;

    fn assert_common_fields(record: &Value) {
        assert!(record.get("schema_version").and_then(Value::as_u64).is_some());
        assert!(record.get("ts").and_then(Value::as_str).is_some());
        assert!(record.get("session_id").and_then(Value::as_str).is_some());
        assert!(record.get("event_type").and_then(Value::as_str).is_some());
        assert!(record.get("payload").is_some());
        assert!(record.get("payload").is_some_and(Value::is_object));
    }

    #[test]
    fn accepts_previous_schema_fixture() {
        let raw = include_str!("../tests/fixtures/event_stream/v1_turn_started.json");
        let record: Value = serde_json::from_str(raw).expect("fixture parses");
        assert_common_fields(&record);
        assert_eq!(record["schema_version"], Value::from(EVENT_SCHEMA_V1));
        assert!(record.get("seq").is_none());
        assert!(record.get("session_epoch").is_none());
    }

    #[test]
    fn accepts_current_schema_fixture() {
        let raw = include_str!("../tests/fixtures/event_stream/v2_turn_started.json");
        let record: Value = serde_json::from_str(raw).expect("fixture parses");
        assert_common_fields(&record);
        assert_eq!(record["schema_version"], Value::from(EVENT_SCHEMA_CURRENT));
        assert!(record.get("seq").and_then(Value::as_u64).is_some());
        assert!(record.get("session_epoch").and_then(Value::as_u64).is_some());
    }

    #[test]
    fn rejects_unsupported_schema_versions() {
        assert!(validate_schema_version(0).is_err());
        assert!(validate_schema_version(EVENT_SCHEMA_V2 + 1).is_err());
        assert!(validate_schema_version(EVENT_SCHEMA_V1).is_ok());
        assert!(validate_schema_version(EVENT_SCHEMA_V2).is_ok());
    }

    #[test]
    fn normalizes_legacy_turn_event_names() {
        assert_eq!(normalize_contract_event_type("task_started"), "turn_started");
        assert_eq!(normalize_contract_event_type("task_complete"), "turn_complete");
        assert_eq!(normalize_contract_event_type("exec_command_begin"), "exec_command_begin");
    }
}
