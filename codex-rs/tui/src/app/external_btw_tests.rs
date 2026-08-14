use super::*;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::ItemCompletedNotification;
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

#[tokio::test]
async fn fresh_session_transition_clears_active_requests() {
    let mut app = make_test_app().await;
    app.external_btw_requests.insert(
        ThreadId::new(),
        ExternalBtwState {
            request_id: "req-reset".to_string(),
            parent_thread_id: ThreadId::new(),
            answer: None,
        },
    );

    app.fail_all_external_btw("main_thread_replaced");

    assert!(app.external_btw_requests.is_empty());
}
