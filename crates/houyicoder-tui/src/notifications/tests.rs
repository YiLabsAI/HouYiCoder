use super::*;
use ratatui::style::Color;

fn txt(key: &str, prio: Priority) -> Notification {
    Notification {
        key: key.into(),
        priority: prio,
        timeout: Duration::from_millis(50),
        kind: NotifKind::text(key),
    }
}

fn current_key(s: &NotificationState) -> Option<&str> {
    s.current().map(|n| n.key.as_str())
}

#[test]
fn test_add_shows_current() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::High));
    assert_eq!(current_key(&s), Some("a"));
}

#[test]
fn test_immediate_preempts_requeues() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::High));
    s.add(txt("b", Priority::Immediate));
    assert_eq!(current_key(&s), Some("b"));
    assert_eq!(s.queue.len(), 1);
    assert_eq!(s.queue[0].key, "a");
}

#[test]
fn test_immediate_displaces_immediate() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::Immediate));
    s.add(txt("b", Priority::Immediate));
    assert_eq!(current_key(&s), Some("b"));
    assert!(s.queue.is_empty());
}

#[test]
fn test_dedup_live_key_noop() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::High));
    s.add(txt("a", Priority::High));
    assert_eq!(current_key(&s), Some("a"));
    assert!(s.queue.is_empty());
}

#[test]
fn test_dedup_queued_noop() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::High));
    s.add(txt("b", Priority::High));
    s.add(txt("b", Priority::High));
    assert_eq!(s.queue.len(), 1);
}

#[test]
fn test_remove_live_promotes() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::High));
    s.add(txt("b", Priority::High));
    s.remove("a");
    assert_eq!(current_key(&s), Some("b"));
}

#[test]
fn test_remove_queued_drops() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::High));
    s.add(txt("b", Priority::High));
    s.add(txt("c", Priority::High));
    s.remove("b");
    assert_eq!(s.queue.len(), 1);
    assert_eq!(s.queue[0].key, "c");
}

#[test]
fn test_advance_picks_priority() {
    let mut s = NotificationState::default();
    s.add(txt("low", Priority::Low));
    s.add(txt("high", Priority::High));
    s.add(txt("med", Priority::Medium));
    s.remove("low");
    assert_eq!(current_key(&s), Some("high"));
}

#[test]
fn test_tick_expires_promotes() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::High));
    s.add(txt("b", Priority::High));
    let later = Instant::now() + Duration::from_millis(100);
    s.tick(later);
    assert_eq!(current_key(&s), Some("b"));
}

#[test]
fn test_tick_keeps_before_expiry() {
    let mut s = NotificationState::default();
    s.add(txt("a", Priority::High));
    s.tick(Instant::now());
    assert_eq!(current_key(&s), Some("a"));
}

#[test]
fn test_copy_toast_pins_config() {
    let native = copy_toast("hello", "native");
    assert_eq!(native.key, "selection-copied");
    assert_eq!(native.priority, Priority::Immediate);
    assert_eq!(native.timeout, Duration::from_millis(2000));
    match &native.kind {
        NotifKind::Text { text, color } => {
            assert_eq!(text, "copied 5 chars to clipboard");
            assert_eq!(*color, Some(Color::Green));
        }
    }
    let tmux = copy_toast("abc", "tmux-buffer");
    assert_eq!(tmux.timeout, Duration::from_millis(4000));
    match &tmux.kind {
        NotifKind::Text { text, .. } => {
            assert_eq!(
                text,
                "copied 3 chars to tmux buffer · paste with prefix + ]"
            )
        }
    }
    let osc = copy_toast("xy", "osc52");
    assert_eq!(osc.timeout, Duration::from_millis(4000));
    match &osc.kind {
        NotifKind::Text { text, .. } => {
            assert_eq!(
                text,
                "sent 2 chars via OSC 52 · check terminal clipboard settings if paste fails"
            )
        }
    }
}
