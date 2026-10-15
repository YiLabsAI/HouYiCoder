//! The identity of the slot rows a cache holds.
//!
//! The cache reuses its rows while this value is unchanged. It is compared
//! field by field rather than folded into one number: an identity that
//! multiplies and xors its inputs together can land two different inputs on
//! the same value and serve the build before. Every input the slot builder
//! reads is a field here, so a rebuild happens exactly when the rows would
//! differ; the active run's id rides along because the design names it part
//! of the identity, not because the builder reads it.

use houyicoder_protocol::envelope::RequestId;

use crate::state::App;

/// Which transcript the slot rows were built from. Three distinct sources sit
/// behind one render path, and a switch between them need not move the
/// parent transcript revision or the view's presentation revision.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum SourceId {
    /// The session's own transcript.
    Parent,
    /// A teammate view's transcript, keyed by the child it belongs to.
    Child(String),
    /// The frozen search corpus, which is not the live transcript.
    Search,
}

/// The inputs that decide the slot rows: the source and its revisions, the
/// wrap width, the mode flags that change which lines become rows, and the
/// bash ticker whose text is baked into a chip. The run id is carried for the
/// identity's sake; the builder does not read it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct SlotsKey {
    source: SourceId,
    transcript: u64,
    presentation: u32,
    width: u16,
    verbose: bool,
    busy: bool,
    run: Option<RequestId>,
    bash: u64,
}

impl SlotsKey {
    /// The key for one frame at the given pane width.
    pub(crate) fn for_frame(app: &App, width: u16) -> Self {
        Self {
            source: source_of(app),
            transcript: app.sessions.current.transcript.revision(),
            presentation: app.ui.transcript_view.presentation_revision(),
            width,
            verbose: app.ui.verbose,
            busy: app.agent_busy(),
            run: app.sessions.current.run.request_id(),
            bash: app.run_progress().map_or(0, |p| p.tools.revision()),
        }
    }
}

/// Which of the three transcript sources the render path would read.
fn source_of(app: &App) -> SourceId {
    if let Some(view) = app.ui.teammate_view.as_ref() {
        SourceId::Child(view.child_sid.clone())
    } else if app.ui.transcript_view.search_active() {
        SourceId::Search
    } else {
        SourceId::Parent
    }
}
