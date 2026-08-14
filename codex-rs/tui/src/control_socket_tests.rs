use super::*;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::unbounded_channel;

fn test_state() -> (
    Arc<ControlState>,
    tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) {
    let (tx, rx) = unbounded_channel();
    let sender = AppEventSender::new(tx);
    let state = Arc::new(ControlState::with_cache(
        sender,
        "epoch-1".to_string(),
        Arc::new(Mutex::new(RequestCache::default())),
    ));
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
fn get_epoch_request_id_returns_fresh_result_across_listener_generations() {
    let cache = Arc::new(Mutex::new(RequestCache::default()));
    let (tx, _rx) = unbounded_channel();
    let first = Arc::new(ControlState::with_cache(
        AppEventSender::new(tx.clone()),
        "epoch-1".to_string(),
        Arc::clone(&cache),
    ));
    let second = Arc::new(ControlState::with_cache(
        AppEventSender::new(tx),
        "epoch-2".to_string(),
        cache,
    ));
    let request = ControlRequest {
        request_id: "same-get-epoch".to_string(),
        expected_epoch: None,
        command: ControlCommand::GetEpoch,
    };

    let first_response = process_request(&first, request.clone());
    let second_response = process_request(&second, request);

    assert_eq!(first_response.epoch, "epoch-1");
    assert_eq!(first_response.result, Some(json!({"epoch": "epoch-1"})));
    assert_eq!(second_response.epoch, "epoch-2");
    assert_eq!(second_response.result, Some(json!({"epoch": "epoch-2"})));
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

#[test]
fn expected_epoch_is_checked_before_cached_response() {
    let (state, _rx) = test_state();
    let first = process_request(
        &state,
        ControlRequest {
            request_id: "req-cached-epoch".to_string(),
            expected_epoch: Some(state.epoch.clone()),
            command: ControlCommand::GetEpoch,
        },
    );
    assert!(first.ok);

    let stale = process_request(
        &state,
        ControlRequest {
            request_id: "req-cached-epoch".to_string(),
            expected_epoch: Some("epoch-old".to_string()),
            command: ControlCommand::GetEpoch,
        },
    );
    assert!(!stale.ok);
    assert_eq!(
        stale.error.as_ref().map(|error| error.code.as_str()),
        Some("stale_epoch")
    );
}
