use futures::{Sink, Stream};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::sync::oneshot;

pub type Callback<T> = oneshot::Sender<T>;
pub type Result<T> = std::result::Result<T, WriterError>;
pub type ResultCallback<T> = Callback<Result<T>>;

// Trait to simplify Generics in consumer structs
pub(crate) trait ResponseStream: Stream<Item = Callback<Self::Response>> {
    type Response;
}

impl<T, U> ResponseStream for T
where
    T: Stream<Item = Callback<U>>,
{
    type Response = U;
}

pin_project! {
    pub struct Writer<S> where S:ResponseStream{
        #[pin]
        stream:S,
        state: WriterState<S::Response>
    }
}

enum WriterState<T> {
    Open {
        buf: Option<T>,
        out: Option<Callback<T>>,
    },
    Closed(WriterError),
}

impl<S> Writer<S>
where
    S: ResponseStream,
{
    pub fn new(stream: S) -> Self {
        Self {
            state: WriterState::Open {
                buf: None,
                out: None,
            },
            stream,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum WriterError {
    PollingError,
    Closed,
}

macro_rules! is_open {
    ($e:expr) => {
        match $e {
            WriterState::Closed(ref r) => return Poll::Ready(Err(*r)),
            WriterState::Open {
                ref mut buf,
                ref mut out,
            } => (buf, out),
        }
    };
}
impl<S, T> Writer<S>
where
    S: ResponseStream<Response = Result<T>>,
{
    // The idea is to only poll inner when no current poll or current poll + no data
    fn poll_inner(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.project();
        let (_, out) = is_open!(this.state);

        match ready!(this.stream.poll_next(cx)) {
            Some(new_poll) => {
                // IF already polling, fatal error
                if out.is_some() {
                    let current_req = out.take().unwrap();
                    let _ = current_req.send(Err(WriterError::PollingError));
                    let _ = new_poll.send(Err(WriterError::PollingError));
                    *(this.state) = WriterState::Closed(WriterError::PollingError);
                    Poll::Ready(Err(WriterError::PollingError))
                } else {
                    out.replace(new_poll);
                    Poll::Ready(Ok(()))
                }
            }
            None => {
                *(this.state) = WriterState::Closed(WriterError::Closed);
                Poll::Ready(Err(WriterError::Closed))
            }
        }
    }
}

// The main interface of Writer is to provide a SINK over long poll connection
impl<S, T> Sink<T> for Writer<S>
where
    S: ResponseStream<Response = Result<T>>,
{
    type Error = WriterError;

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // Flush remaining data before closing
        ready!(self.as_mut().poll_flush(cx))?;

        // explicitly drop 'out' request incase
        let (next, out) = is_open!(self.as_mut().project().state);
        let _ = out.take();

        *(self.project().state) = WriterState::Closed(WriterError::Closed);
        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        loop {
            let (next, out) = is_open!(self.as_mut().project().state);
            if next.is_none() {
                return Poll::Ready(Ok(()));
            }
            if let Some(callback) = out.take() {
                if let Err(_) = callback.send(
                    next.take()
                        .ok_or(WriterError::PollingError)
                        // We assume next is always Ok
                        .map(|r| r.unwrap()),
                ) {
                    // Could not return response to poll request...
                    // assume worst and tear down
                    return Poll::Ready(Err(WriterError::PollingError));
                }
            } else {
                ready!(self.as_mut().poll_inner(cx))?;
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<()> {
        match self.project().state {
            WriterState::Closed(ref r) => return Err(*r),
            WriterState::Open {
                buf: ref mut next, ..
            } => {
                next.replace(Ok(item));
            }
        }
        Ok(())
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // poll before checking 'out' to catch multiple req error from clients
        // but only if no current data, we want to ensure once data is given to the sink
        // it gets there OR has a way of being returned ....
        ready!(self.as_mut().poll_flush(cx))?;
        let _ = self.as_mut().poll_inner(cx)?;
        let (_, out) = is_open!(self.as_mut().project().state);
        if out.is_some() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

// =================
pub trait Appendable {
    fn len(&self) -> usize;
    fn append(&mut self, next: Self);

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pin_project! {
    pub struct ResponseFramer<S,T>  {
        #[pin]
        inner: S,
        buf: VecDeque<T>,
        max_size:usize,
    }
}

impl<S, T> ResponseFramer<S, T> {
    pub fn new(max_size: usize, inner: S) -> Self {
        Self {
            buf: VecDeque::new(),
            max_size,
            inner,
        }
    }
}

impl<S, T> ResponseFramer<S, T>
where
    T: Appendable,
    S: Sink<T>,
{
    fn poll_empty(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), S::Error>> {
        loop {
            if self.buf.is_empty() {
                return Poll::Ready(Ok(()));
            }
            ready!(self.as_mut().project().inner.poll_ready(cx))?;
            let this = self.as_mut().project();
            let mut value = this.buf.pop_front().unwrap();
            loop {
                if this.buf.is_empty() {
                    break;
                }
                if value.len() + this.buf.front().unwrap().len() > *this.max_size {
                    break;
                }
                value.append(this.buf.pop_front().unwrap())
            }
            if let Err(e) = this.inner.start_send(value) {
                return Poll::Ready(Err(e));
            };
        }
    }
}

impl<S, T> Sink<T> for ResponseFramer<S, T>
where
    S: Sink<T>,
    T: Appendable,
{
    type Error = S::Error;

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.project().inner.poll_close(cx)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        // need to flush the last one too
        ready!(self.as_mut().poll_empty(cx))?;
        self.project().inner.poll_flush(cx)
    }

    // We don't inforce maxsize on individual items, left to calling code to enforce if they care
    fn start_send(self: Pin<&mut Self>, item: T) -> std::result::Result<(), Self::Error> {
        self.project().buf.push_back(item);
        Ok(())
    }

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        if self.buf.len() > self.max_size {
            self.poll_empty(cx).map_err(|e| e.into())
        } else {
            Poll::Ready(Ok(()))
        }
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
    use std::task::{ready, Context, Poll};
    use tokio::sync::oneshot;
    use tokio_test::{assert_ok, assert_pending, assert_ready_err, assert_ready_ok};

    use super::{Callback, Writer, WriterError};
    type TestResponse = Callback<Result<Option<()>, WriterError>>;

    macro_rules! test_callback {
        () => {
            oneshot::channel::<Result<Option<()>, WriterError>>()
        };
    }

    #[test]
    fn writer_ready_no_poll_should_be_pending() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut sut = Writer::new(pending::<TestResponse>());

        // return Pending when no poll req
        let result = sut.poll_ready_unpin(&mut cx);
        assert_pending!(result)
    }

    #[test]
    fn writer_ready() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let p1 = test_callback!();
        let inner = stream::iter(vec![p1.0]);

        // should be ready
        let mut sut = Writer::new(inner);
        let result = sut.poll_ready_unpin(&mut cx);
        assert_ready_ok!(result)
    }

    #[test]
    fn writer_ready_double_poll() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let p1 = test_callback!();
        let p2 = test_callback!();
        let inner = stream::iter(vec![p1.0, p2.0]);

        let mut sut = Writer::new(inner);

        // Polling x2 will result in polling error
        let _ = sut.poll_ready_unpin(&mut cx);
        let result = sut.poll_ready_unpin(&mut cx);
        let result = assert_ready_err!(result);
        assert!(
            matches!(result, WriterError::PollingError),
            "got {:?}",
            result
        );
    }

    #[test]
    fn writer_ready_double_poll_with_data() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let p1 = test_callback!();
        let p2 = test_callback!();
        let inner = stream::iter(vec![p1.0, p2.0]);

        let mut sut = Writer::new(inner);
        assert_ready_ok!(sut.poll_ready_unpin(&mut cx));

        let send_result = sut.start_send_unpin(Some(()));
        assert_ok!(send_result, "send data");

        let result = sut.poll_ready_unpin(&mut cx);
        let _ = assert_ready_ok!(result);
    }

    #[test]
    fn writer_client_stream_close() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut sut = Writer::new(empty::<TestResponse>());
        let result = sut.poll_ready_unpin(&mut cx);
        let result = assert_ready_err!(result);
        assert!(matches!(result, WriterError::Closed));
    }

    #[test]
    fn writer_close_with_pending_data() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut p1 = test_callback!();
        let inner = stream::iter(vec![p1.0]);

        let mut sut = Writer::new(inner);

        // Send data but don't flush
        assert_ready_ok!(sut.poll_ready_unpin(&mut cx));
        assert_ok!(sut.start_send_unpin(Some(())), "send data");

        // Close should try to flush pending data first
        let close_result = sut.poll_close_unpin(&mut cx);
        assert_ready_ok!(close_result);

        // Verify data was sent through callback
        let received = assert_ok!(p1.1.try_recv());
        assert_ok!(received, "callback received data");
    }

    #[test]
    fn writer_close_during_active_poll() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut p1 = test_callback!();
        let inner = stream::iter(vec![p1.0]);

        let mut sut = Writer::new(inner);

        // Get ready with active poll
        assert_ready_ok!(sut.poll_ready_unpin(&mut cx));

        // Close with active poll
        let close_result = sut.poll_close_unpin(&mut cx);
        assert_ready_ok!(close_result);

        // Verify callback got closed error
        let received = assert_ok!(p1.1.try_recv());
        assert!(matches!(received, Err(WriterError::Closed)));
    }

    #[test]
    fn writer_send_after_close() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut sut = Writer::new(empty::<TestResponse>());

        // Close the writer
        let close_result = sut.poll_close_unpin(&mut cx);
        assert_ready_ok!(close_result);

        // Attempt operations after close
        let ready_result = sut.poll_ready_unpin(&mut cx);
        assert!(matches!(
            assert_ready_err!(ready_result),
            WriterError::Closed
        ));

        let send_result = sut.start_send_unpin(Some(()));
        assert!(matches!(send_result.unwrap_err(), WriterError::Closed));
    }

    #[test]
    fn writer_double_close() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut sut = Writer::new(empty::<TestResponse>());

        // First close
        let first_close = sut.poll_close_unpin(&mut cx);
        assert_ready_ok!(first_close);

        // Second close should return closed error
        let second_close = sut.poll_close_unpin(&mut cx);
        assert!(matches!(
            assert_ready_err!(second_close),
            WriterError::Closed
        ));
    }
}
