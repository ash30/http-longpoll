#![feature(trace_macros)]

mod http_poll;

use futures::{Sink, Stream};
use http::{Request, Response};
use http_poll::{
    ForwardedReq, ForwardedReqChan, FromPollRequest, IntoPollResponse, PollRequestExtractor,
    ResponseFramer, ResponseFramerError, Writer, WriterError,
};
use pin_project_lite::pin_project;
use tokio::sync::{mpsc, oneshot};

// API
#[derive(Copy, Clone)]
pub struct Config {
    pub message_max_size: usize,
    pub request_capactiy: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            message_max_size: 1 << 20,
            request_capactiy: 16,
        }
    }
}

//#[cfg(feature = "axum")]
pub mod axum {
    use std::convert::Infallible;

    use crate::http_poll::{ForwardedReqChan, FromPollRequest, IntoPollResponse, Len};
    use axum::body::Body;
    use axum::extract::{FromRequest, Request};
    use axum::response::Response;
    pub use bytes::Bytes;
    use bytes::BytesMut;
    use futures::Future;

    pub type Session<E = Bytes> = super::Session<E, ForwardedReqChan<Body>>;
    pub use super::Sender;

    // Allow common response types to be used as aggregate longpoll response bodies
    //
    // Bytes
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
        type Err = Infallible;
        fn into_poll_response(buf: Vec<Self::Buffered>) -> Response<U> {
            let mut out = BytesMut::new();
            for b in buf {
                out.extend_from_slice(&b);
            }
            Response::new(U::from(out.freeze()))
        }
    }

    // Allow extractors to be used within message stream
    impl<T> FromPollRequest<Body> for T
    where
        T: FromRequest<()> + 'static,
    {
        type Error = T::Rejection;
        fn from_poll_req(req: Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
            T::from_request(req, &())
        }
    }
}

// Trait to simplify external API
trait LongPollRequestStream: Stream<Item = ForwardedReq<Self::Body>> {
    type Body: From<()>;
}

impl<T> LongPollRequestStream for ForwardedReqChan<T>
where
    T: From<()>,
{
    type Body = T;
}

pin_project! {
    struct Session<E,S> where S:LongPollRequestStream, E:FromPollRequest<S::Body>, E:IntoPollResponse<S::Body>{
        #[pin]
        read: PollRequestExtractor<E,S,S::Body>,
        #[pin]
        write: ResponseFramer<E,Writer<S,S::Body>,S::Body>
    }
}

impl<E, S> Session<E, S>
where
    S: LongPollRequestStream,
    E: FromPollRequest<S::Body>,
    E: IntoPollResponse<S::Body>,
{
    fn new(read: S, write: S, max_size: usize) -> Self {
        Self {
            read: PollRequestExtractor::new(read),
            write: ResponseFramer::new(max_size, Writer::new(write)),
        }
    }
}
// PROXY STREAM AND SINK
impl<E, S> Stream for Session<E, S>
where
    S: LongPollRequestStream,
    E: FromPollRequest<S::Body>,
    E: IntoPollResponse<S::Body>,
{
    type Item = Result<E, E::Error>;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.project().read.poll_next(cx)
    }
}

impl<E, S> Sink<E> for Session<E, S>
where
    S: LongPollRequestStream,
    E: FromPollRequest<S::Body>,
    E: IntoPollResponse<S::Body>,
{
    type Error = ResponseFramerError<WriterError>;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.project().write.poll_ready(cx)
    }

    fn start_send(self: std::pin::Pin<&mut Self>, item: E) -> Result<(), Self::Error> {
        self.project().write.start_send(item)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.project().write.poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.project().write.poll_close(cx)
    }
}

//
impl<E, B> Session<E, ForwardedReqChan<B>>
where
    E: FromPollRequest<B>,
    E: IntoPollResponse<B>,
    B: From<()>,
{
    pub fn connect(config: Config) -> (Sender<B>, Session<E, ForwardedReqChan<B>>) {
        let (p_tx, p_rx) = mpsc::channel(config.request_capactiy);
        let (m_tx, m_rx) = mpsc::channel(config.request_capactiy);
        (
            Sender {
                msg: m_tx,
                poll: p_tx,
            },
            Session::new(
                ForwardedReqChan::new(m_rx),
                ForwardedReqChan::new(p_rx),
                config.message_max_size,
            ),
        )
    }
}

#[derive(Debug)]
pub struct Sender<B> {
    msg: mpsc::Sender<ForwardedReq<B>>,
    poll: mpsc::Sender<ForwardedReq<B>>,
}

// Implement Clone manually to avoid B affecting derive
impl<B> Clone for Sender<B> {
    fn clone(&self) -> Self {
        Self {
            msg: self.msg.clone(),
            poll: self.poll.clone(),
        }
    }
}

impl<B> Sender<B> {
    pub async fn msg(&mut self, item: Request<B>) -> Result<Response<B>, SenderError<Request<B>>> {
        let (tx, rx) = oneshot::channel();
        self.msg.try_send((item, tx))?;
        rx.await.map_err(|e| e.into())
    }

    pub async fn poll(&mut self, item: Request<B>) -> Result<Response<B>, SenderError<Request<B>>> {
        let (tx, rx) = oneshot::channel();
        self.poll.try_send((item, tx))?;
        rx.await.map_err(|e| e.into())
    }
}

#[derive(Debug)]
pub enum SenderError<T> {
    Full(T),
    Closed(Option<T>),
}

impl<T> From<mpsc::error::TrySendError<ForwardedReq<T>>> for SenderError<Request<T>> {
    fn from(value: mpsc::error::TrySendError<ForwardedReq<T>>) -> Self {
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

#[cfg(test)]
mod tests {

    #[test]
    fn init() {}
}
