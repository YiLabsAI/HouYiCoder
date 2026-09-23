//! State and transitions for the /skills pane.
//!
//! The owner keeps the list cursor, the detail body request, and bounded
//! detail scrolling consistent across the key, message, and view
//! boundaries. The list groups entries by discovery origin; the detail
//! opens one skill and holds the body text the model would read on
//! invocation.

use std::cell::Cell;

use houyicoder_protocol::envelope::RequestId;

/// The open skill detail. Loading holds the request whose reply fills the
/// body; Open holds the body that reply carried (None when the skill could
/// not be resolved) together with the scroll position the view clamps.
pub(crate) enum SkillDetail {
    Loading {
        request_id: RequestId,
        name: String,
    },
    Open {
        name: String,
        body: Option<String>,
        offset: Cell<u16>,
        max_offset: Cell<u16>,
    },
}

/// List cursor plus the open detail for the /skills pane.
#[derive(Default)]
pub(crate) struct SkillsPaneState {
    cursor: Cell<usize>,
    detail: Option<SkillDetail>,
}

impl SkillsPaneState {
    pub(crate) fn cursor(&self) -> usize {
        self.cursor.get()
    }

    /// Move the cursor by a signed delta, clamped to the list bounds. A no-op
    /// on an empty list (no row to point at).
    pub(crate) fn move_cursor(&self, delta: i32, len: usize) {
        if len == 0 {
            self.cursor.set(0);
            return;
        }
        let current = self.cursor.get().min(len - 1) as i32;
        self.cursor
            .set((current + delta).clamp(0, (len - 1) as i32) as usize);
    }

    /// Settle the loading detail with the body unavailable when the request it
    /// waits on is this one. A later request has replaced it; a different id
    /// changes nothing.
    pub(crate) fn fail_detail(&mut self, request_id: RequestId) {
        let Some(SkillDetail::Loading {
            request_id: pending,
            name,
        }) = self.detail.as_ref()
        else {
            return;
        };
        if *pending != request_id {
            return;
        }
        let name = name.clone();
        self.open(name, None);
    }

    /// Settle every body request the connection can no longer answer: no
    /// reply is coming, so the detail keeps its header and shows the body
    /// unavailable rather than loading forever. An open detail is settled
    /// state and survives.
    pub(crate) fn clear_pending(&mut self) {
        let Some(SkillDetail::Loading { name, .. }) = self.detail.as_ref() else {
            return;
        };
        let name = name.clone();
        self.open(name, None);
    }

    pub(crate) fn detail(&self) -> Option<&SkillDetail> {
        self.detail.as_ref()
    }

    /// Open the detail for one skill and mark its body as requested. The
    /// reply is matched by request id, so a reply for an earlier open does
    /// not fill the current one.
    pub(crate) fn request_detail(&mut self, request_id: RequestId, name: String) {
        self.detail = Some(SkillDetail::Loading { request_id, name });
    }

    /// Fill the loading detail with the body the reply carried. Returns false
    /// when the reply belongs to a different request or no detail is loading.
    pub(crate) fn apply_detail(&mut self, request_id: RequestId, body: Option<String>) -> bool {
        let Some(SkillDetail::Loading {
            request_id: pending,
            name,
        }) = self.detail.as_ref()
        else {
            return false;
        };
        if *pending != request_id {
            return false;
        }
        self.open(name.clone(), body);
        true
    }

    /// Open the detail with no body, for a skill whose body cannot be
    /// requested (no connection) or resolved (no longer discoverable).
    pub(crate) fn open_unavailable(&mut self, name: String) {
        self.open(name, None);
    }

    fn open(&mut self, name: String, body: Option<String>) {
        self.detail = Some(SkillDetail::Open {
            name,
            body,
            offset: Cell::new(0),
            max_offset: Cell::new(0),
        });
    }

    pub(crate) fn close_detail(&mut self) {
        self.detail = None;
    }

    /// Record the largest scroll offset the last render produced, clamping
    /// the current offset so a shorter body cannot leave the view past its
    /// end.
    pub(crate) fn set_detail_max_offset(&self, max_offset: u16) {
        if let Some(SkillDetail::Open {
            offset,
            max_offset: limit,
            ..
        }) = self.detail.as_ref()
        {
            limit.set(max_offset);
            offset.set(offset.get().min(max_offset));
        }
    }

    pub(crate) fn scroll_detail(&self, delta: i16) {
        let Some(SkillDetail::Open {
            offset, max_offset, ..
        }) = self.detail.as_ref()
        else {
            return;
        };
        let next = if delta.is_negative() {
            offset.get().saturating_sub(delta.unsigned_abs())
        } else {
            offset.get().saturating_add(delta.unsigned_abs())
        };
        offset.set(next.min(max_offset.get()));
    }

    #[cfg(test)]
    pub(crate) fn detail_body(&self) -> Option<&str> {
        match self.detail.as_ref() {
            Some(SkillDetail::Open { body, .. }) => body.as_deref(),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn detail_offset(&self) -> Option<u16> {
        match self.detail.as_ref() {
            Some(SkillDetail::Open { offset, .. }) => Some(offset.get()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_id(value: u64) -> RequestId {
        RequestId(value)
    }

    #[test]
    fn test_apply_detail_matches_request() {
        let mut state = SkillsPaneState::default();
        state.request_detail(request_id(7), "alpha".into());
        assert!(!state.apply_detail(request_id(8), Some("body".into())));
        assert!(state.apply_detail(request_id(7), Some("body".into())));
        assert_eq!(state.detail_body(), Some("body"));
    }

    #[test]
    fn test_apply_detail_without_request() {
        let mut state = SkillsPaneState::default();
        assert!(!state.apply_detail(request_id(1), Some("body".into())));
        assert!(state.detail().is_none());
    }

    #[test]
    fn test_scroll_clamps_to_body() {
        let mut state = SkillsPaneState::default();
        state.request_detail(request_id(1), "alpha".into());
        assert!(state.apply_detail(request_id(1), Some("body".into())));
        state.set_detail_max_offset(3);
        state.scroll_detail(10);
        assert_eq!(state.detail_offset(), Some(3), "Down stops at the last row");
        state.scroll_detail(-10);
        assert_eq!(state.detail_offset(), Some(0), "Up stops at the first row");
    }

    #[test]
    fn test_shorter_body_resets_offset() {
        let mut state = SkillsPaneState::default();
        state.request_detail(request_id(1), "alpha".into());
        assert!(state.apply_detail(request_id(1), Some("body".into())));
        state.set_detail_max_offset(9);
        state.scroll_detail(9);
        assert_eq!(state.detail_offset(), Some(9));
        state.set_detail_max_offset(2);
        assert_eq!(
            state.detail_offset(),
            Some(2),
            "a shorter body pulls the view back"
        );
    }

    #[test]
    fn test_cursor_moves_and_saturates() {
        let state = SkillsPaneState::default();
        state.move_cursor(-1, 5);
        assert_eq!(state.cursor(), 0, "Up saturates at the first row");
        state.move_cursor(2, 5);
        assert_eq!(state.cursor(), 2);
        state.move_cursor(10, 5);
        assert_eq!(state.cursor(), 4, "Down stops at the last row");
        state.move_cursor(3, 0);
        assert_eq!(state.cursor(), 0, "an empty list has no row to point at");
    }

    /// A failed send settles the loading detail with the body unavailable,
    /// so the pane stops promising a reply that will never arrive.
    #[test]
    fn test_failed_send_settles_detail() {
        let mut state = SkillsPaneState::default();
        state.request_detail(request_id(4), "alpha".into());
        state.fail_detail(request_id(4));
        assert!(state.detail_body().is_none(), "the body stays unavailable");
        match state.detail() {
            Some(SkillDetail::Open { name, .. }) => assert_eq!(name, "alpha"),
            _ => panic!("expected an open detail"),
        }
    }

    /// A reply for a superseded request must not settle the current detail:
    /// the later request is still pending.
    #[test]
    fn test_stale_failure_keeps_loading() {
        let mut state = SkillsPaneState::default();
        state.request_detail(request_id(5), "beta".into());
        state.fail_detail(request_id(4));
        assert!(
            matches!(state.detail(), Some(SkillDetail::Loading { .. })),
            "the current request keeps loading"
        );
    }

    /// A connection loss settles a loading detail but leaves an open one
    /// alone: open is settled state, and the header stays useful.
    #[test]
    fn test_loss_settles_loading_only() {
        let mut state = SkillsPaneState::default();
        state.request_detail(request_id(1), "alpha".into());
        state.clear_pending();
        assert!(
            !matches!(state.detail(), Some(SkillDetail::Loading { .. })),
            "no reply is coming after the loss"
        );
        assert!(state.detail_body().is_none(), "the body stays unavailable");
        state.clear_pending();
        assert!(state.detail().is_some(), "an open detail survives");
    }
}
