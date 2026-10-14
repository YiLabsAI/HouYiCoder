//! The crate's public surface: the pending-future helpers and the task
//! spawn path.

use super::*;
use futures::stream::once;

// Pin the Send bound: the whole point of PFut is the Send boxed future
// that lets async trait methods be dyn-safe. A future regression to a
// non-Send alias fails here, not in a downstream trait.
#[test]
fn test_pfut_is_send() {
    fn assert_send<T: Send + ?Sized>(_: &T) {}
    let f: PFut<'static, ()> = Box::pin(async {});
    assert_send(&f);
}

#[test]
fn test_pstream_boxes_stream() {
    let s: PStream<'static, u8> = Box::pin(once(async { 1 }));
    drop(s);
}
