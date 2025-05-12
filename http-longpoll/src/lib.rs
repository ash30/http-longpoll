#![feature(trace_macros)]

mod http_poll;

use futures::{Future, Sink, SinkExt, Stream};
use http_poll::{Appendable, ResponseFramer, ResultCallback, Writer, WriterError};
use pin_project_lite::pin_project;
use std::{
    task::{ready, Poll},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, Sleep},
};
use tokio_stream::wrappers::ReceiverStream;

// API
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
    pub struct Session<T>{
        #[pin]
        read: ReceiverStream<Result<T,()>>,
        #[pin]
        write: ResponseFramer<Writer<ReceiverStream<ResultCallback<T>>>,T>,
        #[pin]
        timer: Sleep,
        duration: Duration,
        timer_active: bool,
    }
}

impl<T> Session<T>
where
    T: Appendable,
{
    fn new(
        read: ReceiverStream<Result<T, ()>>,
        write: ReceiverStream<ResultCallback<T>>,
        max_size: usize,
        poll_timeout: Duration,
    ) -> Self {
        Self {
            read,
            write: ResponseFramer::new(max_size, Writer::new(write)),
            timer: tokio::time::sleep(Duration::default()),
            duration: poll_timeout,
            timer_active: false,
        }
    }

    pub async fn close(&mut self) -> Result<(), SessionError> {
        // Close read, and then poll write for flush + close
        // calling code should drain session stream as well
        self.read.close();
        self.write.close().await
    }
}

// PROXY STREAM AND SINK
//
impl<T> Session<T>
where
    T: Appendable,
{
    fn poll_timer(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        now: tokio::time::Instant,
    ) {
        if !self.timer_active
            && self
                .as_mut()
                .project()
                .write
                .poll_connection_idle(cx)
                .is_ready()
        {
            let this = self.as_mut().project();
            *this.timer_active = true;
            this.timer.reset(now + *this.duration);
        }
        if self.timer_active && self.as_mut().project().timer.poll(cx).is_ready() {
            // timeout!
            let this = self.as_mut().project();
            *this.timer_active = false;
            // THIS SHOULD NOT PEND!
            // as we have idle connection ...
            // TODO: what about errors ...
            self.as_mut().start_send(T::unit());
            self.as_mut().poll_flush(cx);
        }
    }
}

impl<T> Stream for Session<T>
where
    T: Appendable,
{
    type Item = T;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // Start Idle connection timer if not active
        self.as_mut().poll_timer(cx, Instant::now());
        match ready!(self.as_mut().project().read.poll_next(cx)) {
            // Client close signal
            // NOTE: YOU MUST Poll Stream to see client Closes...
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
    T: Appendable,
{
    type Error = WriterError;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.project().write.poll_ready(cx)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        // Disable idle timeout once we start sending
        *self.as_mut().project().timer_active = false;
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
    T: Appendable,
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
pub struct SessionHandle<T> {
    msg: mpsc::Sender<Result<T, ()>>,
    poll: mpsc::Sender<ResultCallback<T>>,
}

// Implement Clone manually to avoid B affecting derive
impl<T> Clone for SessionHandle<T> {
    fn clone(&self) -> Self {
        Self {
            msg: self.msg.clone(),
            poll: self.poll.clone(),
        }
    }
}

type SessionError = WriterError;

impl<T> SessionHandle<T> {
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

    pub async fn poll(&mut self) -> Result<T, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.poll.send(tx).await.map_err(|_| SessionError::Closed)?;

        rx.await.map_err(|_| SessionError::Closed)?
    }
}

//#[cfg(feature = "axum")]
pub mod axum {
    use crate::http_poll::Appendable;
    use crate::SessionError;
    use axum::response::IntoResponse;
    pub use bytes::Bytes;

    pub type Session<T = Bytes> = super::Session<T>;
    pub type SessionHandle<T = Bytes> = super::SessionHandle<T>;

    // Allow common response types to be used
    impl Appendable for Bytes {
        fn unit() -> Self {
            Bytes::new()
        }
        fn len(&self) -> usize {
            self.len()
        }
        fn append(&mut self, next: Self) {
            todo!()
        }
    }

    // Calling code can just handle method results if desired
    impl IntoResponse for SessionError {
        fn into_response(self) -> axum::response::Response {
            todo!()
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn init() {}
}
