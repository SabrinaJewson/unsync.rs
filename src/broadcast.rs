//! A dynamically-sized `!Send` broadcast channel.
//!
//! This does allocate storage internally to maintain shared state between the
//! [Sender] and [Receiver].

use crate::broad_ref::{BroadRef, Weak};
use std::error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// Error raised when sending a message over the queue.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct SendError;

impl fmt::Debug for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SendError").finish()
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no receivers to broadcast channel")
    }
}

impl error::Error for SendError {}

struct ReceiverState<T> {
    /// Last message id received.
    id: u64,
    /// Waker to wake once receiving is available.
    waker: Option<Waker>,
    /// Test if the interior value is set.
    buf: Option<T>,
}

/// Interior shared state.
struct Shared<T> {
    /// The current message ID.
    id: u64,
    /// Waker to wake once sending is available.
    sender: Option<Waker>,
    /// Collection of receivers.
    receivers: slab::Slab<ReceiverState<T>>,
}

/// Sender end of this queue.
pub struct Sender<T>
where
    T: Clone,
{
    inner: BroadRef<Shared<T>>,
}

impl<T> Sender<T>
where
    T: Clone,
{
    /// Construct a new receiver and return its index in the slab of stored
    /// receivers.
    fn new_receiver(&mut self) -> usize {
        // Safety: Since this structure is single-threaded there is now way to
        // hold an inner reference at multiple locations.
        unsafe {
            let (inner, _) = self.inner.get_mut_unchecked();

            inner.receivers.insert(ReceiverState {
                id: inner.id,
                waker: None,
                buf: None,
            })
        }
    }

    /// Subscribe to the broadcast channel.
    ///
    /// This sets up a new [Receiver] which is guaranteed to receive all updates
    /// on this broadcast channel.
    ///
    /// Note that this means that *slow receivers* are capable of hogging down
    /// the entire broadcast system since they must be delievered to (or
    /// dropped) in order for the system to make progress.
    pub fn subscribe(&mut self) -> Receiver<T> {
        let index = self.new_receiver();

        Receiver {
            index,
            inner: self.inner.weak(),
        }
    }

    /// Get a count on the number of subscribers.
    pub fn subscribers(&self) -> usize {
        unsafe {
            let (inner, _) = self.inner.get_mut_unchecked();
            inner.receivers.len()
        }
    }

    /// Receive a message on the channel.
    ///
    /// Note that *not driving the returned future to completion* might result
    /// in some receivers not receiving value being sent.
    pub fn send(&mut self, value: T) -> Send<'_, T> {
        // Increase the ID of messages to send.
        unsafe {
            let (inner, _) = self.inner.get_mut_unchecked();

            inner.id = inner.id.wrapping_add(1);

            // Avoid 0, since that is what receivers are initialized to.
            if inner.id == 0 {
                inner.id = 1;
            }
        }

        Send {
            inner: &self.inner,
            value,
        }
    }
}

/// Future produced by [Sender::send].
pub struct Send<'a, T> {
    inner: &'a BroadRef<Shared<T>>,
    value: T,
}

impl<'a, T> Future for Send<'a, T>
where
    T: Clone,
{
    type Output = Result<(), SendError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);

            let (inner, any_receivers_present) = this.inner.get_mut_unchecked();

            if !any_receivers_present {
                return Poll::Ready(Err(SendError));
            }

            if !matches!(&inner.sender, Some(w) if w.will_wake(cx.waker())) {
                inner.sender = Some(cx.waker().clone());
            }

            loop {
                let mut any_sent = false;
                let mut delivered = 0;

                for (_, receiver) in &mut inner.receivers {
                    if receiver.id == inner.id {
                        delivered += 1;
                        continue;
                    }

                    // Value is in the process of being delivered to this
                    // receiver.
                    if receiver.buf.is_some() {
                        continue;
                    }

                    receiver.buf = Some(this.value.clone());

                    if let Some(waker) = &receiver.waker {
                        waker.wake_by_ref();
                    }

                    any_sent = true;
                }

                if delivered == inner.receivers.len() {
                    return Poll::Ready(Ok(()));
                }

                if any_sent {
                    continue;
                }

                return Poll::Pending;
            }
        }
    }
}

/// Receiver end of this queue.
pub struct Receiver<T> {
    index: usize,
    inner: Weak<Shared<T>>,
}

impl<T> Receiver<T> {
    /// Receive a message on the channel.
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv { receiver: self }
    }
}

/// Future associated with receiving.
pub struct Recv<'a, T> {
    receiver: &'a mut Receiver<T>,
}

impl<'a, T> Future for Recv<'a, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            let index = this.receiver.index;
            let (inner, sender_present) = this.receiver.inner.load();

            let receiver = match inner.receivers.get_mut(index) {
                Some(receiver) => receiver,
                None => return Poll::Ready(None),
            };

            if let Some(value) = receiver.buf.take() {
                receiver.id = inner.id;

                // Senders have interest once a buffer has been taken.
                if let Some(waker) = &inner.sender {
                    waker.wake_by_ref();
                }

                return Poll::Ready(Some(value));
            }

            if !sender_present {
                receiver.waker = None;
                return Poll::Ready(None);
            }

            if !matches!(&receiver.waker, Some(w) if !w.will_wake(cx.waker())) {
                receiver.waker = Some(cx.waker().clone())
            }

            if let Some(waker) = &inner.sender {
                waker.wake_by_ref();
            }

            Poll::Pending
        }
    }
}

impl<T> Drop for Recv<'_, T> {
    fn drop(&mut self) {
        unsafe {
            let index = self.receiver.index;
            let (inner, _) = self.receiver.inner.load();

            if let Some(receiver) = inner.receivers.get_mut(index) {
                receiver.buf = None;
            }
        }
    }
}

impl<T> Drop for Sender<T>
where
    T: Clone,
{
    fn drop(&mut self) {
        unsafe {
            let (inner, _) = self.inner.get_mut_unchecked();

            for (_, r) in &mut inner.receivers {
                if let Some(waker) = r.waker.take() {
                    waker.wake();
                }
            }
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        unsafe {
            let index = self.index;
            let (inner, _) = self.inner.load();
            let _ = inner.receivers.try_remove(index);

            if let Some(waker) = self.inner.load().0.sender.take() {
                waker.wake();
            }
        }
    }
}

/// Setup a broadcast channel.
pub fn channel<T>() -> Sender<T>
where
    T: Clone,
{
    let inner = BroadRef::new(Shared {
        id: 0,
        sender: None,
        receivers: slab::Slab::new(),
    });

    Sender { inner }
}
