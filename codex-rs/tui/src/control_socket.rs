use crate::app_event::AppEvent;
use crate::app_event::ExitMode;
use crate::app_event_sender::AppEventSender;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::BufRead;
#[cfg(unix)]
use std::io::BufReader;
#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

const REQUEST_CACHE_CAPACITY: usize = 2048;
const REQUEST_MAX_CHARS: usize = 1 << 20;

pub(crate) struct ControlSocketHandle {
    shutdown: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
    socket_path: PathBuf,
}

impl ControlSocketHandle {
    pub(crate) fn start(socket_path: PathBuf, app_event_tx: AppEventSender) -> std::io::Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = socket_path;
            let _ = app_event_tx;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "--control-socket is currently supported on Unix only",
            ));
        }

        #[cfg(unix)]
        {
            if !socket_path.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "control socket path must be absolute",
                ));
            }
            let parent = socket_path.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "control socket path must include a parent directory",
                )
            })?;
            fs::create_dir_all(parent)?;
            remove_existing_socket_if_safe(&socket_path)?;

            let listener = UnixListener::bind(&socket_path)?;
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;

            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_for_thread = Arc::clone(&shutdown);
            let socket_path_for_thread = socket_path.clone();
            let state = Arc::new(ControlState::new(app_event_tx, uuid::Uuid::new_v4().to_string()));

            let join_handle = std::thread::Builder::new()
                .name("codex-control-socket".to_string())
                .spawn(move || {
                    run_listener_loop(listener, state, shutdown_for_thread);
                    if let Err(err) = fs::remove_file(&socket_path_for_thread) {
                        tracing::debug!(
                            "control socket cleanup ignored for {}: {err}",
                            socket_path_for_thread.display()
                        );
                    }
                })?;

            Ok(Self {
                shutdown,
                join_handle: Some(join_handle),
                socket_path,
            })
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join_handle) = self.join_handle.take()
            && let Err(err) = join_handle.join()
        {
            tracing::debug!("control socket thread join failed: {err:?}");
        }
        if let Err(err) = std::fs::remove_file(&self.socket_path) {
            tracing::debug!(
                "control socket post-shutdown cleanup ignored for {}: {err}",
                self.socket_path.display()
            );
        }
    }
}

impl Drop for ControlSocketHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

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
        if self.entries.contains_key(&request_id) {
            self.entries.insert(request_id, response);
            return;
        }
        self.order.push_back(request_id.clone());
        self.entries.insert(request_id, response);
        while self.order.len() > REQUEST_CACHE_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
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

fn response_err(request_id: &str, epoch: &str, code: &str, message: impl Into<String>) -> ControlResponse {
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

    if let Some(cached) = state
        .cache
        .lock()
        .expect("control cache lock poisoned")
        .get(&request.request_id)
    {
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
        state
            .cache
            .lock()
            .expect("control cache lock poisoned")
            .insert(request.request_id, response.clone());
        return response;
    }

    let response = match request.command {
        ControlCommand::GetEpoch => response_ok(
            &request.request_id,
            &state.epoch,
            json!({
                "epoch": state.epoch,
            }),
        ),
        ControlCommand::SubmitMessage { message, thread_id } => {
            if message.trim().is_empty() {
                response_err(
                    &request.request_id,
                    &state.epoch,
                    "invalid_request",
                    "message must be non-empty",
                )
            } else {
                match parse_thread_id(thread_id) {
                    Ok(thread_id) => {
                        let op = Op::UserInput {
                            items: vec![UserInput::Text {
                                text: message,
                                text_elements: Vec::new(),
                            }],
                            final_output_json_schema: None,
                        };
                        match dispatch_op(state, thread_id, op) {
                            Ok(()) => response_ok(
                                &request.request_id,
                                &state.epoch,
                                json!({"status": "accepted", "operation": "submit_message"}),
                            ),
                            Err(err) => response_err(
                                &request.request_id,
                                &state.epoch,
                                "event_channel_closed",
                                err,
                            ),
                        }
                    }
                    Err(err) => response_err(
                        &request.request_id,
                        &state.epoch,
                        "invalid_request",
                        err,
                    ),
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
            Ok(thread_id) => match parse_review_decision(decision.as_str()) {
                Some(decision) => {
                    let op = match approval_kind {
                        ApprovalKind::Exec => Op::ExecApproval {
                            id,
                            turn_id,
                            decision,
                        },
                        ApprovalKind::Patch => Op::PatchApproval { id, decision },
                    };
                    match dispatch_op(state, thread_id, op) {
                        Ok(()) => response_ok(
                            &request.request_id,
                            &state.epoch,
                            json!({"status": "accepted", "operation": "submit_approval"}),
                        ),
                        Err(err) => response_err(
                            &request.request_id,
                            &state.epoch,
                            "event_channel_closed",
                            err,
                        ),
                    }
                }
                None => response_err(
                    &request.request_id,
                    &state.epoch,
                    "invalid_request",
                    "decision must be one of: approved, approved_for_session, denied, abort",
                ),
            },
            Err(err) => response_err(
                &request.request_id,
                &state.epoch,
                "invalid_request",
                err,
            ),
        },
        ControlCommand::SubmitUserInput {
            id,
            response,
            thread_id,
        } => match parse_thread_id(thread_id) {
            Ok(thread_id) => match serde_json::from_value::<RequestUserInputResponse>(response) {
                Ok(parsed_response) => {
                    let op = Op::UserInputAnswer {
                        id,
                        response: parsed_response,
                    };
                    match dispatch_op(state, thread_id, op) {
                        Ok(()) => response_ok(
                            &request.request_id,
                            &state.epoch,
                            json!({"status": "accepted", "operation": "submit_user_input"}),
                        ),
                        Err(err) => response_err(
                            &request.request_id,
                            &state.epoch,
                            "event_channel_closed",
                            err,
                        ),
                    }
                }
                Err(err) => response_err(
                    &request.request_id,
                    &state.epoch,
                    "invalid_request",
                    format!("response is invalid: {err}"),
                ),
            },
            Err(err) => response_err(
                &request.request_id,
                &state.epoch,
                "invalid_request",
                err,
            ),
        },
        ControlCommand::Shutdown { immediate } => {
            let exit_mode = if immediate {
                ExitMode::Immediate
            } else {
                ExitMode::ShutdownFirst
            };
            match dispatch_app_event(state, AppEvent::Exit(exit_mode)) {
                Ok(()) => response_ok(
                    &request.request_id,
                    &state.epoch,
                    json!({"status": "accepted", "operation": "shutdown", "immediate": immediate}),
                ),
                Err(err) => response_err(
                    &request.request_id,
                    &state.epoch,
                    "event_channel_closed",
                    err,
                ),
            }
        }
    };

    state
        .cache
        .lock()
        .expect("control cache lock poisoned")
        .insert(request.request_id, response.clone());
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

fn parse_review_decision(raw: &str) -> Option<ReviewDecision> {
    match raw {
        "approved" => Some(ReviewDecision::Approved),
        "approved_for_session" => Some(ReviewDecision::ApprovedForSession),
        "denied" => Some(ReviewDecision::Denied),
        "abort" => Some(ReviewDecision::Abort),
        _ => None,
    }
}

fn dispatch_op(state: &ControlState, thread_id: Option<ThreadId>, op: Op) -> Result<(), String> {
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
fn remove_existing_socket_if_safe(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_socket() {
        fs::remove_file(path)?;
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "refusing to overwrite existing non-socket path: {}",
            path.display()
        ),
    ))
}

#[cfg(unix)]
fn run_listener_loop(listener: UnixListener, state: Arc<ControlState>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(err) = handle_connection(stream, Arc::clone(&state), Arc::clone(&shutdown)) {
                    tracing::warn!("control socket connection error: {err}");
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                tracing::warn!("control socket accept error: {err}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
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
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line.len() > REQUEST_MAX_CHARS {
                    let response = response_err(
                        "",
                        &state.epoch,
                        "invalid_request",
                        format!("request exceeds max length of {REQUEST_MAX_CHARS} characters"),
                    );
                    write_response(&mut stream, &response)?;
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
            Err(err) if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut => {
                continue;
            }
            Err(err) => return Err(err),
        }
    }
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

    fn test_state() -> (Arc<ControlState>, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
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
            Ok(AppEvent::CodexOp(Op::UserInput { .. })) => {}
            other => panic!("expected one CodexOp user input event, got {other:?}"),
        }
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
        assert_eq!(response.error.as_ref().map(|e| e.code.as_str()), Some("stale_epoch"));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }
}
