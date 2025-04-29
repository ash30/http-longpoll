use axum::http::StatusCode;
use axum::{extract::Request, response::Response};
use futures::future::BoxFuture;
use futures::{Future, FutureExt, Sink, Stream};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::sync::oneshot::Sender;
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
        state: WriterState<B>
    }
}

enum WriterState<B> {
    Open {
        next: Option<Response<B>>,
        out: Option<ForwardedReq<B>>,
    },
    Closed(WriterError),
}

// We cannot move stream once its pinned!
// so to close it...
//
// We will need to KNOW S, to call close

impl<S, B> Writer<S, B> {
    pub fn new(stream: S) -> Self {
        Self {
            state: WriterState::Open {
                next: None,
                out: None,
            },
            stream,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum WriterError {
    ClientClose,
    ServerClose,
    PollingError,
}

macro_rules! is_open {
    ($e:expr) => {
        match $e {
            WriterState::Closed(ref r) => return Poll::Ready(Err(*r)),
            WriterState::Open {
                ref mut next,
                ref mut out,
            } => (next, out),
        }
    };
}
impl<S, B> Writer<S, B>
where
    S: Stream<Item = ForwardedReq<B>>,
    B: From<()>,
{
    // The idea is to only poll inner when no current poll or current poll + no data
    fn poll_inner(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WriterError>> {
        let this = self.project();
        let (_, out) = is_open!(this.state);

        match ready!(this.stream.poll_next(cx)) {
            Some(new_poll) => {
                // IF already polling, fatal error
                if out.is_some() {
                    let current_req = out.take().unwrap();
                    let close = |sender: Sender<Response<B>>| {
                        let _ = sender.send(
                            Response::builder()
                                .status(StatusCode::BAD_REQUEST)
                                .body(().into())
                                .unwrap(),
                        );
                    };
                    close(current_req.1);
                    close(new_poll.1);
                    *(this.state) = WriterState::Closed(WriterError::PollingError);
                    Poll::Ready(Err(WriterError::PollingError))
                } else {
                    out.replace(new_poll);
                    Poll::Ready(Ok(()))
                }
            }
            None => {
                *(this.state) = WriterState::Closed(WriterError::ClientClose);
                Poll::Ready(Err(WriterError::ClientClose))
            }
        }
    }
}

impl<S, B> Sink<Response<B>> for Writer<S, B>
where
    S: Stream<Item = ForwardedReq<B>>,
    B: From<()>,
{
    type Error = WriterError;

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        *(self.project().state) = WriterState::Closed(WriterError::ServerClose);
        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            let (next, out) = is_open!(self.as_mut().project().state);
            if next.is_none() {
                return Poll::Ready(Ok(()));
            }
            if let Some((_, callback)) = out.take() {
                if let Err(_) = callback.send(next.take().unwrap()) {
                    // Could not return response to poll request...
                    // assume worst and tear down
                    return Poll::Ready(Err(WriterError::PollingError));
                }
            } else {
                ready!(self.as_mut().poll_inner(cx))?;
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Response<B>) -> Result<(), Self::Error> {
        match self.project().state {
            WriterState::Closed(ref r) => return Err(*r),
            WriterState::Open { ref mut next, .. } => {
                next.replace(item);
            }
        }
        Ok(())
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // poll before checking 'out' to catch multiple req error from clients
        // but only if no current data, we want to ensure once data is given to the sink
        // it gets there OR has a way of being returned ....
        ready!(self.as_mut().poll_flush(cx))?;
        self.as_mut().poll_inner(cx)?;
        let (_, out) = is_open!(self.as_mut().project().state);
        if out.is_some() {
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
    type Buffered: TryFrom<Self, Error = Self::Err> + Len + Send;
    type Err: Error + 'static;
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
    fn poll_empty(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
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

pub enum ResponseFramerError<E> {
    SendError(E),
    InvalidMessage(Box<dyn Error>),
    SizeLimit,
}

impl<E> From<E> for ResponseFramerError<E> {
    fn from(value: E) -> Self {
        ResponseFramerError::SendError(value)
    }
}

impl<E, S, B> Sink<E> for ResponseFramer<E, S, B>
where
    S: Sink<Response<B>>,
    E: IntoPollResponse<B>,
{
    type Error = ResponseFramerError<S::Error>;

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
        let n: E::Buffered = item
            .try_into()
            .map_err(|e| ResponseFramerError::InvalidMessage(Box::new(e)))?;
        if n.len() > self.max_size {
            Err(ResponseFramerError::SizeLimit)
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
    type Item = Result<E, E::Error>;

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
                break Some(e);
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
//
#[cfg(test)]
mod tests {
    use futures::stream::empty;
    use futures::task::noop_waker_ref;
    use futures::{pin_mut, stream};
    use futures::{stream::pending, SinkExt};
    use futures_test::future::FutureTestExt;
    use futures_test::{assert_stream_done, assert_stream_next, assert_stream_pending};
    use std::task::{ready, Context, Poll};
    use tokio::sync::oneshot;
    use tokio_test::{assert_pending, assert_ready_err, assert_ready_ok};

    use crate::http_poll::WriterError;

    use super::{ForwardedReq, Writer};

    fn poll_req() -> ForwardedReq<()> {
        let (tx, rx) = oneshot::channel();
        (http::Request::builder().body(()).unwrap(), tx)
    }

    #[test]
    fn writer_ready_no_poll_should_be_pending() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut sut = Writer::<_, ()>::new(pending::<ForwardedReq<()>>());
        let result = sut.poll_ready_unpin(&mut cx);
        assert_pending!(result)
    }

    #[test]
    fn writer_ready() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let inner = stream::iter(vec![poll_req()]);
        let mut sut = Writer::<_, ()>::new(inner);
        let result = sut.poll_ready_unpin(&mut cx);
        assert_ready_ok!(result)
    }

    #[test]
    fn writer_client_close() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut sut = Writer::<_, ()>::new(empty::<ForwardedReq<()>>());
        let result = sut.poll_ready_unpin(&mut cx);
        let result = assert_ready_err!(result);
        assert!(matches!(result, WriterError::ClientClose));
    }

    #[test]
    fn writer_double_poll() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let inner = stream::iter(vec![poll_req(), poll_req()]);
        let mut sut = Writer::<_, ()>::new(inner);
        let _ = sut.poll_ready_unpin(&mut cx);
        let result = sut.poll_ready_unpin(&mut cx);
        let result = assert_ready_err!(result);
        assert!(
            matches!(result, WriterError::PollingError),
            "got {:?}",
            result
        );
    }
}
