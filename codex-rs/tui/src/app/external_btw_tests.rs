use super::*;
use crate::app::test_support::make_test_app;
use crate::app::test_support::make_test_app_with_channels;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnItemsView;

#[tokio::test]
async fn hidden_notifications_capture_answer_and_finish_request() {
    let mut app = make_test_app().await;
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    app.external_btw_requests.insert(
        child_thread_id,
        ExternalBtwState {
            request_id: "req-btw".to_string(),
            parent_thread_id,
            answer: None,
        },
    );

    let item_completed = ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: child_thread_id.to_string(),
        turn_id: "turn-btw".to_string(),
        completed_at_ms: 1,
        item: ThreadItem::AgentMessage {
            id: "answer-btw".to_string(),
            text: "current summary".to_string(),
            phase: None,
            memory_citation: None,
        },
    });
    assert!(app.handle_external_btw_notification(child_thread_id, &item_completed));
    assert_eq!(
        app.external_btw_requests
            .get(&child_thread_id)
            .and_then(|state| state.answer.as_deref()),
        Some("current summary")
    );

    let turn_completed = ServerNotification::TurnCompleted(TurnCompletedNotification {
        thread_id: child_thread_id.to_string(),
        turn: Turn {
            id: "turn-btw".to_string(),
            items: Vec::new(),
            items_view: TurnItemsView::Full,
            status: TurnStatus::Completed,
            error: None,
            started_at: Some(0),
            completed_at: Some(1),
            duration_ms: Some(1),
        },
    });
    assert!(app.handle_external_btw_notification(child_thread_id, &turn_completed));
    assert!(!app.external_btw_requests.contains_key(&child_thread_id));
}

#[tokio::test]
async fn unrelated_notifications_remain_visible() {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    let notification = ServerNotification::TurnCompleted(TurnCompletedNotification {
        thread_id: thread_id.to_string(),
        turn: Turn {
            id: "turn-main".to_string(),
            items: Vec::new(),
            items_view: TurnItemsView::Full,
            status: TurnStatus::Completed,
            error: None,
            started_at: Some(0),
            completed_at: Some(1),
            duration_ms: Some(1),
        },
    });

    assert!(!app.handle_external_btw_notification(thread_id, &notification));
}

#[test]
fn fresh_session_transition_cleans_up_external_btw_through_event_dispatch() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 32 * 1024 * 1024;
    std::thread::Builder::new()
        .name("external-btw-replacement-test".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
                let mut app_server =
                    Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
                let child = app_server.start_thread(&app.config).await?;
                let child_thread_id = child.session.thread_id;
                app.ensure_thread_channel(child_thread_id);
                while app_event_rx.try_recv().is_ok() {}
                app.external_btw_requests.insert(
                    child_thread_id,
                    ExternalBtwState {
                        request_id: "req-reset".to_string(),
                        parent_thread_id: ThreadId::new(),
                        answer: None,
                    },
                );
                let mut tui = crate::tui::test_support::make_test_tui()?;

                Box::pin(app.handle_event(
                    &mut tui,
                    &mut app_server,
                    AppEvent::NewSession { name: None },
                ))
                .await?;

                assert!(app.external_btw_requests.is_empty());
                let cleanup = std::iter::from_fn(|| app_event_rx.try_recv().ok())
            .find(|event| {
                matches!(
                    event,
                    AppEvent::CleanupExternalBtw { thread_id } if *thread_id == child_thread_id
                )
            })
            .expect("session replacement should queue external BTW cleanup");
                Box::pin(app.handle_event(&mut tui, &mut app_server, cleanup)).await?;

                assert!(!app.thread_event_channels.contains_key(&child_thread_id));
                let request_id = app_server.next_request_id();
                let response = app_server
                    .request_handle()
                    .request_typed::<ThreadUnsubscribeResponse>(ClientRequest::ThreadUnsubscribe {
                        request_id,
                        params: ThreadUnsubscribeParams {
                            thread_id: child_thread_id.to_string(),
                        },
                    })
                    .await?;
                assert_eq!(response.status, ThreadUnsubscribeStatus::NotSubscribed);
                Ok(())
            })
        })
        .expect("replacement test thread should start")
        .join()
        .expect("replacement test thread should not panic")
}
