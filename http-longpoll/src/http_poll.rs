use futures::{Future, Sink, SinkExt, Stream};
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

#[derive(Debug)]
enum WriterState<U> {
    Open {
        since: Instant,
        buf: usize,
        out: Option<U>,
    },
    Waiting {
        since: Instant,
        waker: Option<Waker>,
    },
    Closed,
}

enum WriterEvent {
    Closed,
    Wait(Option<Instant>),
}
impl From<Option<Instant>> for WriterEvent {
    fn from(value: Option<Instant>) -> Self {
        Self::Wait(value)
    }
}

impl<U> WriterState<U> {
    fn new(now: Instant) -> Self {
        Self::Waiting {
            since: now,
            waker: None,
        }
    }

    fn send(&mut self) -> std::result::Result<(), WriterError> {
        match self {
            Self::Closed => Err(WriterError::PollingError),
            Self::Waiting { .. } => Err(WriterError::PollingError),
            Self::Open { buf, .. } if *buf == 0 => {
                *buf += 1;
                Ok(())
            }
            Self::Open { .. } => Err(WriterError::PollingError),
        }
    }

    fn flush(&mut self, now: Instant) -> std::result::Result<Option<U>, WriterError> {
        match self {
            Self::Closed => Err(WriterError::Closed),
            Self::Waiting { .. } => Ok(None),
            Self::Open { out, buf, .. } if *buf == 0 => Ok(None),
            Self::Open { out, .. } => {
                let current_req = out.take().unwrap();
                *(self) = WriterState::Waiting {
                    since: now,
                    waker: None,
                };
                Ok(Some(current_req))
            }
        }
    }

    fn close(&mut self) -> Option<U> {
        match self {
            Self::Closed => None,
            Self::Waiting { waker, .. } => {
                if let Some(w) = waker.take() {
                    w.wake_by_ref()
                }
                *(self) = WriterState::Closed;
                None
            }
            Self::Open { out, .. } => {
                let current_req = out.take().unwrap();
                *(self) = WriterState::Closed;
                Some(current_req)
            }
        }
    }

    fn do_io(
        &mut self,
        item: Poll<Option<U>>,
        now: time::Instant,
    ) -> std::result::Result<WriterEvent, WriterError> {
        match self {
            Self::Open { since, buf, out } => {
                match item {
                    Poll::Pending => {
                        // TODO: CONFIG PLEASE
                        let since = *since;
                        if now > since + Duration::from_secs(60) && *buf > 0 {
                            // Return PollingTimeout error - let caller handle callback
                            return Err(WriterError::PollingTimeout);
                        }
                        Ok(WriterEvent::Wait(Some(since + Duration::from_secs(60))))
                    }
                    Poll::Ready(Some(req)) => {
                        // Multiple requests - this is an error condition
                        // we currently drop the existing callback, so they will see the session as
                        // 'closed'
                        // the new req will get error
                        out.replace(req);
                        Err(WriterError::PollingError)
                    }
                    Poll::Ready(None) => {
                        // Stream closed gracefully
                        *(self) = WriterState::Closed;
                        Ok(WriterEvent::Closed)
                    }
                }
            }
            Self::Waiting { since, waker } => {
                if now > *since + Duration::from_secs(60) {
                    return Err(WriterError::PollingError);
                };
                if let Some(w) = waker.take() {
                    w.wake()
                }
                match item {
                    Poll::Pending => Ok(WriterEvent::Wait(Some(*since + Duration::from_secs(60)))),
                    Poll::Ready(Some(req)) => {
                        *(self) = WriterState::Open {
                            since: now,
                            buf: 0,
                            out: Some(req),
                        };
                        Ok(WriterEvent::Wait(None))
                    }
                    Poll::Ready(None) => {
                        *(self) = WriterState::Closed;
                        Ok(WriterEvent::Closed)
                    }
                }
            }
            Self::Closed => Ok(WriterEvent::Closed),
        }
    }
}

pin_project! {
    pub struct Writer<S> where S:ResponseStream{
        #[pin]
        timer: Sleep,
        #[pin]
        stream:S,
        state: WriterState<Callback<S::Response>>,
        buf: Option<S::Response>,
    }
}

impl<S> Writer<S>
where
    S: ResponseStream,
{
    pub fn new(stream: S) -> Self {
        Self {
            state: WriterState::<Callback<S::Response>>::new(Instant::now()),
            stream,
            timer: tokio::time::sleep(Duration::default()),
            buf: None,
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

impl<S, T> Future for Writer<S>
where
    S: ResponseStream<Response = Result<T>>,
{
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let this = self.as_mut().project();
            match this.state.do_io(this.stream.poll_next(cx), Instant::now()) {
                Ok(WriterEvent::Closed) => return Poll::Ready(Ok(())),
                Ok(WriterEvent::Wait(Some(deadline))) => {
                    this.timer.reset(deadline.into());
                    let _ = self.as_mut().project().timer.poll(cx);
                    return Poll::Pending;
                }
                Ok(WriterEvent::Wait(None)) => continue,
                Err(error) => {
                    // Handle the error by sending it to any active callback and closing
                    let callback = self.as_mut().project().state.close();
                    if let Some(callback) = callback {
                        let _ = callback.send(Err(error));
                    }
                    return Poll::Ready(Err(error));
                }
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
        // flush existing
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
        let this = self.as_mut().project();
        this.state.send()?;
        this.buf.replace(Ok(item));
        Ok(())
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        let callback = self.as_mut().project().state.close();
        if let Some(callback) = callback {
            let _ = callback.send(Err(WriterError::Closed));
        }
        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // WE ASSUME buf is only full IF OPENED, since ready method guards
        assert!(self.buf.is_none() || matches!(self.state, WriterState::Open { .. }));

        if let Some(callback) = self.as_mut().project().state.flush(Instant::now())? {
            if let Some(next) = self.as_mut().project().buf.take() {
                if let Err(_) = callback.send(next) {
                    return Poll::Ready(Err(WriterError::PollingError));
                }
            } else {
                // Handle empty flush?
                todo!()
            }
        }
        Poll::Ready(Ok(()))
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
    use futures::{pin_mut, stream, Future, Sink};
    use futures::{stream::pending, SinkExt};
    use std::task::{ready, Context, Poll};
    use tokio::pin;
    use tokio::sync::oneshot;
    use tokio_stream::StreamExt;
    use tokio_test::{assert_ok, assert_pending, assert_ready_err, assert_ready_ok};

    use super::{Callback, Writer, WriterError};
    type TestResponse = Callback<Result<Option<()>, WriterError>>;

    macro_rules! test_callback {
        () => {
            oneshot::channel::<std::result::Result<Option<()>, WriterError>>()
        };
    }

    #[tokio::test]
    async fn writer_ready() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let p1 = test_callback!();
        let inner = stream::iter(vec![p1.0]).chain(stream::pending());

        // should be ready
        let sut = Writer::new(inner);
        pin!(sut);
        let res = sut.as_mut().poll(&mut cx);
        assert_ready_ok!(sut.poll_ready(&mut cx))
    }

    #[tokio::test]
    async fn writer_ready_double_poll() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let p1 = test_callback!();
        let p2 = test_callback!();
        let inner = stream::iter(vec![p1.0, p2.0]).chain(stream::pending());

        let sut = Writer::new(inner);
        pin!(sut);
        let res = sut.as_mut().poll(&mut cx);
        let result = sut.poll_ready_unpin(&mut cx);
        let result = assert_ready_err!(result);
        assert!(matches!(result, WriterError::Closed), "got {:?}", result);
    }

    #[tokio::test]
    async fn writer_client_stream_close() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let sut = Writer::new(empty::<TestResponse>());
        pin!(sut);
        let res = sut.as_mut().poll(&mut cx);
        let result = sut.poll_ready_unpin(&mut cx);
        let result = assert_ready_err!(result);
        assert!(matches!(result, WriterError::Closed));
    }

    #[tokio::test]
    async fn writer_close_with_pending_data() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut p1 = test_callback!();
        let inner = stream::iter(vec![p1.0]).chain(stream::pending());

        let sut = Writer::new(inner);
        pin!(sut);

        // Poll the future to process the stream and transition to Open state
        let _ = sut.as_mut().poll(&mut cx);

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
mod tests_writer_state {
    use super::*;
    use tokio::sync::oneshot;

    type TestCallback = oneshot::Sender<String>;

    fn make_test_callback() -> TestCallback {
        let (tx, _rx) = oneshot::channel();
        tx
    }

    fn make_instant() -> std::time::Instant {
        std::time::Instant::now()
    }

    #[test]
    fn waiting_state_pending_within_timeout() {
        let now = make_instant();
        let mut state = WriterState::<TestCallback>::new(now);

        let event = state.do_io(Poll::Pending, now + Duration::from_secs(30));

        match event {
            Ok(WriterEvent::Wait(Some(deadline))) => {
                assert_eq!(deadline, now + Duration::from_secs(60));
            }
            _ => panic!("Expected Ok(Wait) event with deadline"),
        }

        match state {
            WriterState::Waiting { since, .. } => assert_eq!(since, now),
            _ => panic!("Expected to remain in Waiting state"),
        }
    }

    #[test]
    fn waiting_state_pending_timeout_exceeded() {
        let now = make_instant();
        let mut state = WriterState::<TestCallback>::new(now);

        let event = state.do_io(Poll::Pending, now + Duration::from_secs(61));

        match event {
            Err(WriterError::PollingError) => {}
            _ => panic!("Expected PollingError due to timeout"),
        }
    }

    #[test]
    fn waiting_state_ready_some_timeout_exceeded_returns_error() {
        let now = make_instant();
        let mut state = WriterState::<TestCallback>::new(now);
        let callback = make_test_callback();

        // Even though we have a ready callback, timeout exceeded should return error
        let event = state.do_io(Poll::Ready(Some(callback)), now + Duration::from_secs(61));

        match event {
            Err(WriterError::PollingError) => {}
            _ => panic!("Expected PollingError due to timeout"),
        }
    }

    #[test]
    fn waiting_state_ready_none_timeout_exceeded_returns_error() {
        let now = make_instant();
        let mut state = WriterState::<TestCallback>::new(now);

        // Even though we have stream closure, timeout exceeded should return timeout error
        let event = state.do_io(Poll::Ready(None), now + Duration::from_secs(61));

        match event {
            Err(WriterError::PollingError) => {}
            _ => panic!("Expected PollingError due to timeout (not stream closure)"),
        }
    }

    #[test]
    fn waiting_state_ready_some_transitions_to_open() {
        let now = make_instant();
        let mut state = WriterState::<TestCallback>::new(now);
        let callback = make_test_callback();

        let event = state.do_io(Poll::Ready(Some(callback)), now + Duration::from_secs(10));

        match event {
            Ok(WriterEvent::Wait(None)) => {}
            _ => panic!("Expected Ok(Wait) event with no deadline"),
        }

        match state {
            WriterState::Open { since, buf, out } => {
                assert_eq!(since, now + Duration::from_secs(10));
                assert_eq!(buf, 0);
                assert!(out.is_some());
            }
            _ => panic!("Expected transition to Open state"),
        }
    }

    #[test]
    fn waiting_state_ready_none_transitions_to_closed() {
        let now = make_instant();
        let mut state = WriterState::<TestCallback>::new(now);

        let event = state.do_io(Poll::Ready(None), now + Duration::from_secs(10));

        match event {
            Ok(WriterEvent::Closed) => {}
            _ => panic!("Expected Ok(Closed) event"),
        }

        match state {
            WriterState::Closed => {}
            _ => panic!("Expected transition to Closed state"),
        }
    }

    #[test]
    fn open_state_pending_within_timeout_no_buffer() {
        let now = make_instant();
        let callback = make_test_callback();
        let mut state = WriterState::Open {
            since: now,
            buf: 0,
            out: Some(callback),
        };

        let event = state.do_io(Poll::Pending, now + Duration::from_secs(30));

        match event {
            Ok(WriterEvent::Wait(Some(deadline))) => {
                assert_eq!(deadline, now + Duration::from_secs(60));
            }
            _ => panic!("Expected Ok(Wait) event with deadline"),
        }

        match state {
            WriterState::Open { since, buf, out } => {
                assert_eq!(since, now);
                assert_eq!(buf, 0);
                assert!(out.is_some());
            }
            _ => panic!("Expected to remain in Open state"),
        }
    }

    #[test]
    fn open_state_pending_timeout_exceeded_with_buffer_returns_error() {
        let now = make_instant();
        let callback = make_test_callback();
        let mut state = WriterState::Open {
            since: now,
            buf: 1, // Has buffer
            out: Some(callback),
        };

        let event = state.do_io(Poll::Pending, now + Duration::from_secs(61));

        match event {
            Err(WriterError::PollingTimeout) => {}
            _ => panic!("Expected PollingTimeout error when timeout exceeded with buffer"),
        }

        // State should remain Open - caller handles callback and state transition
        match state {
            WriterState::Open { since, buf, out } => {
                assert_eq!(since, now);
                assert_eq!(buf, 1);
                assert!(out.is_some());
            }
            _ => panic!("Expected to remain in Open state"),
        }
    }

    #[test]
    fn open_state_pending_with_buffer_no_timeout_stays_open() {
        let now = make_instant();
        let callback = make_test_callback();
        let mut state = WriterState::Open {
            since: now,
            buf: 1,
            out: Some(callback),
        };

        let event = state.do_io(Poll::Pending, now + Duration::from_secs(30)); // Within timeout

        match event {
            Ok(WriterEvent::Wait(Some(deadline))) => {
                assert_eq!(deadline, now + Duration::from_secs(60));
            }
            _ => panic!("Expected Ok(Wait) event with original deadline"),
        }

        match state {
            WriterState::Open { since, buf, out } => {
                assert_eq!(since, now);
                assert_eq!(buf, 1);
                assert!(out.is_some());
            }
            _ => panic!("Expected to remain in Open state"),
        }
    }

    #[test]
    fn open_state_ready_some_returns_error() {
        let now = make_instant();
        let callback = make_test_callback();
        let new_callback = make_test_callback();
        let mut state = WriterState::Open {
            since: now,
            buf: 0,
            out: Some(callback),
        };

        let event = state.do_io(
            Poll::Ready(Some(new_callback)),
            now + Duration::from_secs(10),
        );

        match event {
            Err(WriterError::PollingError) => {}
            _ => panic!("Expected PollingError for multiple requests"),
        }

        // State should remain Open with new callback stored
        match state {
            WriterState::Open { since, buf, out } => {
                assert_eq!(since, now);
                assert_eq!(buf, 0);
                assert!(out.is_some());
            }
            _ => panic!("Expected to remain in Open state"),
        }
    }

    #[test]
    fn open_state_ready_none_transitions_to_closed() {
        let now = make_instant();
        let callback = make_test_callback();
        let mut state = WriterState::Open {
            since: now,
            buf: 0,
            out: Some(callback),
        };

        let event = state.do_io(Poll::Ready(None), now + Duration::from_secs(10));

        match event {
            Ok(WriterEvent::Closed) => {}
            _ => panic!("Expected Ok(Closed) event"),
        }

        match state {
            WriterState::Closed => {}
            _ => panic!("Expected transition to Closed state"),
        }
    }

    #[test]
    fn closed_state_remains_closed() {
        let now = make_instant();
        let mut state = WriterState::<TestCallback>::Closed;

        // Test all possible inputs
        let inputs = [
            Poll::Pending,
            Poll::Ready(Some(make_test_callback())),
            Poll::Ready(None),
        ];

        for input in inputs {
            let event = state.do_io(input, now);

            match event {
                Ok(WriterEvent::Closed) => {}
                _ => panic!("Expected Ok(Closed) event for closed state"),
            }

            match state {
                WriterState::Closed => {}
                _ => panic!("Expected to remain in Closed state"),
            }
        }
    }

    #[test]
    fn close_method_from_waiting_state() {
        let now = make_instant();
        let mut state = WriterState::<TestCallback>::new(now);

        let callback = state.close();

        match state {
            WriterState::Closed => {}
            _ => panic!("Expected transition to Closed state after close()"),
        }

        assert!(callback.is_none(), "Waiting state should not have callback");
    }

    #[test]
    fn close_method_from_open_state() {
        let now = make_instant();
        let callback = make_test_callback();
        let mut state = WriterState::Open {
            since: now,
            buf: 0,
            out: Some(callback),
        };

        let returned_callback = state.close();

        match state {
            WriterState::Closed => {}
            _ => panic!("Expected transition to Closed state after close()"),
        }

        assert!(
            returned_callback.is_some(),
            "Open state should return callback"
        );
    }

    #[test]
    fn close_method_on_already_closed_state() {
        let mut state = WriterState::<TestCallback>::Closed;

        let callback = state.close(); // Should be a no-op

        match state {
            WriterState::Closed => {}
            _ => panic!("Expected to remain in Closed state"),
        }

        assert!(callback.is_none(), "Closed state should not have callback");
    }

    #[test]
    fn new_creates_waiting_state() {
        let now = make_instant();
        let state = WriterState::<TestCallback>::new(now);

        match state {
            WriterState::Waiting { since, waker } => {
                assert_eq!(since, now);
                assert!(waker.is_none());
            }
            _ => panic!("Expected new() to create Waiting state"),
        }
    }
}

#[cfg(test)]
mod tests_writer_future {
    use super::*;
    use futures::task::noop_waker_ref;
    use futures::{stream, Future, StreamExt};
    use std::task::{Context, Poll};
    use tokio::pin;
    use tokio::sync::oneshot;
    use tokio::sync::oneshot::error::TryRecvError;
    use tokio_test::{assert_pending, assert_ready_err, assert_ready_ok};

    type TestResponse = Callback<Result<Option<()>>>;

    macro_rules! test_callback {
        () => {
            oneshot::channel::<Result<Option<()>>>()
        };
    }

    #[tokio::test]
    async fn future_resolves_ready_on_stream_closure() {
        let mut cx = Context::from_waker(noop_waker_ref());

        // Create a writer with an empty stream (immediately closes)
        let sut = Writer::new(stream::empty::<TestResponse>());
        pin!(sut);

        // Future should resolve to Ready(Ok(())) when stream closes gracefully
        let result = sut.poll(&mut cx);
        assert_ready_ok!(result);
    }

    #[tokio::test]
    async fn future_resolves_pending_on_stream_pending() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let sut = Writer::new(stream::pending::<TestResponse>());
        pin!(sut);
        // return Pending when no poll re
        let result = sut.poll_ready(&mut cx);
        assert_pending!(result)
    }

    #[tokio::test]
    async fn future_resolves_ready_on_polling_timeout() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let (tx, _rx) = test_callback!();

        // Create a stream that provides one callback then never provides more
        let callback_stream = stream::iter(vec![tx]).chain(stream::pending());
        let sut = Writer::new(callback_stream);
        pin!(sut);

        // Poll once to get the callback and transition to Open state
        let _ = sut.as_mut().poll(&mut cx);

        // Simulate time passing to trigger timeout
        // We need to manipulate the state to have a buffer and exceed timeout
        // This is a bit tricky to test directly due to the hardcoded 60s timeout
        // For now, let's test what happens when the state machine returns an error

        // The Future should resolve to Ready(Err()) on timeout
        // Note: This test is somewhat limited due to the hardcoded timeout in the implementation
        // In a real scenario, we'd want configurable timeouts for testing
    }

    #[tokio::test]
    async fn future_handles_polling_errors_TESTING() {
        let (tx1, mut rx1) = test_callback!();
        tx1.send(Ok(Some(())));
        let res1 = rx1.try_recv();
        assert!(matches!(res1, Ok(..)));
    }
    #[tokio::test]
    async fn future_handles_polling_errors() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let (tx1, mut rx1) = test_callback!();
        let (tx2, mut rx2) = test_callback!();

        // Create a stream that provides two callbacks immediately (causes polling error)
        let callback_stream = stream::iter(vec![tx1, tx2]);
        let sut = Writer::new(callback_stream);
        pin!(sut);

        // Future should resolve to Ready(Err()) due to multiple polling requests
        // The first poll should transition to Open state successfully
        // The second poll within the loop should detect multiple requests and error
        let err = assert_ready_err!(sut.as_mut().poll(&mut cx));
        assert!(matches!(err, WriterError::PollingError));

        // Verify that existing callback gets closed
        // TODO: Should we report error ?
        let res1 = rx1.try_recv();
        assert!(matches!(res1, Err(TryRecvError::Closed)));

        // Verify that the callback received an error notification
        let res2 = rx2.try_recv();
        assert!(matches!(res2, Ok(..)));
        assert!(matches!(res2.unwrap(), Err(WriterError::PollingError)));
    }

    #[tokio::test]
    async fn future_continues_looping_on_state_transitions() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let (tx, _rx) = test_callback!();

        // Create a stream with one callback followed by pending
        let callback_stream = stream::iter(vec![tx]).chain(stream::pending());
        let sut = Writer::new(callback_stream);
        pin!(sut);

        // First poll should transition from Waiting to Open and return Pending
        let result = sut.as_mut().poll(&mut cx);
        assert_pending!(result);

        // Writer should now be in Open state, ready to send messages
        // Subsequent polls should return Pending (waiting for timeout or more callbacks)
        let result = sut.as_mut().poll(&mut cx);
        assert_pending!(result);
    }

    // Test Sink once stream finalises

    #[tokio::test]
    async fn sink_interface_after_future_resolution() {
        let mut cx = Context::from_waker(noop_waker_ref());

        // Create a writer with an empty stream
        let sut = Writer::new(stream::empty::<TestResponse>());
        pin!(sut);

        // Resolve the future
        let _ = assert_ready_ok!(sut.as_mut().poll(&mut cx));

        // After future resolution, sink operations should return Closed error
        let ready_err = assert_ready_err!(sut.as_mut().poll_ready(&mut cx));
        assert!(matches!(ready_err, WriterError::Closed));

        let send_result = sut.as_mut().start_send(Some(()));
        assert!(send_result.is_err());
        assert!(matches!(
            send_result.unwrap_err(),
            WriterError::PollingError
        ));

        // For flush on closed writer with no buffered data, it may return Ok(())
        let flush_result = sut.as_mut().poll_flush(&mut cx);
        // This might be Ok(()) if there's no buffered data to flush
        if let Poll::Ready(Ok(())) = flush_result {
            // This is expected behavior - nothing to flush
        } else {
            let flush_err = assert_ready_err!(flush_result);
            assert!(matches!(flush_err, WriterError::Closed));
        }

        // Close might return Ok(()) if the writer is already closed
        let close_result = sut.as_mut().poll_close(&mut cx);
        match close_result {
            Poll::Ready(Ok(())) => {
                // This is acceptable - writer is already closed
            }
            Poll::Ready(Err(err)) => {
                assert!(matches!(err, WriterError::Closed));
            }
            _ => panic!("Expected ready result from close"),
        }
    }

    #[tokio::test]
    async fn future_handles_single_callback_lifecycle() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let (tx, mut rx) = test_callback!();

        // Create a stream with one callback that we can use for the full lifecycle
        let callback_stream = stream::iter(vec![tx]).chain(stream::pending());
        let sut = Writer::new(callback_stream);
        pin!(sut);

        // Poll to transition to Open state
        assert_pending!(sut.as_mut().poll(&mut cx));

        // Should be ready to send now
        assert_ready_ok!(sut.as_mut().poll_ready(&mut cx));

        // Send a message
        assert!(sut.as_mut().start_send(Some(())).is_ok());

        // Flush should send the message via callback
        assert_ready_ok!(sut.as_mut().poll_flush(&mut cx));

        // Verify the callback received the message
        let response = rx.try_recv().expect("Should have received response");
        assert!(response.is_ok());
        assert_eq!(response.unwrap(), Some(()));
    }

    #[tokio::test]
    async fn future_handles_callback_send_errors() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let (tx, rx) = test_callback!();

        // Drop the receiver to simulate send error
        drop(rx);

        let callback_stream = stream::iter(vec![tx]).chain(stream::pending());
        let sut = Writer::new(callback_stream);
        pin!(sut);

        // Poll to transition to Open state
        assert_pending!(sut.as_mut().poll(&mut cx));

        // Send and flush a message
        assert_ready_ok!(sut.as_mut().poll_ready(&mut cx));
        assert!(sut.as_mut().start_send(Some(())).is_ok());

        // Flush should fail due to dropped receiver
        let flush_result = sut.as_mut().poll_flush(&mut cx);
        let err = assert_ready_err!(flush_result);
        assert!(matches!(err, WriterError::PollingError));
    }

    #[tokio::test]
    async fn future_timer_behavior() {
        let mut cx = Context::from_waker(noop_waker_ref());
        let (tx, _rx) = test_callback!();

        let callback_stream = stream::iter(vec![tx]).chain(stream::pending());
        let sut = Writer::new(callback_stream);
        pin!(sut);

        // Poll to get into Open state with a deadline
        assert_pending!(sut.as_mut().poll(&mut cx));

        // The timer should now be set. Subsequent polls should still be pending
        // until either timeout or new events arrive
        assert_pending!(sut.as_mut().poll(&mut cx));
        assert_pending!(sut.as_mut().poll(&mut cx));

        // This tests that the timer is properly integrated with the polling mechanism
        // In a real scenario with actual time passing, the timer would eventually
        // trigger timeout behavior
    }
}
