#![feature(trace_macros)]

mod http_poll;
mod session;

use std::sync::mpsc::TrySendError;

use http::{Request, Response};
use tokio::sync::{mpsc, oneshot};

use http_poll::{ForwardedReq, PollReqStream, ReqStream};
use session::{FromPollRequest, IntoPollResponse};

pub use session::Session;

// API

pub struct Config {
    pub message_max_size: usize,
    pub request_capactiy: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            message_max_size: 1 << 20,
            request_capactiy: 32,
        }
    }
}

//#[cfg(feature = "axum")]
pub mod axum {
    use crate::session::Len;
    use crate::IntoPollResponse;
    use crate::Session;
    use axum::body::Body;
    use axum::response::Response;
    pub use bytes::Bytes;
    use bytes::BytesMut;

    pub type HTTPLongPoll<E> = Session<E, Body, Body>;

    impl Len for Bytes {
        fn len(&self) -> usize {
            self.len()
        }
    }

    impl<U> IntoPollResponse<U> for Bytes
    where
        U: From<Bytes>,
    {
        type Buffered = Self;
        fn into_poll_response(buf: Vec<Self::Buffered>) -> Response<U> {
            let mut out = BytesMut::new();
            for b in buf {
                out.extend_from_slice(&b);
            }
            Response::new(U::from(out.freeze()))
        }
    }
}

impl<E, T, U> Session<E, T, U>
where
    E: IntoPollResponse<U> + FromPollRequest<T> + Send + 'static,
{
    pub fn connect(config: &Config) -> (Sender<T, U>, Session<E, T, U>) {
        let (p_tx, p_rx) = mpsc::channel(config.request_capactiy);
        let (m_tx, m_rx) = mpsc::channel(config.request_capactiy);
        let s = Session::new(
            PollReqStream::new(ReqStream::new(p_rx)),
            config.message_max_size,
        );
        (Sender { tx: p_tx }, s)
    }
}

#[derive(Debug)]
pub enum SenderError<T> {
    Full(T),
    Closed(Option<T>),
}
impl<T, U> From<mpsc::error::TrySendError<ForwardedReq<T, U>>> for SenderError<Request<T>> {
    fn from(value: mpsc::error::TrySendError<ForwardedReq<T, U>>) -> Self {
        match value {
            mpsc::error::TrySendError::Closed(v) => Self::Closed(Some(v.0)),
            mpsc::error::TrySendError::Full(v) => Self::Full(v.0),
        }
    }
}
impl<T> From<oneshot::error::RecvError> for SenderError<Request<T>> {
    fn from(_: oneshot::error::RecvError) -> Self {
        Self::Closed(None)
    }
}

#[derive(Debug)]
pub struct Sender<T, U> {
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
    pub async fn send(&mut self, item: Request<T>) -> Result<Response<U>, SenderError<Request<T>>> {
        let (tx, rx) = oneshot::channel();
        self.tx.try_send((item, tx))?;
        rx.await.map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn init() {}
}
