use futures::{Future, Sink, Stream};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{ready, Context, Poll, Waker};
use std::time::{self, Duration, Instant};
use tokio::sync::oneshot;
use tokio::time::Sleep;

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
        timer: Sleep,
        #[pin]
        stream:S,
        state: WriterState<S::Response>,
    }
}

enum WriterState<T> {
    Open {
        since: Instant,
        buf: Option<T>,
        out: Option<Callback<T>>,
    },
    Waiting {
        since: Instant,
        waker: Option<Waker>,
    },
    Closed,
}

enum WriterEvent {
    Closed(Result<()>),
    Wait(Option<Instant>),
}
impl From<Result<()>> for WriterEvent {
    fn from(value: Result<()>) -> Self {
        Self::Closed(value)
    }
}
impl From<Option<Instant>> for WriterEvent {
    fn from(value: Option<Instant>) -> Self {
        Self::Wait(value)
    }
}

impl<T> WriterState<T> {
    fn new(now: Instant) -> Self {
        Self::Waiting {
            since: now,
            waker: None,
        }
    }
}
impl<T> WriterState<Result<T>> {
    fn close(&mut self) {
        match self {
            Self::Closed => {}
            Self::Waiting { waker, .. } => {
                if let Some(w) = waker.take() {
                    w.wake_by_ref()
                }
                *(self) = WriterState::Closed;
            }
            Self::Open { out, .. } => {
                let current_req = out.take().unwrap();
                let _ = current_req.send(Err(WriterError::Closed));
                *(self) = WriterState::Closed;
            }
        }
    }

    fn do_io(
        &mut self,
        item: Poll<Option<Callback<Result<T>>>>,
        now: time::Instant,
    ) -> WriterEvent {
        match self {
            Self::Open { since, buf, out } => {
                match item {
                    Poll::Pending => {
                        // TODO: CONFIG PLEASE
                        let since = *since;
                        if now > since + Duration::from_secs(60) && buf.is_none() {
                            let current_req = out.take().unwrap();
                            *(self) = WriterState::Waiting {
                                since: now,
                                waker: None,
                            };
                            if let Err(_) = current_req.send(Err(WriterError::PollingTimeout)) {
                                *(self) = WriterState::Closed;
                                return Err(WriterError::PollingError).into();
                            }
                        }
                        WriterEvent::Wait(Some(since + Duration::from_secs(60)))
                    }
                    Poll::Ready(Some(req)) => {
                        // Error !
                        let current_req = out.take().unwrap();
                        let _ = current_req.send(Err(WriterError::PollingError));
                        let _ = req.send(Err(WriterError::PollingError));
                        Err(WriterError::PollingError).into()
                    }
                    Poll::Ready(None) => {
                        *(self) = WriterState::Closed;
                        Ok(()).into()
                    }
                }
            }
            Self::Waiting { since, waker } => match item {
                Poll::Pending => {
                    // Close if we haven't been polled for a while
                    if now > *since + Duration::from_secs(60) {
                        if let Some(w) = waker.take() {
                            w.wake()
                        }
                        return Err(WriterError::PollingError).into();
                    } else {
                        WriterEvent::Wait(Some(*since + Duration::from_secs(60)))
                    }
                }
                Poll::Ready(Some(req)) => {
                    if let Some(w) = waker.take() {
                        w.wake()
                    }
                    *(self) = WriterState::Open {
                        since: now,
                        buf: None,
                        out: Some(req),
                    };
                    WriterEvent::Wait(None)
                }
                Poll::Ready(None) => {
                    if let Some(w) = waker.take() {
                        w.wake()
                    }
                    *(self) = WriterState::Closed;
                    Ok(()).into()
                }
            },
            Self::Closed => Ok(()).into(),
        }
    }
}

impl<S> Writer<S>
where
    S: ResponseStream,
{
    pub fn new(stream: S) -> Self {
        Self {
            state: WriterState::<S::Response>::new(Instant::now()),
            stream,
            timer: tokio::time::sleep(Duration::default()),
        }
    }
}

// SHOULD WE SPLIT THIS? ONE FOR FUT, another for SINK
#[derive(Copy, Clone, Debug)]
pub enum WriterError {
    Closed,
    PollingError,
    PollingTimeout,
}

// SO WRITER needs a closed state because :
// we need to signal to writer interface, potentially in separate task
// POLL errors wouldn't close the channel
// ALSO by only accepting streams, we don't have access to close method
// NOW we can actually close things!

// The futre returns §
//
// WE keep the state design because SINK interface won't poll stream directly
// and needs to know if its closed already?
//

impl<S, T> Future for Writer<S>
where
    S: ResponseStream<Response = Result<T>>,
{
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let this = self.as_mut().project();
            match this.state.do_io(this.stream.poll_next(cx), Instant::now()) {
                WriterEvent::Closed(res) => return Poll::Ready(res),
                WriterEvent::Wait(Some(deadline)) => {
                    this.timer.reset(deadline.into());
                    self.as_mut().project().timer.poll(cx);
                    return Poll::Pending;
                }
                WriterEvent::Wait(None) => continue,
            }
        }
    }
}

// The main interface of Writer is to provide a SINK over long poll connection
//
impl<S, T> Sink<T> for Writer<S>
where
    S: ResponseStream<Response = Result<T>>,
{
    type Error = WriterError;

    // We are ready when we have an active polling req waiting
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        match self.as_mut().project().state {
            WriterState::Closed => Poll::Ready(Err(WriterError::Closed)),
            WriterState::Waiting { waker, .. } => {
                waker.replace(cx.waker().clone());
                Poll::Pending
            }
            _ => Poll::Ready(Ok(())),
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<()> {
        match self.as_mut().project().state {
            WriterState::Closed => Err(WriterError::Closed),
            WriterState::Waiting { waker, .. } => Err(WriterError::PollingError),
            WriterState::Open { ref mut buf, .. } => {
                // we silently overwrite here...
                buf.replace(Ok(item));
                Ok(())
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.as_mut().project().state.close();
        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match self.as_mut().project().state {
            WriterState::Closed => return Poll::Ready(Err(WriterError::Closed)),
            WriterState::Waiting { waker, .. } => {
                waker.replace(cx.waker().clone());
                return Poll::Pending;
            }
            WriterState::Open {
                ref mut buf,
                ref mut out,
                ..
            } => match buf.take() {
                None => Poll::Ready(Ok(())),
                Some(next) => {
                    let callback = out.take().unwrap();
                    if let Err(_) = callback.send(next) {
                        return Poll::Ready(Err(WriterError::PollingError));
                    }
                    return Poll::Ready(Ok(()));
                }
            },
        }
    }
}

// =================
pub type TotalSize = usize;

pub trait Foldable: Default {
    type Start: Default;
    fn append(current: &mut Self::Start, value: Self) -> TotalSize;
}

pin_project! {
    pub struct ResponseFramer<S,T>
    {
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

impl<S, T> Future for ResponseFramer<S, T>
where
    S: Future,
{
    type Output = S::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

impl<S, T> ResponseFramer<S, T>
where
    T: Foldable,
    S: Sink<T::Start>,
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
            let mut start = T::Start::default();
            loop {
                if this.buf.is_empty() {
                    break;
                }
                let value = this.buf.pop_front().unwrap();
                if T::append(&mut start, value) > *this.max_size {
                    break;
                }
            }
            if let Err(e) = this.inner.start_send(start) {
                return Poll::Ready(Err(e));
            };
        }
    }
}

impl<S, T> Sink<T> for ResponseFramer<S, T>
where
    S: Sink<T::Start>,
    T: Foldable,
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
mod tests_writer {
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
            oneshot::channel::<std::result::Result<Option<()>, WriterError>>()
        };
    }

    //    #[test]
    //    fn writer_ready_no_poll_should_be_pending() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let mut sut = Writer::new(pending::<TestResponse>());
    //
    //        // return Pending when no poll req
    //        let result = sut.poll_ready_unpin(&mut cx);
    //        assert_pending!(result)
    //    }
    //
    //    #[test]
    //    fn writer_ready() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let p1 = test_callback!();
    //        let inner = stream::iter(vec![p1.0]);
    //
    //        // should be ready
    //        let mut sut = Writer::new(inner);
    //        let result = sut.poll_ready_unpin(&mut cx);
    //        assert_ready_ok!(result)
    //    }
    //
    //    #[test]
    //    fn writer_ready_double_poll() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let p1 = test_callback!();
    //        let p2 = test_callback!();
    //        let inner = stream::iter(vec![p1.0, p2.0]);
    //
    //        let mut sut = Writer::new(inner);
    //
    //        // Polling x2 will result in polling error
    //        let _ = sut.poll_ready_unpin(&mut cx);
    //        let result = sut.poll_ready_unpin(&mut cx);
    //        let result = assert_ready_err!(result);
    //        assert!(
    //            matches!(result, WriterError::PollingError),
    //            "got {:?}",
    //            result
    //        );
    //    }
    //
    //    #[test]
    //    fn writer_ready_double_poll_with_data() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let p1 = test_callback!();
    //        let p2 = test_callback!();
    //        let inner = stream::iter(vec![p1.0, p2.0]);
    //
    //        let mut sut = Writer::new(inner);
    //        assert_ready_ok!(sut.poll_ready_unpin(&mut cx));
    //
    //        let send_result = sut.start_send_unpin(Some(()));
    //        assert_ok!(send_result, "send data");
    //
    //        let result = sut.poll_ready_unpin(&mut cx);
    //        let _ = assert_ready_ok!(result);
    //    }
    //
    //    #[test]
    //    fn writer_client_stream_close() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let mut sut = Writer::new(empty::<TestResponse>());
    //        let result = sut.poll_ready_unpin(&mut cx);
    //        let result = assert_ready_err!(result);
    //        assert!(matches!(result, WriterError::Closed));
    //    }
    //
    //    #[test]
    //    fn writer_close_with_pending_data() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let mut p1 = test_callback!();
    //        let inner = stream::iter(vec![p1.0]);
    //
    //        let mut sut = Writer::new(inner);
    //
    //        // Send data but don't flush
    //        assert_ready_ok!(sut.poll_ready_unpin(&mut cx));
    //        assert_ok!(sut.start_send_unpin(Some(())), "send data");
    //
    //        // Close should try to flush pending data first
    //        let close_result = sut.poll_close_unpin(&mut cx);
    //        assert_ready_ok!(close_result);
    //
    //        // Verify data was sent through callback
    //        let received = assert_ok!(p1.1.try_recv());
    //        assert_ok!(received, "callback received data");
    //    }
    //
    //    #[test]
    //    fn writer_close_during_active_poll() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let mut p1 = test_callback!();
    //        let inner = stream::iter(vec![p1.0]);
    //
    //        let mut sut = Writer::new(inner);
    //
    //        // Get ready with active poll
    //        assert_ready_ok!(sut.poll_ready_unpin(&mut cx));
    //
    //        // Close with active poll
    //        let close_result = sut.poll_close_unpin(&mut cx);
    //        assert_ready_ok!(close_result);
    //
    //        // Verify callback got closed error
    //        let received = assert_ok!(p1.1.try_recv());
    //        assert!(matches!(received, Err(WriterError::Closed)));
    //    }
    //
    //    #[test]
    //    fn writer_send_after_close() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let mut sut = Writer::new(empty::<TestResponse>());
    //
    //        // Close the writer
    //        let close_result = sut.poll_close_unpin(&mut cx);
    //        assert_ready_ok!(close_result);
    //
    //        // Attempt operations after close
    //        let ready_result = sut.poll_ready_unpin(&mut cx);
    //        assert!(matches!(
    //            assert_ready_err!(ready_result),
    //            WriterError::Closed
    //        ));
    //
    //        let send_result = sut.start_send_unpin(Some(()));
    //        assert!(matches!(send_result.unwrap_err(), WriterError::Closed));
    //    }
    //
    //    #[test]
    //    fn writer_double_close() {
    //        let mut cx = Context::from_waker(noop_waker_ref());
    //        let mut sut = Writer::new(empty::<TestResponse>());
    //
    //        // First close
    //        let first_close = sut.poll_close_unpin(&mut cx);
    //        assert_ready_ok!(first_close);
    //
    //        // Second close should return closed error
    //        let second_close = sut.poll_close_unpin(&mut cx);
    //        assert!(matches!(
    //            assert_ready_err!(second_close),
    //            WriterError::Closed
    //        ));
    //    }
}

#[cfg(test)]
mod test_response_framer {
    use super::*;
    use futures::task::noop_waker_ref;
    use futures::{pin_mut, stream};
    use std::task::Context;
    use tokio::sync::oneshot;
    use tokio_stream::StreamExt;
    use tokio_test::{assert_ok, assert_pending, assert_ready_ok};

    #[derive(Debug, Default)]
    struct TestData(());

    impl Foldable for TestData {
        type Start = Self;

        fn append(current: &mut Self::Start, value: Self) -> TotalSize {
            0
        }
    }

    macro_rules! test_callback {
        () => {
            oneshot::channel::<std::result::Result<TestData, WriterError>>()
        };
    }
    type TestResponse = Callback<Result<TestData>>;
}
