//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//

//! Subscribing primitives.

use super::net::protocol::core::SubInfo;
use crate::prelude::{Id, KeyExpr, Sample};
use crate::sync::channel::Receiver;
use crate::sync::ZFuture;
use crate::time::Period;
use crate::Result as ZResult;
use crate::Session;
use crate::API_DATA_RECEPTION_CHANNEL_SIZE;
use async_std::sync::Arc;
use flume::{bounded, Iter, RecvError, RecvTimeoutError, Sender, TryIter, TryRecvError};
use std::fmt;
use std::pin::Pin;
use std::sync::RwLock;
use std::task::{Context, Poll};
use zenoh_util::sync::Runnable;

/// The subscription mode.
pub use super::net::protocol::core::SubMode;

/// The kind of reliability.
pub use super::net::protocol::core::Reliability;

/// The callback that will be called on each data for a [`CallbackSubscriber`](CallbackSubscriber).
pub type DataHandler = dyn FnMut(Sample) + Send + Sync + 'static;

pub(crate) enum SubscriberInvoker {
    Sender(Sender<Sample>),
    Handler(Arc<RwLock<DataHandler>>),
}

pub(crate) struct SubscriberState {
    pub(crate) id: Id,
    pub(crate) key_expr: KeyExpr<'static>,
    pub(crate) key_expr_str: String,
    pub(crate) invoker: SubscriberInvoker,
}

impl fmt::Debug for SubscriberState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Subscriber{{ id:{}, key_expr:{} }}",
            self.id, self.key_expr_str
        )
    }
}

zreceiver! {
    /// A [`Receiver`] of [`Sample`].
    ///
    /// Returned by [`Subscriber`](crate::Subscriber).[`receiver`](crate::Subscriber::receiver)(), it must be used
    /// to wait for queries mathing the [`Subscriber`](crate::Subscriber).
    ///
    /// The samples of this receiver can be accessed:
    ///  - synchronously as with a [`std::sync::mpsc::Receiver`](std::sync::mpsc::Receiver)
    ///  - asynchronously as with a [`async_std::channel::Receiver`](async_std::channel::Receiver).
    ///
    /// # Examples
    ///
    /// ### sync
    /// ```no_run
    /// # use zenoh::prelude::*;
    /// # let session = zenoh::open(config::peer()).wait().unwrap();
    ///
    /// let mut subscriber = session.subscribe("/key/expression").wait().unwrap();
    /// while let Ok(sample) = subscriber.receiver().recv() {
    ///      println!(">> Received sample '{}'", sample);
    /// }
    /// ```
    ///
    /// ### async
    /// ```no_run
    /// # async_std::task::block_on(async {
    /// # use futures::prelude::*;
    /// # use zenoh::prelude::*;
    /// # let session = zenoh::open(config::peer()).await.unwrap();
    ///
    /// let mut subscriber = session.subscribe("/key/expression").await.unwrap();
    /// while let Some(sample) = subscriber.receiver().next().await {
    ///      println!(">> Received sample '{}'", sample);
    /// }
    /// # })
    /// ```
    #[derive(Clone)]
    pub struct SampleReceiver : Receiver<Sample> {}
}

/// A subscriber that provides data through a stream.
///
/// Subscribers are automatically undeclared when dropped.
pub struct Subscriber<'a> {
    pub(crate) session: &'a Session,
    pub(crate) state: Arc<SubscriberState>,
    pub(crate) alive: bool,
    pub(crate) receiver: SampleReceiver,
}

impl Subscriber<'_> {
    pub fn receiver(&mut self) -> &mut SampleReceiver {
        &mut self.receiver
    }

    /// Pull available data for a pull-mode [`Subscriber`](Subscriber).
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use futures::prelude::*;
    /// use zenoh::prelude::*;
    /// use zenoh::subscriber::SubMode;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// let mut subscriber = session.subscribe("/key/expression")
    ///                             .mode(SubMode::Pull).await.unwrap();
    /// async_std::task::spawn(subscriber.receiver().clone().for_each(
    ///     move |sample| async move { println!("Received : {:?}", sample); }
    /// ));
    /// subscriber.pull();
    /// # })
    /// ```
    #[must_use = "ZFutures do nothing unless you `.wait()`, `.await` or poll them"]
    pub fn pull(&self) -> impl ZFuture<Output = ZResult<()>> {
        self.session.pull(&self.state.key_expr)
    }

    /// Close a [`Subscriber`](Subscriber) previously created with [`subscribe`](Session::subscribe).
    ///
    /// Subscribers are automatically closed when dropped, but you may want to use this function to handle errors or
    /// close the Subscriber asynchronously.
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use zenoh::prelude::*;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// let subscriber = session.subscribe("/key/expression").await.unwrap();
    /// subscriber.close().await.unwrap();
    /// # })
    /// ```
    #[inline]
    #[must_use = "ZFutures do nothing unless you `.wait()`, `.await` or poll them"]
    pub fn close(mut self) -> impl ZFuture<Output = ZResult<()>> {
        self.alive = false;
        self.session.unsubscribe(self.state.id)
    }
}

impl Drop for Subscriber<'_> {
    fn drop(&mut self) {
        if self.alive {
            let _ = self.session.unsubscribe(self.state.id).wait();
        }
    }
}

impl fmt::Debug for Subscriber<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.state.fmt(f)
    }
}

/// A subscriber that provides data through a callback.
///
/// Subscribers are automatically undeclared when dropped.
pub struct CallbackSubscriber<'a> {
    pub(crate) session: &'a Session,
    pub(crate) state: Arc<SubscriberState>,
    pub(crate) alive: bool,
}

impl CallbackSubscriber<'_> {
    /// Pull available data for a pull-mode [`CallbackSubscriber`](CallbackSubscriber).
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use zenoh::prelude::*;
    /// use zenoh::subscriber::SubMode;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// let subscriber = session.subscribe("/key/expression")
    ///     .callback(|sample| { println!("Received : {} {}", sample.key_expr, sample.value); })
    ///     .mode(SubMode::Pull).await.unwrap();
    /// subscriber.pull();
    /// # })
    /// ```
    #[must_use = "ZFutures do nothing unless you `.wait()`, `.await` or poll them"]
    pub fn pull(&self) -> impl ZFuture<Output = ZResult<()>> {
        self.session.pull(&self.state.key_expr)
    }

    /// Undeclare a [`CallbackSubscriber`](CallbackSubscriber).
    ///
    /// `CallbackSubscribers` are automatically undeclared when dropped, but you may want to use this function to handle errors or
    /// undeclare the `CallbackSubscriber` asynchronously.
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use zenoh::prelude::*;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// # fn data_handler(_sample: Sample) { };
    /// let subscriber = session.subscribe("/key/expression")
    ///     .callback(data_handler).await.unwrap();
    /// subscriber.undeclare().await.unwrap();
    /// # })
    /// ```
    #[inline]
    #[must_use = "ZFutures do nothing unless you `.wait()`, `.await` or poll them"]
    pub fn undeclare(mut self) -> impl ZFuture<Output = ZResult<()>> {
        self.alive = false;
        self.session.unsubscribe(self.state.id)
    }
}

impl Drop for CallbackSubscriber<'_> {
    fn drop(&mut self) {
        if self.alive {
            let _ = self.session.unsubscribe(self.state.id).wait();
        }
    }
}

impl fmt::Debug for CallbackSubscriber<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.state.fmt(f)
    }
}

derive_zfuture! {
    /// A builder for initializing a [`Subscriber`](Subscriber).
    ///
    /// The result of this builder can be accessed synchronously via [`wait()`](ZFuture::wait())
    /// or asynchronously via `.await`.
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use zenoh::prelude::*;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// let subscriber = session
    ///     .subscribe("/key/expression")
    ///     .best_effort()
    ///     .pull_mode()
    ///     .await
    ///     .unwrap();
    /// # })
    /// ```
    #[derive(Debug, Clone)]
    pub struct SubscriberBuilder<'a, 'b> {
        pub(crate) session: &'a Session,
        pub(crate) key_expr: KeyExpr<'b>,
        pub(crate) reliability: Reliability,
        pub(crate) mode: SubMode,
        pub(crate) period: Option<Period>,
        pub(crate) local: bool,
    }
}

impl<'a, 'b> SubscriberBuilder<'a, 'b> {
    /// Make the built Subscriber a [`CallbackSubscriber`](CallbackSubscriber).
    #[inline]
    pub fn callback<DataHandler>(self, handler: DataHandler) -> CallbackSubscriberBuilder<'a, 'b>
    where
        DataHandler: FnMut(Sample) + Send + Sync + 'static,
    {
        CallbackSubscriberBuilder {
            session: self.session,
            key_expr: self.key_expr,
            reliability: self.reliability,
            mode: self.mode,
            period: self.period,
            local: self.local,
            handler: Arc::new(RwLock::new(handler)),
        }
    }

    /// Change the subscription reliability.
    #[inline]
    pub fn reliability(mut self, reliability: Reliability) -> Self {
        self.reliability = reliability;
        self
    }

    /// Change the subscription reliability to `Reliable`.
    #[inline]
    pub fn reliable(mut self) -> Self {
        self.reliability = Reliability::Reliable;
        self
    }

    /// Change the subscription reliability to `BestEffort`.
    #[inline]
    pub fn best_effort(mut self) -> Self {
        self.reliability = Reliability::BestEffort;
        self
    }

    /// Change the subscription mode.
    #[inline]
    pub fn mode(mut self, mode: SubMode) -> Self {
        self.mode = mode;
        self
    }

    /// Change the subscription mode to Push.
    #[inline]
    pub fn push_mode(mut self) -> Self {
        self.mode = SubMode::Push;
        self.period = None;
        self
    }

    /// Change the subscription mode to Pull.
    #[inline]
    pub fn pull_mode(mut self) -> Self {
        self.mode = SubMode::Pull;
        self
    }

    /// Change the subscription period.
    #[inline]
    pub fn period(mut self, period: Option<Period>) -> Self {
        self.period = period;
        self
    }

    /// Make the subscription local only.
    #[inline]
    pub fn local(mut self) -> Self {
        self.local = true;
        self
    }
}

impl<'a> Runnable for SubscriberBuilder<'a, '_> {
    type Output = ZResult<Subscriber<'a>>;

    fn run(&mut self) -> Self::Output {
        log::trace!("subscribe({:?})", self.key_expr);
        let (sender, receiver) = bounded(*API_DATA_RECEPTION_CHANNEL_SIZE);

        if self.local {
            self.session
                .declare_any_local_subscriber(&self.key_expr, SubscriberInvoker::Sender(sender))
                .map(|sub_state| Subscriber {
                    session: self.session,
                    state: sub_state,
                    alive: true,
                    receiver: SampleReceiver::new(receiver),
                })
        } else {
            self.session
                .declare_any_subscriber(
                    &self.key_expr,
                    SubscriberInvoker::Sender(sender),
                    &SubInfo {
                        reliability: self.reliability,
                        mode: self.mode,
                        period: self.period,
                    },
                )
                .map(|sub_state| Subscriber {
                    session: self.session,
                    state: sub_state,
                    alive: true,
                    receiver: SampleReceiver::new(receiver),
                })
        }
    }
}

derive_zfuture! {
    /// A builder for initializing a [`CallbackSubscriber`](CallbackSubscriber).
    ///
    /// The result of this builder can be accessed synchronously via [`wait()`](ZFuture::wait())
    /// or asynchronously via `.await`.
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use zenoh::prelude::*;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// let subscriber = session
    ///     .subscribe("/key/expression")
    ///     .callback(|sample| { println!("Received : {} {}", sample.key_expr, sample.value); })
    ///     .best_effort()
    ///     .pull_mode()
    ///     .await
    ///     .unwrap();
    /// # })
    /// ```
    #[derive(Clone)]
    pub struct CallbackSubscriberBuilder<'a, 'b> {
        session: &'a Session,
        key_expr: KeyExpr<'b>,
        reliability: Reliability,
        mode: SubMode,
        period: Option<Period>,
        local: bool,
        handler: Arc<RwLock<DataHandler>>,
    }
}

impl fmt::Debug for CallbackSubscriberBuilder<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallbackSubscriberBuilder")
            .field("session", self.session)
            .field("key_expr", &self.key_expr)
            .field("reliability", &self.reliability)
            .field("mode", &self.mode)
            .field("period", &self.period)
            .finish()
    }
}

impl<'a, 'b> CallbackSubscriberBuilder<'a, 'b> {
    /// Change the subscription reliability.
    #[inline]
    pub fn reliability(mut self, reliability: Reliability) -> Self {
        self.reliability = reliability;
        self
    }

    /// Change the subscription reliability to `Reliable`.
    #[inline]
    pub fn reliable(mut self) -> Self {
        self.reliability = Reliability::Reliable;
        self
    }

    /// Change the subscription reliability to `BestEffort`.
    #[inline]
    pub fn best_effort(mut self) -> Self {
        self.reliability = Reliability::BestEffort;
        self
    }

    /// Change the subscription mode.
    #[inline]
    pub fn mode(mut self, mode: SubMode) -> Self {
        self.mode = mode;
        self
    }

    /// Change the subscription mode to Push.
    #[inline]
    pub fn push_mode(mut self) -> Self {
        self.mode = SubMode::Push;
        self.period = None;
        self
    }

    /// Change the subscription mode to Pull.
    #[inline]
    pub fn pull_mode(mut self) -> Self {
        self.mode = SubMode::Pull;
        self
    }

    /// Change the subscription period.
    #[inline]
    pub fn period(mut self, period: Option<Period>) -> Self {
        self.period = period;
        self
    }

    /// Make the subscription local onlyu.
    #[inline]
    pub fn local(mut self) -> Self {
        self.local = true;
        self
    }
}

impl<'a> Runnable for CallbackSubscriberBuilder<'a, '_> {
    type Output = ZResult<CallbackSubscriber<'a>>;

    fn run(&mut self) -> Self::Output {
        log::trace!("declare_callback_subscriber({:?})", self.key_expr);

        if self.local {
            self.session
                .declare_any_local_subscriber(
                    &self.key_expr,
                    SubscriberInvoker::Handler(self.handler.clone()),
                )
                .map(|sub_state| CallbackSubscriber {
                    session: self.session,
                    state: sub_state,
                    alive: true,
                })
        } else {
            self.session
                .declare_any_subscriber(
                    &self.key_expr,
                    SubscriberInvoker::Handler(self.handler.clone()),
                    &SubInfo {
                        reliability: self.reliability,
                        mode: self.mode,
                        period: self.period,
                    },
                )
                .map(|sub_state| CallbackSubscriber {
                    session: self.session,
                    state: sub_state,
                    alive: true,
                })
        }
    }
}
