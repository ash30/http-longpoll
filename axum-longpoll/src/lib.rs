#![feature(trace_macros)]

//trace_macros!(true);

use axum::body::{Body, Bytes};
use axum::response::IntoResponse;
use axum::Extension;
use axum::{extract::Request, response::Response};
use futures::{stream, Sink, Stream, StreamExt};
use pin_project_lite::pin_project;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::sync::{mpsc, oneshot};
use tower::{Layer, Service};

type ResponseCallback = oneshot::Sender<Response>;

pin_project! {
    #[derive(Debug)]
    struct HTTPSession {
        rx: mpsc::Receiver<(Request, ResponseCallback)>,
    }
}

impl HTTPSession {
    fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }
    fn close(&mut self) {
        self.rx.close()
    }
}

impl Stream for HTTPSession {
    type Item = (Request, oneshot::Sender<Response>);
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.rx.poll_recv(cx)
    }
}

// Out needs to be Into Response, but really, we've lost http semantics at this point
pin_project! {
    pub struct HTTPPollingSession<T:IntoResponse> {
        #[pin]
        inner:HTTPSession,
        next:Option<ResponseCallback>,
        in_buf: VecDeque<Request>,
        out: Option<T>,
    }
}

impl<T> HTTPPollingSession<T>
where
    T: IntoResponse,
{
    fn poll_loop(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), axum::Error>> {
        todo!()
    }
}

impl<T> Stream for HTTPPollingSession<T>
where
    T: IntoResponse,
{
    type Item = Request;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.inner.is_closed() {
            Poll::Ready(None)
        } else {
            self.as_mut().poll_loop(cx);
            let this = self.project();
            this.in_buf
                .pop_front()
                .map(Some)
                .map(Poll::Ready)
                .unwrap_or(Poll::Pending)
        }
    }
}

impl<T> Sink<T> for HTTPPollingSession<T>
where
    T: IntoResponse,
{
    type Error = axum::Error;

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.close();
        self.poll_loop(cx)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            if self.out.is_none() {
                return Poll::Ready(Ok(()));
            }
            if let Some(callback) = self.next.take() {
                if let Err(_) =
                    callback.send(self.as_mut().project().out.take().unwrap().into_response())
                {
                    return Poll::Ready(Err(todo!()));
                }
            } else {
                ready!(self.as_mut().poll_loop(cx))?;
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        if self.out.is_some() {
            Err(todo!());
        } else {
            self.project().out.replace(item);
            Ok(())
        }
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            if self.out.is_some() {
                ready!(self.as_mut().poll_flush(cx))?;
            }
            if self.next.is_some() {
                return Poll::Ready(Ok(()));
            }
            ready!(self.as_mut().poll_loop(cx))?;
        }
    }
}

pin_project! {
    struct Framer<W> {
        #[pin]
        inner: W,
        buf: VecDeque<Bytes>,
        max_size:usize
    }
}

// delegate
impl<W> Stream for Framer<W>
where
    W: Stream,
{
    type Item = W::Item;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

impl<W> Framer<W>
where
    W: Sink<Response>,
{
    fn poll_empty(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), W::Error>> {
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

            if let Err(e) = this
                .inner
                .start_send(Response::new(Body::from_stream(stream::iter(
                    v.into_iter().map(|a| Ok::<_, &str>(a)),
                ))))
            {
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
        self.as_mut().poll_empty(cx);
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

// ==========

type LongpollTX = mpsc::Sender<(Request, ResponseCallback)>;

pub struct _Longpoll<K, M> {
    store: M,
    _key: PhantomData<K>,
}

use std::sync::RwLock;
struct DefaultStorage<K>(Arc<RwLock<HashMap<K, LongpollTX>>>);
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
    fn add(&self, k: K, capacity: usize) -> Result<HTTPSession, ()> {
        todo!()
    }

    fn remove(&self, k: K) {
        todo!()
    }

    // TODO: Define Error !
    async fn forward(&self, k: K, req: Request) -> Result<Response, ()> {
        todo!()
    }
}

pub type HTTPLongpoll<K = String> = _Longpoll<K, DefaultStorage<K>>;

#[cfg(test)]
mod tests {
    use crate::HTTPLongpoll;

    #[test]
    fn init() {
        let a = HTTPLongpoll::new_layer();
        a.add("test".to_string(), 10);
    }
}
