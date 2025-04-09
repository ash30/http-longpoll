#![feature(trace_macros)]

mod http_poll;
mod session;

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
        let (tx, rx) = mpsc::channel(config.request_capactiy);
        let s = Session::new(
            PollReqStream::new(ReqStream::new(rx)),
            config.message_max_size,
        );
        (Sender { tx }, s)
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
