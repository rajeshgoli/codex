use crate::app_command::AppCommand;
use crate::app_event::AppEvent;
use crate::app_event::ExitMode;
use crate::app_event_sender::AppEventSender;
use crate::chatwidget::normalize_thread_name;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::ToolRequestUserInputResponse;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(unix)]
use std::io::BufReader;
#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

mod supervisor;

pub(crate) use supervisor::ControlSocketHandle;

const REQUEST_CACHE_CAPACITY: usize = 2048;
const REQUEST_MAX_CHARS: usize = 1 << 20;
const REQUEST_ID_MAX_CHARS: usize = 256;
const MAX_CONNECTION_WORKERS: usize = 64;
const MAX_EXTERNAL_BTW_PROMPT_BYTES: usize = 4 * 1024;

struct ControlState {
    epoch: String,
    app_event_tx: AppEventSender,
    cache: Mutex<RequestCache>,
}

impl ControlState {
    fn new(app_event_tx: AppEventSender, epoch: String) -> Self {
        Self {
            epoch,
            app_event_tx,
            cache: Mutex::new(RequestCache::default()),
        }
    }
}

#[derive(Default)]
struct RequestCache {
    order: VecDeque<String>,
    entries: HashMap<String, ControlResponse>,
}

impl RequestCache {
    fn get(&self, request_id: &str) -> Option<ControlResponse> {
        self.entries.get(request_id).cloned()
    }

    fn insert(&mut self, request_id: String, response: ControlResponse) {
        match self.entries.entry(request_id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.insert(response);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                self.order.push_back(request_id);
                entry.insert(response);
                while self.order.len() > REQUEST_CACHE_CAPACITY {
                    if let Some(oldest) = self.order.pop_front() {
                        self.entries.remove(&oldest);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ControlRequest {
    request_id: String,
    #[serde(default)]
    expected_epoch: Option<String>,
    #[serde(flatten)]
    command: ControlCommand,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum ControlCommand {
    GetEpoch,
    SubmitMessage {
        message: String,
        thread_id: Option<String>,
    },
    SetThreadName {
        name: String,
        thread_id: Option<String>,
    },
    SubmitBtw {
        prompt: String,
    },
    SubmitApproval {
        id: String,
        decision: String,
        #[serde(default)]
        approval_kind: ApprovalKind,
        thread_id: Option<String>,
        turn_id: Option<String>,
    },
    SubmitUserInput {
        id: String,
        response: Value,
        thread_id: Option<String>,
    },
    Shutdown {
        #[serde(default)]
        immediate: bool,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum ApprovalKind {
    #[default]
    Exec,
    Patch,
}

#[derive(Debug, Clone, Serialize)]
struct ControlResponse {
    request_id: String,
    ok: bool,
    epoch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ControlError>,
}

#[derive(Debug, Clone, Serialize)]
struct ControlError {
    code: String,
    message: String,
}

fn response_ok(request_id: &str, epoch: &str, result: Value) -> ControlResponse {
    ControlResponse {
        request_id: request_id.to_string(),
        ok: true,
        epoch: epoch.to_string(),
        result: Some(result),
        error: None,
    }
}

fn response_err(
    request_id: &str,
    epoch: &str,
    code: &str,
    message: impl Into<String>,
) -> ControlResponse {
    ControlResponse {
        request_id: request_id.to_string(),
        ok: false,
        epoch: epoch.to_string(),
        result: None,
        error: Some(ControlError {
            code: code.to_string(),
            message: message.into(),
        }),
    }
}

fn process_request(state: &Arc<ControlState>, request: ControlRequest) -> ControlResponse {
    if request.request_id.trim().is_empty() {
        return response_err(
            "",
            &state.epoch,
            "invalid_request",
            "request_id must be a non-empty string",
        );
    }
    if request.request_id.len() > REQUEST_ID_MAX_CHARS {
        return response_err(
            "",
            &state.epoch,
            "invalid_request",
            format!("request_id exceeds max length of {REQUEST_ID_MAX_CHARS} characters"),
        );
    }

    let request_id = request.request_id.clone();
    let Ok(mut cache) = state.cache.lock() else {
        return response_err(
            &request_id,
            &state.epoch,
            "internal_error",
            "control cache lock poisoned",
        );
    };
    if let Some(cached) = cache.get(&request_id) {
        return cached;
    }

    if let Some(expected_epoch) = request.expected_epoch.as_deref()
        && expected_epoch != state.epoch
    {
        let response = response_err(
            &request.request_id,
            &state.epoch,
            "stale_epoch",
            format!(
                "expected_epoch={expected_epoch} does not match current epoch={}",
                state.epoch
            ),
        );
        return response;
    }

    let response = match request.command {
        ControlCommand::GetEpoch => response_ok(
            &request_id,
            &state.epoch,
            json!({
                "epoch": state.epoch,
            }),
        ),
        ControlCommand::SubmitMessage { message, thread_id } => {
            if message.trim().is_empty() {
                response_err(
                    &request_id,
                    &state.epoch,
                    "invalid_request",
                    "message must be non-empty",
                )
            } else {
                match parse_thread_id(thread_id) {
                    Ok(Some(_thread_id)) => response_err(
                        &request_id,
                        &state.epoch,
                        "unsupported_request",
                        "thread_id is not supported for submit_message in this build",
                    ),
                    Ok(None) => match dispatch_app_event(
                        state,
                        AppEvent::SubmitExternalLiteralUserMessage { text: message },
                    ) {
                        Ok(()) => response_ok(
                            &request_id,
                            &state.epoch,
                            json!({"status": "accepted", "operation": "submit_message"}),
                        ),
                        Err(err) => {
                            response_err(&request_id, &state.epoch, "event_channel_closed", err)
                        }
                    },
                    Err(err) => response_err(&request_id, &state.epoch, "invalid_request", err),
                }
            }
        }
        ControlCommand::SetThreadName { name, thread_id } => {
            if let Some(name) = normalize_thread_name(&name) {
                match parse_thread_id(thread_id) {
                    Ok(thread_id) => {
                        match dispatch_command(state, thread_id, AppCommand::set_thread_name(name))
                        {
                            Ok(()) => response_ok(
                                &request_id,
                                &state.epoch,
                                json!({"status": "accepted", "operation": "set_thread_name"}),
                            ),
                            Err(err) => {
                                response_err(&request_id, &state.epoch, "event_channel_closed", err)
                            }
                        }
                    }
                    Err(err) => response_err(&request_id, &state.epoch, "invalid_request", err),
                }
            } else {
                response_err(
                    &request_id,
                    &state.epoch,
                    "invalid_request",
                    "thread name must not be empty",
                )
            }
        }
        ControlCommand::SubmitBtw { prompt } => {
            if prompt.trim().is_empty() {
                response_err(
                    &request_id,
                    &state.epoch,
                    "invalid_request",
                    "prompt must be non-empty",
                )
            } else if prompt.len() > MAX_EXTERNAL_BTW_PROMPT_BYTES {
                response_err(
                    &request_id,
                    &state.epoch,
                    "invalid_request",
                    format!("prompt exceeds {MAX_EXTERNAL_BTW_PROMPT_BYTES} UTF-8 bytes"),
                )
            } else {
                match dispatch_app_event(
                    state,
                    AppEvent::StartExternalBtw {
                        request_id: request_id.clone(),
                        prompt,
                    },
                ) {
                    Ok(()) => response_ok(
                        &request_id,
                        &state.epoch,
                        json!({"status": "accepted", "operation": "submit_btw"}),
                    ),
                    Err(err) => {
                        response_err(&request_id, &state.epoch, "event_channel_closed", err)
                    }
                }
            }
        }
        ControlCommand::SubmitApproval {
            id,
            decision,
            approval_kind,
            thread_id,
            turn_id,
        } => match parse_thread_id(thread_id) {
            Ok(thread_id) => match approval_kind {
                ApprovalKind::Exec => match parse_exec_approval_decision(decision.as_str()) {
                    Some(decision) => {
                        let command = AppCommand::exec_approval(id, turn_id, decision);
                        match dispatch_command(state, thread_id, command) {
                            Ok(()) => response_ok(
                                &request_id,
                                &state.epoch,
                                json!({"status": "accepted", "operation": "submit_approval"}),
                            ),
                            Err(err) => {
                                response_err(&request_id, &state.epoch, "event_channel_closed", err)
                            }
                        }
                    }
                    None => response_err(
                        &request_id,
                        &state.epoch,
                        "invalid_request",
                        "exec approval decision must be one of: approved, approved_for_session, denied, abort",
                    ),
                },
                ApprovalKind::Patch => match parse_patch_approval_decision(decision.as_str()) {
                    Some(decision) => {
                        let command = AppCommand::patch_approval(id, decision);
                        match dispatch_command(state, thread_id, command) {
                            Ok(()) => response_ok(
                                &request_id,
                                &state.epoch,
                                json!({"status": "accepted", "operation": "submit_approval"}),
                            ),
                            Err(err) => {
                                response_err(&request_id, &state.epoch, "event_channel_closed", err)
                            }
                        }
                    }
                    None => response_err(
                        &request_id,
                        &state.epoch,
                        "invalid_request",
                        "patch approval decision must be one of: approved, approved_for_session, denied, abort",
                    ),
                },
            },
            Err(err) => response_err(&request_id, &state.epoch, "invalid_request", err),
        },
        ControlCommand::SubmitUserInput {
            id,
            response,
            thread_id,
        } => match parse_thread_id(thread_id) {
            Ok(thread_id) => match serde_json::from_value::<ToolRequestUserInputResponse>(response)
            {
                Ok(parsed_response) => {
                    let command = AppCommand::user_input_answer(id, parsed_response);
                    match dispatch_command(state, thread_id, command) {
                        Ok(()) => response_ok(
                            &request_id,
                            &state.epoch,
                            json!({"status": "accepted", "operation": "submit_user_input"}),
                        ),
                        Err(err) => {
                            response_err(&request_id, &state.epoch, "event_channel_closed", err)
                        }
                    }
                }
                Err(err) => response_err(
                    &request_id,
                    &state.epoch,
                    "invalid_request",
                    format!("response is invalid: {err}"),
                ),
            },
            Err(err) => response_err(&request_id, &state.epoch, "invalid_request", err),
        },
        ControlCommand::Shutdown { immediate } => {
            let exit_mode = if immediate {
                ExitMode::Immediate
            } else {
                ExitMode::ShutdownFirst
            };
            match dispatch_app_event(state, AppEvent::Exit(exit_mode)) {
                Ok(()) => response_ok(
                    &request_id,
                    &state.epoch,
                    json!({"status": "accepted", "operation": "shutdown", "immediate": immediate}),
                ),
                Err(err) => response_err(&request_id, &state.epoch, "event_channel_closed", err),
            }
        }
    };

    cache.insert(request_id, response.clone());
    response
}

fn parse_thread_id(raw: Option<String>) -> Result<Option<ThreadId>, String> {
    match raw {
        Some(value) => ThreadId::from_string(&value)
            .map(Some)
            .map_err(|_| format!("thread_id is not a valid UUID: {value}")),
        None => Ok(None),
    }
}

fn parse_exec_approval_decision(raw: &str) -> Option<CommandExecutionApprovalDecision> {
    match raw {
        "approved" => Some(CommandExecutionApprovalDecision::Accept),
        "approved_for_session" => Some(CommandExecutionApprovalDecision::AcceptForSession),
        "denied" => Some(CommandExecutionApprovalDecision::Decline),
        "abort" => Some(CommandExecutionApprovalDecision::Cancel),
        _ => None,
    }
}

fn parse_patch_approval_decision(raw: &str) -> Option<FileChangeApprovalDecision> {
    match raw {
        "approved" => Some(FileChangeApprovalDecision::Accept),
        "approved_for_session" => Some(FileChangeApprovalDecision::AcceptForSession),
        "denied" => Some(FileChangeApprovalDecision::Decline),
        "abort" => Some(FileChangeApprovalDecision::Cancel),
        _ => None,
    }
}

fn dispatch_command(
    state: &ControlState,
    thread_id: Option<ThreadId>,
    op: AppCommand,
) -> Result<(), String> {
    match thread_id {
        Some(thread_id) => dispatch_app_event(state, AppEvent::SubmitThreadOp { thread_id, op }),
        None => dispatch_app_event(state, AppEvent::CodexOp(op)),
    }
}

fn dispatch_app_event(state: &ControlState, event: AppEvent) -> Result<(), String> {
    state
        .app_event_tx
        .app_event_tx
        .send(event)
        .map_err(|_| "control socket is unavailable; app event channel is closed".to_string())
}

#[cfg(unix)]
fn handle_connection(
    mut stream: UnixStream,
    state: Arc<ControlState>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        match read_line_capped(&mut reader, &shutdown)? {
            ReadLineOutcome::Eof => return Ok(()),
            ReadLineOutcome::TooLarge => {
                let response = response_err(
                    "",
                    &state.epoch,
                    "invalid_request",
                    format!("request exceeds max length of {REQUEST_MAX_CHARS} characters"),
                );
                write_response(&mut stream, &response)?;
            }
            ReadLineOutcome::Line(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let response = match serde_json::from_str::<ControlRequest>(line) {
                    Ok(request) => process_request(&state, request),
                    Err(err) => response_err(
                        "",
                        &state.epoch,
                        "invalid_json",
                        format!("failed to parse request JSON: {err}"),
                    ),
                };
                write_response(&mut stream, &response)?;
            }
        }
    }
}

#[cfg(unix)]
enum ReadLineOutcome {
    Eof,
    Line(String),
    TooLarge,
}

#[cfg(unix)]
fn read_line_capped(
    reader: &mut BufReader<UnixStream>,
    shutdown: &Arc<AtomicBool>,
) -> std::io::Result<ReadLineOutcome> {
    let mut bytes = Vec::new();
    let mut too_large = false;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(ReadLineOutcome::Eof);
        }
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => {
                if bytes.is_empty() && !too_large {
                    return Ok(ReadLineOutcome::Eof);
                }
                break;
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if !too_large {
                    bytes.push(byte[0]);
                    if bytes.len() > REQUEST_MAX_CHARS {
                        too_large = true;
                        bytes.clear();
                    }
                }
            }
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    if too_large {
        return Ok(ReadLineOutcome::TooLarge);
    }
    let line = String::from_utf8(bytes).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("request is not valid UTF-8: {err}"),
        )
    })?;
    Ok(ReadLineOutcome::Line(line))
}

#[cfg(unix)]
fn write_response(stream: &mut UnixStream, response: &ControlResponse) -> std::io::Result<()> {
    let encoded = serde_json::to_string(response).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to encode control response: {err}"),
        )
    })?;
    stream.write_all(encoded.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::sync::mpsc::unbounded_channel;

    fn test_state() -> (
        Arc<ControlState>,
        tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let (tx, rx) = unbounded_channel();
        let sender = AppEventSender::new(tx);
        let state = Arc::new(ControlState::new(sender, "epoch-1".to_string()));
        (state, rx)
    }

    #[test]
    fn get_epoch_returns_current_epoch() {
        let (state, _rx) = test_state();
        let response = process_request(
            &state,
            ControlRequest {
                request_id: "req-1".to_string(),
                expected_epoch: None,
                command: ControlCommand::GetEpoch,
            },
        );
        assert!(response.ok);
        assert_eq!(response.epoch, "epoch-1");
    }

    #[test]
    fn duplicate_request_id_returns_cached_response_once() {
        let (state, mut rx) = test_state();
        let request = ControlRequest {
            request_id: "req-dup".to_string(),
            expected_epoch: None,
            command: ControlCommand::SubmitMessage {
                message: "hello".to_string(),
                thread_id: None,
            },
        };

        let first = process_request(&state, request.clone());
        let second = process_request(&state, request);
        assert!(first.ok);
        assert_eq!(first.request_id, second.request_id);
        assert_eq!(first.ok, second.ok);
        assert_eq!(first.epoch, second.epoch);

        match rx.try_recv() {
            Ok(AppEvent::SubmitExternalLiteralUserMessage { text }) => {
                assert_eq!(text, "hello")
            }
            other => panic!("expected one external user message event, got {other:?}"),
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn set_thread_name_dispatches_thread_name_command() {
        let (state, mut rx) = test_state();
        let response = process_request(
            &state,
            ControlRequest {
                request_id: "req-set-name".to_string(),
                expected_epoch: None,
                command: ControlCommand::SetThreadName {
                    name: "  worker-one  ".to_string(),
                    thread_id: None,
                },
            },
        );

        assert!(response.ok);
        match rx.try_recv() {
            Ok(AppEvent::CodexOp(AppCommand::SetThreadName { name })) => {
                assert_eq!(name, "worker-one");
            }
            other => panic!("expected set thread name command, got {other:?}"),
        }
    }

    #[test]
    fn set_thread_name_with_thread_id_dispatches_thread_command() {
        let (state, mut rx) = test_state();
        let thread_id = ThreadId::new();
        let response = process_request(
            &state,
            ControlRequest {
                request_id: "req-thread-set-name".to_string(),
                expected_epoch: None,
                command: ControlCommand::SetThreadName {
                    name: "worker-two".to_string(),
                    thread_id: Some(thread_id.to_string()),
                },
            },
        );

        assert!(response.ok);
        match rx.try_recv() {
            Ok(AppEvent::SubmitThreadOp {
                thread_id: actual_thread_id,
                op: AppCommand::SetThreadName { name },
            }) => {
                assert_eq!(actual_thread_id, thread_id);
                assert_eq!(name, "worker-two");
            }
            other => panic!("expected thread-scoped set thread name command, got {other:?}"),
        }
    }

    #[test]
    fn set_thread_name_rejects_empty_name() {
        let (state, mut rx) = test_state();
        let response = process_request(
            &state,
            ControlRequest {
                request_id: "req-empty-name".to_string(),
                expected_epoch: None,
                command: ControlCommand::SetThreadName {
                    name: "   ".to_string(),
                    thread_id: None,
                },
            },
        );

        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_request")
        );
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn submit_btw_dispatches_correlated_external_event() {
        let (state, mut rx) = test_state();
        let response = process_request(
            &state,
            ControlRequest {
                request_id: "req-btw".to_string(),
                expected_epoch: None,
                command: ControlCommand::SubmitBtw {
                    prompt: "Summarize the current work".to_string(),
                },
            },
        );

        assert!(response.ok);
        match rx.try_recv() {
            Ok(AppEvent::StartExternalBtw { request_id, prompt }) => {
                assert_eq!(request_id, "req-btw");
                assert_eq!(prompt, "Summarize the current work");
            }
            other => panic!("expected external btw event, got {other:?}"),
        }
    }

    #[test]
    fn submit_btw_rejects_empty_prompt() {
        let (state, mut rx) = test_state();
        let response = process_request(
            &state,
            ControlRequest {
                request_id: "req-empty-btw".to_string(),
                expected_epoch: None,
                command: ControlCommand::SubmitBtw {
                    prompt: "   ".to_string(),
                },
            },
        );

        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_request")
        );
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn submit_btw_rejects_oversized_prompt() {
        let (state, mut rx) = test_state();
        let response = process_request(
            &state,
            ControlRequest {
                request_id: "req-large-btw".to_string(),
                expected_epoch: None,
                command: ControlCommand::SubmitBtw {
                    prompt: "x".repeat(MAX_EXTERNAL_BTW_PROMPT_BYTES + 1),
                },
            },
        );

        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_request")
        );
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn expected_epoch_mismatch_returns_stale_epoch_error() {
        let (state, mut rx) = test_state();
        let response = process_request(
            &state,
            ControlRequest {
                request_id: "req-stale".to_string(),
                expected_epoch: Some("epoch-old".to_string()),
                command: ControlCommand::GetEpoch,
            },
        );
        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|e| e.code.as_str()),
            Some("stale_epoch")
        );
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        let retry = process_request(
            &state,
            ControlRequest {
                request_id: "req-stale".to_string(),
                expected_epoch: Some(state.epoch.clone()),
                command: ControlCommand::GetEpoch,
            },
        );
        assert!(retry.ok);
    }
}
