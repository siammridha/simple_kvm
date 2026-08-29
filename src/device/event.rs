//! Mirrors the DOM's `EventTarget`/`addEventListener`/`removeEventListener`
//! model (see `docs/capture-redesign-ideas.md`'s API sketch). Each
//! `EventEmitter<T>` instance corresponds to one event kind, keyed by its
//! payload type `T` - Rust doesn't need the DOM's string-keyed
//! multiplexing of many event types through one object, since each event
//! kind already gets its own type, so a mismatch is a compile error
//! instead of a silent no-op.
//!
//! Used by `Device<D>` for its `devicechange` events and by
//! `CaptureCard` for a `CaptureStream`'s `ended` event; `rtc::session`
//! holds the `Subscription`s those hand back. It lives inside `device`
//! rather than at the top level so it isn't a shared utility module
//! outside the dependency graph; `device` re-exports both types for the
//! other two.
//!
//! Two emitters live here. `EventEmitter<T>` is the plain edge-triggered
//! one: a dispatch reaches whoever is subscribed at that instant and
//! nobody else. `StateEmitter<T>` is for the events whose payload
//! describes *state* rather than a momentary edge - it remembers the value
//! it last dispatched and replays it to a listener that subscribes
//! afterwards (see `StateEmitter`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Local alias for a boxed, pinned, `Send` future - lets listener closures
/// return an arbitrary `async` block without depending on the `futures`
/// crate just for this one alias.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct SubscriptionId(u64);

type Listener<T> = Box<dyn Fn(T) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct EventEmitter<T: Clone + Send + 'static> {
    listeners: Mutex<HashMap<SubscriptionId, Listener<T>>>,
    next_id: AtomicU64,
}

impl<T: Clone + Send + 'static> Default for EventEmitter<T> {
    fn default() -> Self {
        Self { listeners: Mutex::new(HashMap::new()), next_id: AtomicU64::new(0) }
    }
}

impl<T: Clone + Send + 'static> EventEmitter<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mirrors `addEventListener(type, callback)`. The returned
    /// `Subscription` must be kept alive for as long as the callback
    /// should keep firing - dropping it deregisters the callback (mirrors
    /// `removeEventListener`, but automatic, see `Subscription`).
    pub fn add_event_listener<F, Fut>(self: &Arc<Self>, callback: F) -> Subscription<T>
    where
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let id = SubscriptionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.listeners.lock().unwrap().insert(id, Box::new(move |value| Box::pin(callback(value))));
        Subscription { emitter: Arc::downgrade(self), id }
    }

    /// Mirrors `dispatchEvent`. Every listener runs on its own
    /// `tokio::spawn`, with no `.join()` - a slow or broken listener can
    /// never stall another listener or this call itself.
    pub fn dispatch(&self, value: T) {
        for callback in self.listeners.lock().unwrap().values() {
            tokio::spawn(callback(value.clone()));
        }
    }
}

/// An `EventEmitter` for an event whose payload describes *state* rather
/// than a momentary edge: it remembers the value it last dispatched and
/// replays that value to a listener that subscribes afterwards, so a
/// subscriber that arrives late still learns where things stand instead of
/// registering a callback that can never fire.
///
/// Both of this codebase's state-shaped events need that, for the same
/// reason (issue #023): the thing that dispatches them starts before the
/// consumer can subscribe.
///
/// - A `CaptureStream`'s `ended`: the stream is handed to its consumer,
///   which then does await-heavy work (adding a WebRTC video track) before
///   subscribing. A pass that fails fast dispatches inside that window.
/// - A `Device`'s `devicechange`: `Device::spawn` starts the presence task
///   before its caller can subscribe, so the first status can be published
///   before anyone is listening.
///
/// The stored value doubles as the lock that makes dispatching and
/// subscribing atomic with respect to each other, so a listener that
/// registers while a dispatch is in flight is notified exactly once -
/// by that dispatch or by the replay, never by both.
pub struct StateEmitter<T: Clone + Send + 'static> {
    emitter: Arc<EventEmitter<T>>,
    last: Mutex<Option<T>>,
}

impl<T: Clone + Send + 'static> Default for StateEmitter<T> {
    fn default() -> Self {
        Self { emitter: Arc::new(EventEmitter::new()), last: Mutex::new(None) }
    }
}

impl<T: Clone + Send + 'static> StateEmitter<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The value last dispatched, or `None` if nothing has been dispatched
    /// yet - the "ask it" half of the state this emitter carries, for a
    /// caller that wants the current value without subscribing.
    pub fn latest(&self) -> Option<T> {
        self.last.lock().unwrap().clone()
    }

    /// Mirrors `dispatchEvent`, and records the value as the current
    /// state on the way through.
    pub fn dispatch(&self, value: T) {
        let mut last = self.last.lock().unwrap();
        *last = Some(value.clone());
        self.emitter.dispatch(value);
    }

    /// Mirrors `addEventListener(type, callback)`, except that a listener
    /// registered after a dispatch is called once with the value that
    /// dispatch carried. The replay runs on its own `tokio::spawn`, the
    /// same fire-and-forget way `EventEmitter::dispatch` runs a listener.
    pub fn add_event_listener<F, Fut>(&self, callback: F) -> Subscription<T>
    where
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // Held across both the registration and the replay: that is what
        // stops a concurrent `dispatch` from reaching this listener as
        // well as the replay does.
        let last = self.last.lock().unwrap();
        let callback = Arc::new(callback);
        let subscription = self.emitter.add_event_listener({
            let callback = Arc::clone(&callback);
            move |value| {
                let callback = Arc::clone(&callback);
                async move { callback(value).await }
            }
        });
        if let Some(value) = last.clone() {
            tokio::spawn(async move { callback(value).await });
        }
        subscription
    }
}

/// Mirrors `removeEventListener`, but automatic: dropping the subscription
/// removes its listener from the emitter, so nothing has to remember to
/// make an explicit unsubscribe call. Holds only a `Weak` reference so an
/// outstanding `Subscription` never keeps its `EventEmitter` alive.
///
/// Needs the same `T: Clone + Send + 'static` bound as `EventEmitter<T>`
/// itself, not just there - this struct names `Weak<EventEmitter<T>>`
/// directly, so the bound is required the moment `Subscription` is
/// declared, and again on its `Drop` impl.
pub struct Subscription<T: Clone + Send + 'static> {
    emitter: Weak<EventEmitter<T>>,
    id: SubscriptionId,
}

impl<T: Clone + Send + 'static> Drop for Subscription<T> {
    fn drop(&mut self) {
        if let Some(emitter) = self.emitter.upgrade() {
            emitter.listeners.lock().unwrap().remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn listener_fires_on_dispatch() {
        let emitter = Arc::new(EventEmitter::<u32>::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = emitter.add_event_listener(move |value| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(value);
            }
        });

        emitter.dispatch(42);

        let received = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.expect("listener should have fired").expect("channel should still be open");
        assert_eq!(received, 42);
    }

    #[tokio::test]
    async fn dropping_subscription_stops_future_dispatches() {
        let emitter = Arc::new(EventEmitter::<u32>::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sub = emitter.add_event_listener(move |value| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(value);
            }
        });

        drop(sub);
        emitter.dispatch(1);

        // Give a wrongly-still-registered listener a chance to run before
        // asserting nothing arrived.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "dropped subscription's listener must not fire");
    }

    #[tokio::test]
    async fn a_slow_listener_does_not_block_another_listener() {
        let emitter = Arc::new(EventEmitter::<u32>::new());
        let (fast_tx, mut fast_rx) = mpsc::unbounded_channel();
        let (slow_tx, mut slow_rx) = mpsc::unbounded_channel();

        let _slow_sub = emitter.add_event_listener(move |value| {
            let slow_tx = slow_tx.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let _ = slow_tx.send(value);
            }
        });
        let _fast_sub = emitter.add_event_listener(move |value| {
            let fast_tx = fast_tx.clone();
            async move {
                let _ = fast_tx.send(value);
            }
        });

        emitter.dispatch(7);

        let fast = tokio::time::timeout(Duration::from_millis(500), fast_rx.recv()).await.expect("fast listener must not be blocked by the slow one").expect("channel should still be open");
        assert_eq!(fast, 7);
        assert!(slow_rx.try_recv().is_err(), "slow listener shouldn't have fired yet");
    }

    // --- `StateEmitter` - the late-subscriber case `EventEmitter` alone
    // can't serve (issue #023) ---

    #[tokio::test]
    async fn state_emitter_replays_the_last_value_to_a_late_subscriber() {
        let emitter = StateEmitter::<u32>::new();
        emitter.dispatch(5);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = emitter.add_event_listener(move |value| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(value);
            }
        });

        let received = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.expect("a listener subscribing after the dispatch must still be told").expect("channel should still be open");
        assert_eq!(received, 5);
    }

    #[tokio::test]
    async fn state_emitter_does_not_replay_before_anything_was_dispatched() {
        let emitter = StateEmitter::<u32>::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = emitter.add_event_listener(move |value| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(value);
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "nothing has been dispatched, so there is nothing to replay");
    }

    #[tokio::test]
    async fn state_emitter_notifies_a_listener_that_subscribed_in_time_exactly_once() {
        let emitter = StateEmitter::<u32>::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _sub = emitter.add_event_listener(move |value| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(value);
            }
        });

        emitter.dispatch(1);

        let received = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.expect("listener should have fired").expect("channel should still be open");
        assert_eq!(received, 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "a listener present at dispatch time must not also get the replay");
    }

    #[tokio::test]
    async fn state_emitter_stops_replaying_to_a_dropped_subscription() {
        let emitter = StateEmitter::<u32>::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sub = emitter.add_event_listener(move |value| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(value);
            }
        });

        drop(sub);
        emitter.dispatch(3);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "dropped subscription's listener must not fire");
    }

    #[tokio::test]
    async fn state_emitter_latest_reports_the_last_dispatched_value() {
        let emitter = StateEmitter::<u32>::new();
        assert_eq!(emitter.latest(), None);
        emitter.dispatch(1);
        emitter.dispatch(2);
        assert_eq!(emitter.latest(), Some(2));
    }
}
