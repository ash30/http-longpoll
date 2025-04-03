#![feature(trace_macros)]

//trace_macros!(true);
pub use axum::body::Bytes;

use axum::body::Body;
use axum::extract::FromRequest;
use axum::http::{Method, StatusCode};
use axum::response::{ErrorResponse, IntoResponse};
use axum::Json;
use axum::{extract::Request, response::Response};
use bytes::buf::Limit;
use bytes::{Buf, BufMut, BytesMut};
use futures::future::BoxFuture;
use futures::{Future, Sink, Stream, TryFutureExt};
use pin_project_lite::pin_project;
use serde::Serialize;
use serde_json::value::RawValue;
use std::collections::VecDeque;
use std::default;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::sync::{mpsc, oneshot};

type ResponseCallback<U> = oneshot::Sender<Response<U>>;
type ForwardedReq<T, U> = (Request<T>, ResponseCallback<U>);

pin_project! {
    #[derive(Debug)]
    pub struct ReqStream<T,U> {
        rx: mpsc::Receiver<ForwardedReq<T,U>>,
    }
}

impl<T, U> ReqStream<T, U> {
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }
    pub fn close(&mut self) {
        self.rx.close()
    }
}

impl<T, U> Stream for ReqStream<T, U> {
    type Item = ForwardedReq<T, U>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_closed() {
            Poll::Ready(None)
        } else {
            self.project().rx.poll_recv(cx)
        }
    }
}

// =======

pub enum Payload<T> {
    Poll,
    Req(T),
}

pin_project! {
    pub struct HTTPPoll<T,U> {
        #[pin]
        inner:ReqStream<T,U>,
        next_payload: VecDeque<Payload<ForwardedReq<T,U>>>,
        next_res:Option<ForwardedReq<T,U>>,
        next_send: Option<Response<U>>,
        closed:Option<HTTPPollError>
    }
}

impl<T, U> HTTPPoll<T, U> {
    fn new(s: ReqStream<T, U>) -> Self {
        HTTPPoll {
            inner: s,
            next_payload: VecDeque::new(),
            next_res: None,
            next_send: None,
            closed: None,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum HTTPPollError {
    RemoteClose,
    LocalClose,
    AlreadyClosed,
    PollingError,
}

impl<T, U> Stream for HTTPPoll<T, U>
where
    U: From<()>,
{
    type Item = Result<Payload<ForwardedReq<T, U>>, HTTPPollError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(p) = self.as_mut().project().next_payload.pop_front() {
                return Poll::Ready(Some(Ok(p)));
            }
            ready!(self.as_mut().poll_inner(cx))?;
        }
    }
}

impl<T, U> HTTPPoll<T, U>
where
    U: From<()>,
{
    fn close(self: Pin<&mut Self>, err: HTTPPollError) -> HTTPPollError {
        // close existing polling responses
        let this = self.project();
        if let Some((_, polling_callback)) = this.next_res.take() {
            let _ = polling_callback.send(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(().into())
                    .unwrap(),
            );
        }
        if this.closed.is_none() {
            this.closed.replace(err);
        }
        err
    }

    fn poll_inner(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), HTTPPollError>> {
        loop {
            if let Some(e) = self.closed {
                return Poll::Ready(Err(e));
            }

            match ready!(self.as_mut().project().inner.poll_next(cx)) {
                // shouldn't really get here...
                None => break Poll::Ready(Err(HTTPPollError::AlreadyClosed)),

                Some((req, res)) => match *req.method() {
                    Method::GET => {
                        // ERROR!
                        if self.next_res.is_some() {
                            // close original poll
                            // send err to new poll
                            let _ = res.send(
                                Response::builder()
                                    .status(StatusCode::BAD_REQUEST)
                                    .body(().into())
                                    .unwrap(),
                            );
                            let e = self.close(HTTPPollError::PollingError);
                            break Poll::Ready(Err(e));
                        } else {
                            let this = self.project();
                            this.next_res.replace((req, res));
                            this.next_payload.push_back(Payload::Poll);
                            break Poll::Ready(Ok(()));
                        }
                    }
                    Method::POST => {
                        self.as_mut()
                            .project()
                            .next_payload
                            .push_back(Payload::Req((req, res)));
                        break Poll::Ready(Ok(()));
                    }
                    Method::DELETE => {
                        let e = self.close(HTTPPollError::RemoteClose);
                        break Poll::Ready(Err(e));
                    }
                    _ => {
                        let _ = res.send(
                            Response::builder()
                                .status(StatusCode::METHOD_NOT_ALLOWED)
                                .body(().into())
                                .unwrap(),
                        );
                    }
                },
            }
        }
    }
}

impl<T, U> Sink<Response<U>> for HTTPPoll<T, U>
where
    U: From<()>,
{
    type Error = HTTPPollError;

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.close(HTTPPollError::LocalClose);
        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            if self.next_send.is_none() {
                return Poll::Ready(Ok(()));
            }
            if let Some((_, callback)) = self.next_res.take() {
                // In theory, only get here when not closed...
                if let Err(_) = callback.send(self.as_mut().project().next_send.take().unwrap()) {
                    todo!()
                    //return Poll::Ready(Err(todo!()));
                }
            } else {
                ready!(self.as_mut().poll_inner(cx))?;
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Response<U>) -> Result<(), Self::Error> {
        if self.next_send.is_some() {
            todo!()
            // Err(todo!());
        } else {
            self.project().next_send.replace(item);
            Ok(())
        }
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            if self.next_send.is_some() {
                ready!(self.as_mut().poll_flush(cx))?;
            }
            if self.next_res.is_some() {
                return Poll::Ready(Ok(()));
            }
            ready!(self.as_mut().poll_inner(cx))?;
        }
    }
}

pin_project! {
    pub struct Framer<T,U,E:IntoPollResponse<U>> {
        #[pin]
        inner: HTTPPoll<T,U>,

        // This should be bytes... and we should just write ito it?
        buf: VecDeque<E::Buffered>,
        max_size:usize,

        next: Option<(BoxFuture<'static,Result<E,Response<U>>>, ResponseCallback<U>)>,
    }
}

impl<T, U, E> Framer<T, U, E>
where
    E: IntoPollResponse<U>,
{
    fn new(s: HTTPPoll<T, U>, max_size: usize) -> Self {
        let mut buf = VecDeque::default();
        buf.make_contiguous();
        Framer {
            inner: s,
            next: None,
            buf,
            max_size,
        }
    }
}

pub trait FromPollRequest<T>: Sized {
    fn from_poll_req(req: Request<T>) -> impl Future<Output = Result<Self, Response>> + Send;
}

pub trait Len {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub trait IntoPollResponse<U>: Sized {
    type Buffered: TryFrom<Self> + Len + Send;
    fn into_poll_response(buf: Vec<Self::Buffered>) -> Response<U>;
}

/// ======
// Axum
impl<T> FromPollRequest<Body> for T
where
    T: FromRequest<()>,
{
    fn from_poll_req(req: Request) -> impl Future<Output = Result<Self, Response>> + Send {
        T::from_request(req, &()).map_err(|e| e.into_response())
    }
}

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

impl<T, U, E> Stream for Framer<T, U, E>
where
    U: From<()>,
    E: FromPollRequest<T> + IntoPollResponse<U>,
{
    type Item = Result<E, HTTPPollError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        Poll::Ready(loop {
            if let Some(p) = this.next.as_mut().map(|f| f.0.as_mut().poll(cx)) {
                // If extractor finished
                let e = ready!(p);
                let (_, res) = this.next.take().unwrap();
                let _ = res.send(
                    Response::builder()
                        .status(if e.is_ok() {
                            StatusCode::OK
                        } else {
                            StatusCode::BAD_REQUEST
                        })
                        .body(().into())
                        .unwrap(),
                );
                if e.is_ok() {
                    break Some(e.map_err(|_| HTTPPollError::PollingError));
                } else {
                    continue;
                }
            } else if let Some(n) = ready!(this.inner.poll_next(cx)) {
                // get next extractor fut
                todo!()
            } else {
                // inner has finished
                break None;
            }
        })
    }
}

impl<T, U, E> Framer<T, U, E>
where
    U: From<()>,
    E: IntoPollResponse<U>,
{
    fn poll_empty(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), HTTPPollError>> {
        loop {
            if self.buf.is_empty() {
                return Poll::Ready(Ok(()));
            }
            ready!(self.as_mut().project().inner.poll_ready(cx))?;
            let this = self.as_mut().project();
            let mut v = vec![];
            let mut n = 0;
            loop {
                let Some(b) = this.buf.front() else { break };
                if n + b.len() > *this.max_size {
                    break;
                }
                n += b.len();
                v.push(this.buf.pop_front().unwrap());
            }
            let res = E::into_poll_response(v);
            if let Err(e) = this.inner.start_send(res) {
                return Poll::Ready(Err(e));
            };
        }
    }
}

pub enum FrameError<E> {
    Send(E),
    SendLimit,
    Polling(HTTPPollError),
}

impl<E> From<HTTPPollError> for FrameError<E> {
    fn from(value: HTTPPollError) -> Self {
        FrameError::Polling(value)
    }
}

impl<T, U, E> Sink<E> for Framer<T, U, E>
where
    U: From<()>,
    E: IntoPollResponse<U>,
{
    type Error = FrameError<<E::Buffered as TryFrom<E>>::Error>;

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.project().inner.poll_close(cx).map_err(|e| e.into())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_empty(cx))?;
        // need to flush the last one too
        self.project().inner.poll_flush(cx).map_err(|e| e.into())
    }

    fn start_send(self: Pin<&mut Self>, item: E) -> Result<(), Self::Error> {
        let n: E::Buffered = item.try_into().map_err(FrameError::Send)?;
        if n.len() > self.max_size {
            Err(FrameError::SendLimit)
        } else {
            self.project().buf.push_back(n);
            Ok(())
        }
    }

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.buf.len() > self.max_size {
            self.poll_empty(cx).map_err(|e| e.into())
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

pub type Session<Ex = Bytes, B = Body, Res = Body> = Framer<B, Res, Ex>;

// ==========

pub struct LongPoll {
    message_max_size: usize,
    request_capactiy: usize,
}

impl Default for LongPoll {
    fn default() -> Self {
        Self {
            message_max_size: 1 << 20,
            request_capactiy: 32,
        }
    }
}

impl LongPoll {
    pub fn connect<F, Fut, E, T, U>(&self, callback: F) -> Sender<T, U>
    where
        F: FnOnce(Session<E, T, U>) -> Fut + Send + 'static,
        E: IntoPollResponse<U> + FromPollRequest<T> + Send + 'static,
        T: Send + 'static,
        U: Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(self.request_capactiy);
        let s = Session::new(HTTPPoll::new(ReqStream { rx }), self.message_max_size);
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
    fn init() {
        //let a = HTTPLongpoll::new_layer();
        //a.add("test".to_string(), 10);
    }
}
