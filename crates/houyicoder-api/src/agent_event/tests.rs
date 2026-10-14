//! Tests for this module.

use super::*;
use std::sync::Mutex;

#[test]
fn test_handlers_isolate_event_domains() {
    let responses = Arc::new(Mutex::new(Vec::new()));
    let notices = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = AgentEventHandlers::default();
    let captured = Arc::clone(&responses);
    handlers.set_response_stream(Arc::new(move |event| {
        captured.lock().expect("response lock").push(event);
    }));
    let captured = Arc::clone(&notices);
    handlers.set_user_notice(Arc::new(move |event| {
        captured.lock().expect("notice lock").push(event);
    }));

    handlers.emit_response_stream(ResponseStreamEvent::AssistantTextDelta {
        text: "answer".into(),
    });

    assert_eq!(responses.lock().expect("response lock").len(), 1);
    assert!(notices.lock().expect("notice lock").is_empty());
}
