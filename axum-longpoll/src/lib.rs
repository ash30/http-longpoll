#![feature(trace_macros)]

//trace_macros!(true);

use axum::body::{Body, Bytes};
use axum::extract::rejection::ExtensionRejection;
use axum::extract::FromRequestParts;
use axum::http::{Method, StatusCode};
use axum::Extension;
use axum::{extract::Request, response::Response};
use futures::{stream, Future, Sink, Stream};
use pin_project_lite::pin_project;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::sync::{mpsc, oneshot};

type ResponseCallback<T> = oneshot::Sender<Response<T>>;
type ForwardedReq<T> = (Request<T>, ResponseCallback<T>);

pin_project! {
    #[derive(Debug)]
    pub struct ReqStream<T> {
        rx: mpsc::Receiver<ForwardedReq<T>>,
    }
}

impl<T> ReqStream<T> {
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }
    pub fn close(&mut self) {
        self.rx.close()
    }
}

impl<T> Stream for ReqStream<T> {
    type Item = (Request<T>, oneshot::Sender<Response<T>>);
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
    Msg(T),
}

pin_project! {
    pub struct HTTPPoll<S:Stream> {
        #[pin]
        inner:S,
        next_payload: VecDeque<Payload<S::Item>>,
        next_res:Option<S::Item>,
        next_send: Option<Response>,
        closed:Option<HTTPPollError>
    }
}

#[derive(Copy, Clone, Debug)]
pub enum HTTPPollError {
    RemoteClose,
    LocalClose,
    AlreadyClosed,
    PollingError,
}

impl<S> Stream for HTTPPoll<S>
where
    S: Stream<Item = ForwardedReq<Body>>,
{
    type Item = Result<Payload<ForwardedReq<Body>>, HTTPPollError>;

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

impl<S> HTTPPoll<S>
where
    S: Stream<Item = ForwardedReq<Body>>,
{
    fn close(self: Pin<&mut Self>, err: HTTPPollError) -> HTTPPollError {
        // close existing polling responses
        let this = self.project();
        if let Some((_, polling_callback)) = this.next_res.take() {
            let _ = polling_callback.send(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::empty())
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
                                    .body(Body::empty())
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
                            .push_back(Payload::Msg((req, res)));
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
                                .body(Body::empty())
                                .unwrap(),
                        );
                    }
                },
            }
        }
    }
}

impl Sink<Response> for HTTPPoll<ReqStream<Body>> {
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

    fn start_send(self: Pin<&mut Self>, item: Response) -> Result<(), Self::Error> {
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
    pub struct Framer<Si> {
        #[pin]
        inner: Si,
        buf: VecDeque<Bytes>,
        max_size:usize
    }
}

// delegate
impl<Si> Stream for Framer<Si>
where
    Si: Stream,
{
    type Item = Si::Item;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

impl<Si> Framer<Si>
where
    Si: Sink<Response>,
{
    fn poll_empty(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Si::Error>> {
        loop {
            if self.buf.len() == 0 {
                return Poll::Ready(Ok(()));
            }
            ready!(self.as_mut().project().inner.poll_ready(cx))?;
            let this = self.as_mut().project();

            let mut v = vec![];
            let mut cur_size = 0;
            loop {
                let Some(a) = this.buf.front() else { break };
                if cur_size + a.len() > *this.max_size {
                    break;
                };
                cur_size += a.len();
                v.push(this.buf.pop_front().unwrap());
            }

            let body = Body::from_stream(stream::iter(v.into_iter().map(Ok::<_, &str>)));
            if let Err(e) = this.inner.start_send(Response::new(body)) {
                return Poll::Ready(Err(e));
            }
        }
    }
}

impl<W> Sink<Bytes> for Framer<W>
where
    W: Sink<Response>,
{
    type Error = W::Error;
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.project().inner.poll_close(cx)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_empty(cx))?;
        // need to flush the last one too
        self.project().inner.poll_flush(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        self.project().buf.push_back(item);
        Ok(())
    }

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.buf.len() > self.max_size {
            self.poll_empty(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

pub type Session = Framer<HTTPPoll<ReqStream<Body>>>;

// ==========

type LongPollSender = mpsc::Sender<(Request<Body>, ResponseCallback<Body>)>;

#[derive(Clone, Debug)]
pub struct _Longpoll<K, M> {
    store: M,
    _key: PhantomData<K>,
}

use std::sync::RwLock;

#[derive(Clone, Debug)]
pub struct DefaultStorage<K>(Arc<RwLock<HashMap<K, LongPollSender>>>);

impl<K> Default for DefaultStorage<K> {
    fn default() -> Self {
        Self(RwLock::new(HashMap::new()).into())
    }
}

impl<K, M> Default for _Longpoll<K, M>
where
    M: Default,
{
    fn default() -> Self {
        Self {
            store: M::default(),
            _key: PhantomData,
        }
    }
}

impl<K, M> _Longpoll<K, M>
where
    M: Default,
    K: Eq + Hash + Send + Sync + 'static,
{
    pub fn new_layer() -> Extension<Self> {
        Extension(Self::default())
    }
}

impl<K> _Longpoll<K, DefaultStorage<K>>
where
    K: Eq + Hash,
{
    fn add(&self, k: K, capacity: usize) -> Result<ReqStream<Body>, ()> {
        todo!()
    }

    fn remove(&self, k: K) {
        todo!()
    }

    // TODO: Define Error !
    pub async fn forward(&self, k: K, req: Request) -> Result<Response, ()> {
        todo!()
    }

    pub fn new<C, Fut>(self, key: K, callback: C)
    where
        C: FnOnce(Session) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        todo!()
    }
}

impl<K, M, S> FromRequestParts<S> for _Longpoll<K, M>
where
    M: Clone + Sync + Send + 'static,
    K: Clone + Sync + Send + 'static,
    S: Send + Sync,
{
    type Rejection = ExtensionRejection;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        Extension::<Self>::from_request_parts(parts, state)
            .await
            .map(|a| a.0)
    }
}

pub type HTTPLongpoll<K> = _Longpoll<K, DefaultStorage<K>>;

#[cfg(test)]
mod tests {
    use crate::HTTPLongpoll;

    #[test]
    fn init() {
        let a = HTTPLongpoll::new_layer();
        a.add("test".to_string(), 10);
    }
}
