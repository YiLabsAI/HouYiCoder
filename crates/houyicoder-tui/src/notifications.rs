//! Transient notification toast: a one-line auto-expiring hint shown above
//! the input box. One at a time (no stacking); an immediate-priority entry
//! preempts the current and skips the queue; lower-priority entries queue
//! behind it, deduped by key. Expiry is poll-driven: each entry carries an
//! Instant deadline the run loop checks, so no timer handle is held.

use std::time::{Duration, Instant};

use ratatui::style::Color;

/// Default on-screen lifetime when a notification does not name its own.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(8000);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Priority {
    Low,
    Medium,
    High,
    /// Show now, preempt the current entry, skip the queue.
    Immediate,
}
impl Priority {
    fn rank(self) -> u8 {
        match self {
            Priority::Immediate => 0,
            Priority::High => 1,
            Priority::Medium => 2,
            Priority::Low => 3,
        }
    }
}

pub enum NotifKind {
    Text { text: String, color: Option<Color> },
}
impl NotifKind {
    pub fn text(text: impl Into<String>) -> Self {
        NotifKind::Text {
            text: text.into(),
            color: None,
        }
    }
}

pub struct Notification {
    pub key: String,
    pub priority: Priority,
    pub timeout: Duration,
    pub kind: NotifKind,
}
impl Notification {
    pub fn immediate(key: impl Into<String>, kind: NotifKind, timeout: Duration) -> Self {
        Notification {
            key: key.into(),
            priority: Priority::Immediate,
            timeout,
            kind,
        }
    }
}

/// The copy-confirmation toast: "copied N chars to <path>". Timeout is short
/// for native (reliable), longer for tmux/osc52 (paste may need a nudge).
/// Green = success.
pub fn copy_toast(text: &str, path: &str) -> Notification {
    let n = text.chars().count();
    let (msg, timeout) = match path {
        "tmux-buffer" => (
            format!("copied {n} chars to tmux buffer · paste with prefix + ]"),
            Duration::from_millis(4000),
        ),
        "osc52" => (
            format!("sent {n} chars via OSC 52 · check terminal clipboard settings if paste fails"),
            Duration::from_millis(4000),
        ),
        _ => (
            format!("copied {n} chars to clipboard"),
            Duration::from_millis(2000),
        ),
    };
    Notification::immediate(
        "selection-copied",
        NotifKind::Text {
            text: msg,
            color: Some(Color::Green),
        },
        timeout,
    )
}

struct Current {
    notif: Notification,
    expires_at: Instant,
}

#[derive(Default)]
pub struct NotificationState {
    current: Option<Current>,
    queue: Vec<Notification>,
}

impl NotificationState {
    pub fn add(&mut self, n: Notification) {
        if n.priority == Priority::Immediate {
            // Preempt the current entry; a non-immediate displaced entry
            // returns to the queue, an immediate one is dropped (two
            // immediate hints would crowd out each other's confirmation window).
            if let Some(c) = self.current.take()
                && c.notif.priority != Priority::Immediate
            {
                self.queue.push(c.notif);
            }
            self.set_current(n);
        } else {
            // Dedup by key: a repeated key is a no-op while one is live or
            // queued. (fold/merge is a follow-up, not v1.)
            if self.current.as_ref().is_some_and(|c| c.notif.key == n.key)
                || self.queue.iter().any(|q| q.key == n.key)
            {
                return;
            }
            self.queue.push(n);
            self.advance();
        }
    }

    pub fn remove(&mut self, key: &str) {
        if self.current.as_ref().is_some_and(|c| c.notif.key == key) {
            self.current = None;
            self.advance();
        } else {
            self.queue.retain(|q| q.key != key);
        }
    }

    /// Expire the current entry when its window elapsed, then promote the
    /// next queued entry. Called once per run-loop poll. Returns true when
    /// the visible entry changed (so the caller marks the view dirty).
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.current.as_ref().is_some_and(|c| now >= c.expires_at) {
            self.current = None;
            self.advance();
            return true;
        }
        false
    }

    pub fn current(&self) -> Option<&Notification> {
        self.current.as_ref().map(|c| &c.notif)
    }

    fn set_current(&mut self, n: Notification) {
        let timeout = if n.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            n.timeout
        };
        self.current = Some(Current {
            notif: n,
            expires_at: Instant::now() + timeout,
        });
    }

    fn advance(&mut self) {
        if self.current.is_some() || self.queue.is_empty() {
            return;
        }
        // Pick the highest-priority queued entry; ties keep the first seen.
        let mut idx = 0;
        for i in 1..self.queue.len() {
            if self.queue[i].priority.rank() < self.queue[idx].priority.rank() {
                idx = i;
            }
        }
        let n = self.queue.remove(idx);
        self.set_current(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use ratatui::style::Color;
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
}
