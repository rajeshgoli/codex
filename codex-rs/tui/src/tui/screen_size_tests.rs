use std::time::Duration;

use pretty_assertions::assert_eq;
use ratatui::layout::Size;

use crate::tui::TuiEvent;

#[tokio::test]
async fn draw_size_policy_reuses_recent_geometry_between_checks() {
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    let cached = Size::new(/*width*/ 120, /*height*/ 40);
    let resumed = tui.terminal.size().expect("backend size");
    tui.screen_size.last_backend_check = Some(std::time::Instant::now());
    tui.terminal.last_known_screen_size = cached;
    let resized = Size::new(/*width*/ 100, /*height*/ 30);
    for (event, expected) in [
        (TuiEvent::Draw, cached),
        (TuiEvent::Resume, resumed),
        (TuiEvent::Resize(resized), resized),
        (TuiEvent::Paste(String::new()), cached),
        (TuiEvent::Draw, resized),
        (TuiEvent::Paste(String::new()), cached),
    ] {
        assert_eq!(tui.screen_size_for_event(&event).expect("size"), expected);
        if matches!(event, TuiEvent::Resize(_)) {
            tui.defer_screen_size(resized);
        }
    }
    assert_eq!(tui.take_event_screen_size().expect("size"), resumed);
    assert!(tui.screen_size.pending_recheck_at.is_none());
}

#[tokio::test]
async fn standalone_resize_draw_rechecks_settled_screen_size_once() {
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    let resize_size = Size::new(/*width*/ 120, /*height*/ 40);

    tui.screen_size_for_event(&TuiEvent::Resize(resize_size))
        .expect("resolve resize");
    tui.terminal.resize(resize_size).expect("apply resize");
    assert_eq!(
        tui.screen_size_for_event(&TuiEvent::Draw)
            .expect("resolve early draw"),
        resize_size
    );

    tui.schedule_screen_size_recheck(Duration::ZERO);
    assert_eq!(
        tui.screen_size_for_event(&TuiEvent::Draw)
            .expect("resolve settled draw"),
        tui.terminal.size().expect("terminal size")
    );
    assert!(tui.screen_size.pending_recheck_at.is_none());
}

#[tokio::test]
async fn entering_alternate_screen_updates_cached_screen_size() {
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    let screen_size = tui.terminal.size().expect("terminal size");
    tui.terminal.last_known_screen_size = Size::new(/*width*/ 120, /*height*/ 40);

    tui.enter_alt_screen().expect("enter alternate screen");

    assert_eq!(tui.terminal.last_known_screen_size, screen_size);
    tui.leave_alt_screen().expect("leave alternate screen");
}

#[tokio::test]
async fn redraw_recovers_from_a_missed_resize_notification() {
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    let actual = tui.terminal.size().expect("backend size");
    let stale = Size::new(/*width*/ 153, /*height*/ 51);
    tui.terminal.last_known_screen_size = stale;
    tui.screen_size.last_backend_check = Some(std::time::Instant::now() - Duration::from_secs(2));

    assert_eq!(
        tui.screen_size_for_event(&TuiEvent::Draw).expect("size"),
        actual
    );
    assert_eq!(tui.take_event_screen_size().expect("draw size"), actual);
    assert!(tui.screen_size.last_backend_check.is_some());

    // Once the draw has applied the sampled size, the next frame can reuse it.
    tui.terminal.resize(actual).expect("apply size");
    let checked_at = tui.screen_size.last_backend_check;
    assert_eq!(
        tui.screen_size_for_event(&TuiEvent::Draw).expect("size"),
        actual
    );
    assert_eq!(tui.screen_size.last_backend_check, checked_at);
}

#[tokio::test]
async fn focus_gain_refreshes_geometry_even_after_a_recent_check() {
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    let actual = tui.terminal.size().expect("backend size");
    tui.terminal.last_known_screen_size = Size::new(/*width*/ 153, /*height*/ 51);
    tui.screen_size.last_backend_check = Some(std::time::Instant::now());
    tui.defer_screen_size(Size::new(/*width*/ 100, /*height*/ 30));

    assert_eq!(
        tui.screen_size_for_event(&TuiEvent::FocusGained)
            .expect("size"),
        actual
    );
    assert_eq!(tui.take_event_screen_size().expect("draw size"), actual);
    assert!(tui.screen_size.deferred_size.is_none());
}

#[tokio::test]
async fn recent_cached_draw_schedules_idle_geometry_recovery() {
    let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
    let (requester, mut requests) = crate::tui::FrameRequester::test_channel();
    tui.frame_requester = requester;
    let actual = tui.terminal.size().expect("backend size");
    let stale = Size::new(/*width*/ 153, /*height*/ 51);
    tui.terminal.last_known_screen_size = stale;
    let checked = std::time::Instant::now();
    tui.screen_size.last_backend_check = Some(checked);

    assert_eq!(
        tui.screen_size_for_event(&TuiEvent::Draw)
            .expect("early draw"),
        stale
    );
    let deadline = requests
        .try_recv()
        .expect("cached draw must schedule a refresh");
    assert!(deadline >= checked + Duration::from_secs(1));
    assert!(deadline <= std::time::Instant::now() + Duration::from_secs(1));

    // Simulate the scheduled draw after expiry, with no intervening input or resize.
    tui.screen_size.last_backend_check = Some(checked - Duration::from_secs(2));
    assert_eq!(
        tui.screen_size_for_event(&TuiEvent::Draw)
            .expect("scheduled draw"),
        actual
    );
    assert_eq!(tui.take_event_screen_size().expect("draw size"), actual);
}
