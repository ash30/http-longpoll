use crate::http_poll::{Foldable, ResponseFramer, ResultCallback, Writer, WriterError};
use futures::{Future, Sink, SinkExt, Stream};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::{
    task::{ready, Poll},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

pub type SessionError = WriterError;

#[derive(Copy, Clone)]
pub struct Config {
    pub message_max_size: usize,
    pub request_capactiy: usize,
    pub poll_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            message_max_size: 1 << 20,
            request_capactiy: 16,
            poll_timeout: Duration::from_secs(30),
        }
    }
}

pin_project! {
    pub struct Session<T> where T:Foldable {
        #[pin]
        read: ReceiverStream<Result<T,()>>,
        #[pin]
        write: ResponseFramer<Writer<ReceiverStream<ResultCallback<T::Start>>>,T>,
    }
}

impl<T> Session<T>
where
    T: Foldable,
{
    fn new(
        read: ReceiverStream<Result<T, ()>>,
        write: ReceiverStream<ResultCallback<T::Start>>,
        max_size: usize,
        poll_timeout: Duration,
    ) -> Self {
        Self {
            read,
            write: ResponseFramer::new(max_size, Writer::new(write)),
        }
    }

    pub async fn close_session(mut self: Pin<&mut Self>) -> Result<(), SessionError> {
        // Close read, and then poll write for flush + close
        // calling code should drain session stream as well
        self.as_mut().project().read.close();
        self.project().write.close().await
    }
}

// PROXY STREAM AND SINK
//

impl<T> Stream for Session<T>
where
    T: Foldable,
{
    type Item = T;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        //
        // Close reader stream so eventually stream finishes
        if let Poll::Ready(result) = self.as_mut().project().write.poll(cx) {
            // what todo about error ?
            self.as_mut().project().read.close();
        }

        match ready!(self.as_mut().project().read.poll_next(cx)) {
            Some(Err(_)) => {
                self.project().read.close();
                Poll::Ready(None)
                // TODO: fuse it ?
            }
            other => Poll::Ready(other.map(|r| r.unwrap())),
        }
    }
}

impl<T> Sink<T> for Session<T>
where
    T: Foldable,
{
    type Error = WriterError;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.project().write.poll_ready(cx)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
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

impl<T> Session<T>
where
    T: Foldable,
{
    pub fn connect(config: Config) -> (SessionHandle<T>, Session<T>) {
        let (p_tx, p_rx) = mpsc::channel(config.request_capactiy);
        let (m_tx, m_rx) = mpsc::channel(config.request_capactiy);
        (
            SessionHandle {
                msg: m_tx,
                poll: p_tx,
            },
            Session::new(
                ReceiverStream::new(m_rx),
                ReceiverStream::new(p_rx),
                config.message_max_size,
                config.poll_timeout,
            ),
        )
    }
}

#[derive(Debug)]
pub struct SessionHandle<T>
where
    T: Foldable,
{
    msg: mpsc::Sender<Result<T, ()>>,
    poll: mpsc::Sender<ResultCallback<T::Start>>,
}

// Implement Clone manually to avoid B affecting derive
impl<T> Clone for SessionHandle<T>
where
    T: Foldable,
{
    fn clone(&self) -> Self {
        Self {
            msg: self.msg.clone(),
            poll: self.poll.clone(),
        }
    }
}

impl<T> SessionHandle<T>
where
    T: Foldable,
{
    pub async fn close(&mut self) -> Result<(), SessionError> {
        // We send a ERR down to session (client code might need to timeout this op ?)
        // Client can choose to drain remaining message or DROP handle
        self.msg
            .send(Err(()))
            .await
            .map_err(|_| SessionError::Closed)?;
        Ok(())
    }

    pub async fn msg(&mut self, item: T) -> Result<(), SessionError> {
        self.msg
            .send(Ok(item))
            .await
            .map_err(|_| SessionError::Closed)
    }

    pub async fn poll(&mut self) -> Result<T::Start, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.poll.send(tx).await.map_err(|_| SessionError::Closed)?;

        rx.await.map_err(|_| SessionError::Closed)?
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::axum::*;

    use futures::task::noop_waker_ref;
    use futures::Future;
    use futures::Stream;
    use std::task::Context;
    use std::time::Duration;
    use tokio_test::assert_pending;
    use tokio_test::assert_ready;

    #[tokio::test]
    async fn session_timeout_no_idle() {
        let config = Config {
            poll_timeout: Duration::from_secs(1),
            ..Default::default()
        };
        let (handle, session) = Session::<Bytes>::connect(config);
        tokio::pin!(session);
        let mut cx = Context::from_waker(noop_waker_ref());

        // Initially no message
        // this should NOT trigger timeout, since no waiting poll req
        assert_pending!(session.as_mut().poll_next(&mut cx));
    }

    // TODO: WE have to use tokio test because of sleep timer...
    #[tokio::test]
    async fn session_timeout_idle_conn() {
        let config = Config {
            poll_timeout: Duration::from_secs(1),
            ..Default::default()
        };
        let (mut handle, session) = Session::<Bytes>::connect(config);
        tokio::pin!(session);
        let mut cx = Context::from_waker(noop_waker_ref());

        let poll_req = handle.poll();
        tokio::pin!(poll_req);

        // Should pend waiting for reply
        assert_pending!(poll_req.poll(&mut cx));

        // Active timer on msg poll
        assert_pending!(session.as_mut().poll_next(&mut cx));
    }

    #[tokio::test]
    async fn session_timeout_idle_conn_flush() {
        tokio::time::pause();
        let config = Config {
            poll_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let (mut handle, session) = Session::<Bytes>::connect(config);
        tokio::pin!(session);
        let mut cx = Context::from_waker(noop_waker_ref());

        let poll_req = handle.poll();
        tokio::pin!(poll_req);

        assert_pending!(poll_req.as_mut().poll(&mut cx));
        assert_pending!(session.as_mut().poll_next(&mut cx));

        tokio::time::advance(Duration::from_millis(200)).await;

        assert_ready!(poll_req.poll(&mut cx));
    }
}
