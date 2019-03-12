//! This crate provides an asynchronous, atomic `Option` type.
//!
//! At a high level, this crate is exactly like `Arc<Mutex<Option<T>>>`, except with support for
//! asynchronous operations. Given an [`Aption<T>`], you can call [`poll_put`] to attempt to place
//! a value into the `Option`, or `poll_take` to take a value out of the `Option`. Both methods
//! will return `Async::NotReady` if the `Option` is occupied or empty respectively, and will at
//! that point have scheduled for the current task to be notified when the `poll_*` call may
//! succeed in the future. `Aption<T>` can also be used as a `Sink<SinkItem = T>` and `Stream<Item
//! = T>` by effectively operating as a single-element channel.
//!
//! An `Aption<T>` can also be closed using [`poll_close`]. Any `poll_put` after a `poll_close`
//! will fail, and the next `poll_take` will return the current value (if any), and from then on
//! `poll_take` will return an error.
//!
//!   [`Aption<T>`]: struct.Aption.html
//!   [`poll_put`]: struct.Aption.html#method.poll_put
//!   [`poll_take`]: struct.Aption.html#method.poll_take
//!   [`poll_close`]: struct.Aption.html#method.poll_close

#![deny(
    unused_extern_crates,
    missing_debug_implementations,
    missing_docs,
    unreachable_pub
)]
#![cfg_attr(test, deny(warnings))]

use futures::{task, try_ready, Async, AsyncSink, Poll, Sink, StartSend, Stream};
use std::cell::UnsafeCell;
use std::sync::Arc;
use std::{fmt, mem};
use tokio_sync::semaphore;

/// Indicates that an [`Aption`] has been closed, and no further operations are available on it.
///
///   [`Aption`]: struct.Aption.html
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct Closed;

/// An asynchronous, atomic `Option` type.
///
/// See the [crate-level documentation] for details.
///
///   [crate-level documentation]: ../
#[derive(Debug)]
pub struct Aption<T> {
    inner: Arc<Inner<T>>,
    permit: semaphore::Permit,
}

impl<T> Clone for Aption<T> {
    fn clone(&self) -> Self {
        Aption {
            inner: self.inner.clone(),
            permit: semaphore::Permit::new(),
        }
    }
}

#[allow(missing_docs)]
pub fn new<T>() -> Aption<T> {
    let m = Arc::new(Inner {
        semaphore: semaphore::Semaphore::new(1),
        value: UnsafeCell::new(CellValue::None),
        put_task: task::AtomicTask::new(),
        take_task: task::AtomicTask::new(),
    });

    Aption {
        inner: m.clone(),
        permit: semaphore::Permit::new(),
    }
}

enum CellValue<T> {
    Some(T),
    None,
    Fin(Option<T>),
}

impl<T> CellValue<T> {
    fn is_none(&self) -> bool {
        if let CellValue::None = *self {
            true
        } else {
            false
        }
    }

    fn take(&mut self) -> Option<T> {
        match mem::replace(self, CellValue::None) {
            CellValue::None => None,
            CellValue::Some(t) => Some(t),
            CellValue::Fin(f) => {
                // retore Fin bit
                mem::replace(self, CellValue::Fin(None));
                f
            }
        }
    }
}

struct Inner<T> {
    semaphore: semaphore::Semaphore,
    value: UnsafeCell<CellValue<T>>,
    put_task: task::AtomicTask,
    take_task: task::AtomicTask,
}

// we never expose &T, only ever &mut T, so we only require T: Send
unsafe impl<T: Send> Sync for Inner<T> {}
unsafe impl<T: Send> Send for Inner<T> {}

impl<T> fmt::Debug for Inner<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "AptionInner")
    }
}

impl<T> Aption<T> {
    /// Attempt to take the value contained in the `Aption`.
    ///
    /// Returns `NotReady` if no value is available, and schedules the current task to be woken up
    /// when one might be.
    ///
    /// Returns an error if the `Aption` has been closed with `Aption::poll_close`.
    pub fn poll_take(&mut self) -> Poll<T, Closed> {
        try_ready!(self
            .permit
            .poll_acquire(&self.inner.semaphore)
            .map_err(|_| unreachable!("semaphore dropped while we have an Arc to it")));

        // we have the lock -- is there a value?
        let value = unsafe { &mut *self.inner.value.get() };

        let v = value.take();
        if v.is_none() {
            // no, sadly not...
            // has it been closed altogether?
            if let CellValue::Fin(None) = *value {
                // it has! nothing more to do except release the permit
                // don't even have to wake anyone up, since we didn't take anything
                self.permit.release(&self.inner.semaphore);
                return Err(Closed);
            }

            // we're going to have to wait for someone to put a value.
            // we need to do this _before_ releasing the lock,
            // otherwise we might miss a quick notify.
            self.inner.take_task.register();
        }

        // give up the lock for someone to put
        self.permit.release(&self.inner.semaphore);

        if let Some(t) = v {
            // let waiting putters know that they can now put
            self.inner.put_task.notify();
            Ok(Async::Ready(t))
        } else {
            Ok(Async::NotReady)
        }
    }

    /// Attempt to put a value into the `Aption`.
    ///
    /// Returns `NotReady` if there's already a value there, and schedules the current task to be
    /// woken up when the `Aption` is free again.
    ///
    /// Returns an error if the `Aption` has been closed with `Aption::poll_close`.
    pub fn poll_put(&mut self, t: T) -> Result<AsyncSink<T>, T> {
        match self.permit.poll_acquire(&self.inner.semaphore) {
            Ok(Async::Ready(())) => {}
            Ok(Async::NotReady) => {
                return Ok(AsyncSink::NotReady(t));
            }
            Err(_) => {
                unreachable!("semaphore dropped while we have an Arc to it");
            }
        }

        // we have the lock!
        let value = unsafe { &mut *self.inner.value.get() };

        // has the channel already been closed?
        if let CellValue::Fin(_) = *value {
            // it has, so we're not going to get to send our value
            // we do have to release the lock though
            self.permit.release(&self.inner.semaphore);
            return Err(t);
        }

        // is there already a value there?
        if value.is_none() {
            // no, we're home free!
            *value = CellValue::Some(t);

            // give up the lock so someone can take
            self.permit.release(&self.inner.semaphore);

            // and notify any waiting takers
            self.inner.take_task.notify();

            Ok(AsyncSink::Ready)
        } else {
            // yes, sadly, so we can't put...

            // we're going to have to wait for someone to take the existing value.
            // we need to do this _before_ releasing the lock,
            // otherwise we might miss a quick notify.
            self.inner.put_task.register();

            // give up the lock so someone can take
            self.permit.release(&self.inner.semaphore);

            Ok(AsyncSink::NotReady(t))
        }
    }

    /// Indicate the `Aption` as closed so that no future puts are permitted.
    ///
    /// Once this method succeeds, every subsequent call to `Aption::poll_put` will return an
    /// error. If there is currently a value in the `Aption`, the next call to `Aption::poll_take`
    /// will return that value. Any later calls to `Aption::poll_take` will return an error.
    pub fn poll_close(&mut self) -> Poll<(), ()> {
        try_ready!(self
            .permit
            .poll_acquire(&self.inner.semaphore)
            .map_err(|_| unreachable!("semaphore dropped while we have an Arc to it")));

        // we have the lock -- wrap whatever value is there in Fin
        let value = unsafe { &mut *self.inner.value.get() };
        let v = value.take();
        *value = CellValue::Fin(v);

        // if the value is None, we've closed successfully!
        let ret = if let CellValue::Fin(None) = *value {
            Async::Ready(())
        } else {
            // otherwise, we'll have to wait for someone to take
            // and again, *before* we release the lock
            self.inner.put_task.register();
            Async::NotReady
        };

        // give up the lock so someone can take
        self.permit.release(&self.inner.semaphore);

        // and notify any waiting takers
        self.inner.take_task.notify();

        Ok(ret)
    }
}

impl<T> Sink for Aption<T> {
    type SinkItem = T;
    type SinkError = T;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.poll_put(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.poll_close()
            .map_err(|_| unreachable!("failed to close because already closed elsewhere"))
    }
}

impl<T> Stream for Aption<T> {
    type Item = T;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.poll_take() {
            Ok(Async::Ready(v)) => Ok(Async::Ready(Some(v))),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(Closed) => {
                // error on take just means it's been closed
                Ok(Async::Ready(None))
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use tokio_mock_task::MockTask;

    #[test]
    fn basic() {
        let mut mt = MockTask::new();

        let mut a = new::<usize>();
        assert_eq!(mt.enter(|| a.poll_take()), Ok(Async::NotReady));
        assert!(!mt.is_notified());
        assert_eq!(mt.enter(|| a.poll_put(42)), Ok(AsyncSink::Ready));
        assert!(mt.is_notified()); // taker is notified
        assert_eq!(mt.enter(|| a.poll_take()), Ok(Async::Ready(42)));

        assert_eq!(mt.enter(|| a.poll_take()), Ok(Async::NotReady));
        assert!(!mt.is_notified());
        assert_eq!(mt.enter(|| a.poll_put(43)), Ok(AsyncSink::Ready));
        assert!(mt.is_notified()); // taker is notified
        assert_eq!(mt.enter(|| a.poll_put(44)), Ok(AsyncSink::NotReady(44)));
        assert!(!mt.is_notified());
        assert_eq!(mt.enter(|| a.poll_take()), Ok(Async::Ready(43)));
        assert!(mt.is_notified()); // putter is notified
        assert_eq!(mt.enter(|| a.poll_take()), Ok(Async::NotReady));
        assert!(!mt.is_notified());
        assert_eq!(mt.enter(|| a.poll_put(44)), Ok(AsyncSink::Ready));
        assert!(mt.is_notified());

        // close fails since there's still a message to be sent
        assert_eq!(mt.enter(|| a.poll_close()), Ok(Async::NotReady));
        assert_eq!(mt.enter(|| a.poll_take()), Ok(Async::Ready(44)));
        assert!(mt.is_notified()); // closer is notified
        assert_eq!(mt.enter(|| a.poll_close()), Ok(Async::Ready(())));
        assert!(!mt.is_notified());
        assert_eq!(mt.enter(|| a.poll_take()), Err(Closed));
    }

    #[test]
    fn sink_stream() {
        use tokio::prelude::*;

        let a = new::<usize>();
        let (mut tx, rx) = tokio_sync::mpsc::unbounded_channel();
        tokio::run(future::lazy(move || {
            tokio::spawn(
                rx.forward(a.clone().sink_map_err(|_| unreachable!()))
                    .map(|_| ())
                    .map_err(|_| unreachable!()),
            );

            // send a bunch of things, and make sure we get them all
            tx.try_send(1).unwrap();
            tx.try_send(2).unwrap();
            tx.try_send(3).unwrap();
            tx.try_send(4).unwrap();
            tx.try_send(5).unwrap();
            drop(tx);

            a.collect()
                .inspect(|v| {
                    assert_eq!(v, &[1, 2, 3, 4, 5]);
                })
                .map(|_| ())
        }));
    }

    #[test]
    fn notified_on_empty_drop() {
        let mut mt = MockTask::new();

        let mut a = new::<usize>();
        assert_eq!(mt.enter(|| a.poll_take()), Ok(Async::NotReady));
        assert!(!mt.is_notified());
        assert_eq!(mt.enter(|| a.poll_close()), Ok(Async::Ready(())));
        assert!(mt.is_notified());
        assert_eq!(mt.enter(|| a.poll_take()), Err(Closed));
    }
}
