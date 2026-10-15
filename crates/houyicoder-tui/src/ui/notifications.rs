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
mod tests;
