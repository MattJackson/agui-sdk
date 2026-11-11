//! Cooperative cancellation shared by the client and the server.
//!
//! One [`CancelToken`] type serves both roles: a client run
//! ([`RunHandle::cancel_token`][crate::client::RunHandle::cancel_token],
//! [`RunRequest::cancel_token`][crate::client::RunRequest::cancel_token],
//! [`TransportRequest::cancel`][crate::client::transport::TransportRequest::cancel])
//! and a server run ([`Runner::run`][crate::server::Runner::run], client
//! disconnect in the framework glue). Cancelling wakes every task parked on
//! [`CancelToken::cancelled`], so a transport blocked on network I/O can
//! `select!` on it and close the connection at once, instead of noticing at
//! the next byte.
//!
//! Std only: an atomic flag plus a mutex-guarded waker list; no runtime.
//! A client run that is cancelled ends with `run.cancelled`; a `RUN_FINISHED`
//! is never synthesized (LIFE-20).
//!
//! Each pending [`Cancelled`] future holds one slot in the token's waker
//! table; re-polling with the same waker reuses it and dropping the future
//! frees it, so a loop that races `cancelled()` against every read keeps the
//! table at the number of live futures.
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::RunHandle::cancel_token]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.RunHandle.html#method.cancel_token"
)]
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::RunRequest::cancel_token]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.RunRequest.html#method.cancel_token"
)]
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::transport::TransportRequest::cancel]: https://docs.rs/agui-sdk/latest/agui_sdk/client/transport/struct.TransportRequest.html#structfield.cancel"
)]
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[crate::server::Runner::run]: https://docs.rs/agui-sdk/latest/agui_sdk/server/struct.Runner.html#method.run"
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

/// A cloneable cancellation flag with waker support. Clones share state.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    wakers: Mutex<WakerTable>,
}

/// Wakers of pending [`Cancelled`] futures, by slot.
#[derive(Default)]
struct WakerTable {
    slots: Vec<Option<Waker>>,
    free: Vec<usize>,
}

impl CancelInner {
    /// The waker table. A panic while it was held cannot leave it
    /// inconsistent (every update is a single assignment), so a poisoned lock
    /// is recovered.
    fn table(&self) -> MutexGuard<'_, WakerTable> {
        self.wakers.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CancelToken {
    /// A fresh, uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        CancelToken::default()
    }

    /// Cancels and wakes every waiter. Idempotent.
    ///
    /// Each registered waker is woken exactly once, outside the lock; later
    /// calls do nothing.
    pub fn cancel(&self) {
        if self.inner.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let slots = {
            let mut table = self.inner.table();
            table.free = Vec::new();
            std::mem::take(&mut table.slots)
        };
        for waker in slots.into_iter().flatten() {
            waker.wake();
        }
    }

    /// Whether cancellation was requested. One atomic load.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// A future that resolves once the token is cancelled (immediately if it
    /// already is). Cancel-safe; dropping it frees its waker slot.
    pub fn cancelled(&self) -> Cancelled {
        Cancelled {
            token: self.clone(),
            slot: None,
        }
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Future from [`CancelToken::cancelled`].
#[derive(Debug)]
#[must_use = "futures do nothing unless awaited"]
pub struct Cancelled {
    token: CancelToken,
    /// This future's slot in the token's waker table, once registered.
    slot: Option<usize>,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.token.is_cancelled() {
            return Poll::Ready(());
        }
        let inner = &this.token.inner;
        let mut table = inner.table();
        // `cancel` sets the flag before it takes the table, so a flag still
        // clear under the lock means the waker stored below will be woken.
        if inner.cancelled.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        match this.slot.and_then(|i| table.slots.get_mut(i)) {
            Some(Some(waker)) => {
                if !waker.will_wake(cx.waker()) {
                    waker.clone_from(cx.waker());
                }
            }
            Some(empty) => *empty = Some(cx.waker().clone()),
            None => {
                let waker = Some(cx.waker().clone());
                let index = match table.free.pop() {
                    Some(index) => {
                        table.slots[index] = waker;
                        index
                    }
                    None => {
                        table.slots.push(waker);
                        table.slots.len() - 1
                    }
                };
                this.slot = Some(index);
            }
        }
        Poll::Pending
    }
}

impl Drop for Cancelled {
    fn drop(&mut self) {
        let Some(index) = self.slot else {
            return;
        };
        let inner = &self.token.inner;
        if inner.cancelled.load(Ordering::Acquire) {
            return;
        }
        let mut table = inner.table();
        if let Some(slot) = table.slots.get_mut(index) {
            if slot.take().is_some() {
                table.free.push(index);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    const fn assert_send_sync<T: Send + Sync + Unpin + 'static>() {}
    const _: () = {
        assert_send_sync::<CancelToken>();
        assert_send_sync::<Cancelled>();
    };

    #[derive(Default)]
    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll(future: &mut Cancelled, waker: &Waker) -> Poll<()> {
        Pin::new(future).poll(&mut Context::from_waker(waker))
    }

    fn slots(token: &CancelToken) -> usize {
        token.inner.table().slots.len()
    }

    #[test]
    fn cancel_wakes_every_waiter_exactly_once() {
        let token = CancelToken::new();
        let counters: Vec<Arc<CountingWaker>> = (0..3).map(|_| Arc::default()).collect();
        let mut futures: Vec<Cancelled> = (0..3).map(|_| token.clone().cancelled()).collect();
        for (future, counter) in futures.iter_mut().zip(&counters) {
            let waker = Waker::from(Arc::clone(counter));
            assert_eq!(poll(future, &waker), Poll::Pending);
            // Re-polling with the same waker does not register it twice.
            assert_eq!(poll(future, &waker), Poll::Pending);
        }
        assert!(!token.is_cancelled());
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
        for counter in &counters {
            assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        }
        let waker = Waker::from(Arc::clone(&counters[0]));
        for future in &mut futures {
            assert_eq!(poll(future, &waker), Poll::Ready(()));
        }
    }

    #[test]
    fn a_new_waker_replaces_the_old_one() {
        let token = CancelToken::new();
        let first = Arc::new(CountingWaker::default());
        let second = Arc::new(CountingWaker::default());
        let mut future = token.cancelled();
        let _ = poll(&mut future, &Waker::from(Arc::clone(&first)));
        let _ = poll(&mut future, &Waker::from(Arc::clone(&second)));
        assert_eq!(slots(&token), 1);
        token.cancel();
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(second.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn already_cancelled_resolves_without_registering() {
        let token = CancelToken::new();
        token.cancel();
        let mut future = token.cancelled();
        let waker = Waker::from(Arc::new(CountingWaker::default()));
        assert_eq!(poll(&mut future, &waker), Poll::Ready(()));
        assert_eq!(slots(&token), 0);
        assert_eq!(format!("{token:?}"), "CancelToken { cancelled: true }");
    }

    #[test]
    fn dropped_futures_free_their_slots() {
        let token = CancelToken::new();
        let waker = Waker::from(Arc::new(CountingWaker::default()));
        let mut kept = token.cancelled();
        let _ = poll(&mut kept, &waker);
        for _ in 0..1000 {
            let mut racing = token.cancelled();
            let _ = poll(&mut racing, &waker);
        }
        assert_eq!(
            slots(&token),
            2,
            "the table must not grow with dropped futures"
        );
    }

    #[test]
    fn cancel_from_another_thread_wakes_a_parked_waiter() {
        let token = CancelToken::new();
        let counter = Arc::new(CountingWaker::default());
        let mut future = token.cancelled();
        let _ = poll(&mut future, &Waker::from(Arc::clone(&counter)));
        let remote = token.clone();
        let joined = std::thread::spawn(move || remote.cancel()).join();
        assert!(joined.is_ok());
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert!(token.is_cancelled());
    }
}
