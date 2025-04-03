#![feature(trace_macros)]

mod http_poll;
mod session;

#[doc(no_inline)]
pub use bytes::Bytes;

use axum::body::Body;
use axum::{extract::Request, response::Response};
use futures::Future;
use tokio::sync::{mpsc, oneshot};

use http_poll::{ForwardedReq, PollReqStream, ReqStream};
use session::{FromPollRequest, IntoPollResponse};

pub use session::Session;

// API

pub struct HTTPLongPoll {
    message_max_size: usize,
    request_capactiy: usize,
}

impl Default for HTTPLongPoll {
    fn default() -> Self {
        Self {
            message_max_size: 1 << 20,
            request_capactiy: 32,
        }
    }
}

impl HTTPLongPoll {
    pub fn connect<F, Fut, E, T, U>(&self, callback: F) -> Sender<T, U>
    where
        F: FnOnce(Session<E, T, U>) -> Fut + Send + 'static,
        E: IntoPollResponse<U> + FromPollRequest<T> + Send + 'static,
        T: Send + 'static,
        U: Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(self.request_capactiy);
        let s = Session::new(
            PollReqStream::new(ReqStream::new(rx)),
            self.message_max_size,
        );
        tokio::spawn(async move { callback(s).await });
        Sender { tx }
    }
}

pub struct SenderError<T>(Option<Request<T>>);

impl<T, U> From<mpsc::error::SendError<ForwardedReq<T, U>>> for SenderError<T> {
    fn from(value: mpsc::error::SendError<ForwardedReq<T, U>>) -> Self {
        Self(Some(value.0 .0))
    }
}
impl<T> From<oneshot::error::RecvError> for SenderError<T> {
    fn from(_: oneshot::error::RecvError) -> Self {
        Self(None)
    }
}

#[derive(Debug)]
pub struct Sender<T = Body, U = Body> {
    tx: mpsc::Sender<ForwardedReq<T, U>>,
}

impl<T, U> Clone for Sender<T, U> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<T, U> Sender<T, U> {
    pub async fn send(&mut self, item: Request<T>) -> Result<Response<U>, SenderError<T>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send((item, tx)).await?;
        rx.await.map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn init() {}
}
