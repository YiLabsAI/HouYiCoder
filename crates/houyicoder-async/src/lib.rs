//! Async foundation aliases for dyn-safe trait methods.
//!
//! PFut is the pinned boxed Send future returned by dyn-safe async trait
//! methods across the engine (provider, context, memory, proposer, tool).
//! An impl Future return in a trait is not dyn-safe, so every async trait
//! shares this one alias as the standard workaround. PStream is the stream
//! equivalent for streaming trait methods.
//!
//! A foundation leaf: no internal dependencies. Every layer that declares
//! an async interface depends on this crate the way it depends on std.

use std::future::Future;
use std::pin::Pin;

/// A pinned, boxed, Send future: the return type of dyn-safe async trait
/// methods. Pin<Box<dyn Future + Send>> is required because an impl Future
/// return in a trait is not dyn-safe; this alias is the standard workaround.
pub type PFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A pinned, boxed, Send async stream: the return type of dyn-safe streaming
/// trait methods (e.g. a model provider event stream). Parallel to PFut — the
/// stream equivalent of the boxed-future workaround for object-safe async
/// traits.
pub type PStream<'a, T> = Pin<Box<dyn futures::Stream<Item = T> + Send + 'a>>;

/// A cooperative cancellation token shared between the agent loop and a tool
/// it dispatches. Re-exported from tokio-util so the port trait signature
/// references a concrete, ergonomic type: a tool can await the token
/// cancelled() future directly in a select branch, without the ports crate
/// depending on tokio-util; downstream crates reach the type through this
/// foundation leaf.
pub use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod tests {
    use super::*;

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
        let s: PStream<'static, u8> = Box::pin(futures::stream::once(async { 1 }));
        drop(s);
    }
}
