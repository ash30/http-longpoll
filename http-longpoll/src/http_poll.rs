use axum::http::StatusCode;
use axum::{extract::Request, response::Response};
use futures::future::BoxFuture;
use futures::{Future, FutureExt, Sink, Stream};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::sync::{mpsc, oneshot};

pub type ResponseCallback<U> = oneshot::Sender<Response<U>>;
pub type ForwardedReq<T> = (Request<T>, ResponseCallback<T>);

pin_project! {
    #[derive(Debug)]
    pub struct ForwardedReqChan<T> {
        rx: mpsc::Receiver<ForwardedReq<T>>,
    }
}

impl<T> ForwardedReqChan<T> {
    pub fn new(rx: mpsc::Receiver<ForwardedReq<T>>) -> Self {
        Self { rx }
    }
}

impl<T> ForwardedReqChan<T> {
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }
    pub fn close(&mut self) {
        self.rx.close()
    }
}

impl<T> Stream for ForwardedReqChan<T> {
    type Item = ForwardedReq<T>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_closed() {
            Poll::Ready(None)
        } else {
            self.project().rx.poll_recv(cx)
        }
    }
}

// ======

pin_project! {
    pub struct Writer<S,B> {
        #[pin]
        stream:S,
        next_send: Option<Response<B>>,
        next_res:Option<ForwardedReq<B>>,
    }
}

impl<S, B> Writer<S, B> {
    pub fn new(stream: S) -> Self {
        Self {
            next_res: None,
            next_send: None,
            stream,
        }
    }
}

enum SenderError {
    MultiplePoll,
}
#[derive(Copy, Clone, Debug)]
pub enum HTTPPollError {
    RemoteClose,
    LocalClose,
    AlreadyClosed,
    PollingError,
}

impl<S, B> Writer<S, B>
where
    S: Stream<Item = ForwardedReq<B>>,
{
    fn poll_inner(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), SenderError>> {
        let n = ready!(self.as_mut().project().stream.poll_next(cx))?;
        self.as_mut().next_res.replace(n);
        Ok(());
    }
}

impl<S, B> Sink<Response<B>> for Writer<S, B>
where
    S: Stream<Item = ForwardedReq<B>>,
{
    type Error = SenderError;

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        //self.close(HTTPPollError::LocalClose);
        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            if self.next_send.is_none() {
                return Poll::Ready(Ok(()));
            }
            if let Some((_, callback)) = self.as_mut().project().next_res.take() {
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

    fn start_send(self: Pin<&mut Self>, item: Response<B>) -> Result<(), Self::Error> {
        if self.next_send.is_some() {
            todo!()
            // Err(todo!());
        } else {
            self.project().next_send.replace(item);
            Ok(())
        }
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        // always poll source to check for multiple
        // req error
        self.as_mut().poll_inner(cx)?;
        if self.next_res.is_some() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
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

pin_project! {
    pub struct ResponseFramer<E,S,B> where  E:IntoPollResponse<B>  {
        #[pin]
        inner: S,
        buf: VecDeque<E::Buffered>,
        max_size:usize,
    }
}

impl<E, S, B> ResponseFramer<E, S, B>
where
    E: IntoPollResponse<B>,
{
    pub fn new(max_size: usize, inner: S) -> Self {
        Self {
            buf: VecDeque::new(),
            max_size,
            inner,
        }
    }
}

impl<E, S, B> ResponseFramer<E, S, B>
where
    E: IntoPollResponse<B>,
    S: Sink<Response<B>>,
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
    InvalidMessage(E),
    SendLimit,
    Polling(HTTPPollError),
}

impl<E> From<HTTPPollError> for FrameError<E> {
    fn from(value: HTTPPollError) -> Self {
        FrameError::Polling(value)
    }
}

impl<E, S, B> Sink<E> for ResponseFramer<E, S, B>
where
    S: Sink<Response<B>>,
    E: IntoPollResponse<B>,
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
        let n: E::Buffered = item.try_into().map_err(FrameError::InvalidMessage)?;
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
// =============

pub trait FromPollRequest<T>: Sized {
    type Error;
    fn from_poll_req(
        req: Request<T>,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send + 'static;
}

pin_project! {
    pub struct PollRequestExtractor<E:FromPollRequest<B>,S,B> {
        #[pin]
        inner: S,
        next: Option<(BoxFuture<'static,Result<E,E::Error>>, ResponseCallback<B>)>,
    }
}

impl<E, S, B> PollRequestExtractor<E, S, B>
where
    E: FromPollRequest<B>,
{
    pub fn new(inner: S) -> Self {
        Self { next: None, inner }
    }
}

impl<E, S, B> Stream for PollRequestExtractor<E, S, B>
where
    S: Stream<Item = ForwardedReq<B>>,
    E: FromPollRequest<B>,
    B: From<()>,
{
    type Item = Result<E, HTTPPollError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(loop {
            let this = self.as_mut().project();
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
            } else if let Some((req, res)) = ready!(this.inner.poll_next(cx)) {
                // get next extractor fut
                let _ = this.next.replace((E::from_poll_req(req).boxed(), res));
            } else {
                // inner has finished
                break None;
            }
        })
    }
}

// =====
