#![feature(trace_macros)]

mod http_poll;

use std::task::{ready, Poll};

use futures::{Sink, SinkExt, Stream};
use http_poll::{Appendable, ResponseFramer, ResultCallback, Writer, WriterError};
use pin_project_lite::pin_project;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

// API
#[derive(Copy, Clone)]
pub struct Config {
    pub message_max_size: usize,
    pub request_capactiy: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            message_max_size: 1 << 20,
            request_capactiy: 16,
        }
    }
}

pin_project! {
    struct Session<T>{
        #[pin]
        read: ReceiverStream<Result<T,()>>,
        #[pin]
        write: ResponseFramer<Writer<ReceiverStream<ResultCallback<T>>>,T>
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
    ) -> Self {
        Self {
            read,
            write: ResponseFramer::new(max_size, Writer::new(write)),
        }
    }

    pub async fn close(&mut self) -> Result<(), SessionError> {
        // Close read, and then poll write for flush + close
        // calling code should drain session stream as well
        // this is 'server' side close, but can also be used as response
        // to 'client close' for convenience / simple api
        self.read.close();
        self.write.close().await
    }
}

// PROXY STREAM AND SINK
impl<T> Stream for Session<T> {
    type Item = T;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match ready!(self.as_mut().project().read.poll_next(cx)) {
            // Client close signl
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

    fn start_send(self: std::pin::Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
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
    pub fn connect(config: Config) -> (Sender<T>, Session<T>) {
        let (p_tx, p_rx) = mpsc::channel(config.request_capactiy);
        let (m_tx, m_rx) = mpsc::channel(config.request_capactiy);
        (
            Sender {
                msg: m_tx,
                poll: p_tx,
            },
            Session::new(
                ReceiverStream::new(m_rx),
                ReceiverStream::new(p_rx),
                config.message_max_size,
            ),
        )
    }
}

#[derive(Debug)]
pub struct Sender<B> {
    msg: mpsc::Sender<Result<B, ()>>,
    poll: mpsc::Sender<ResultCallback<B>>,
}

// Implement Clone manually to avoid B affecting derive
impl<B> Clone for Sender<B> {
    fn clone(&self) -> Self {
        Self {
            msg: self.msg.clone(),
            poll: self.poll.clone(),
        }
    }
}

type SessionError = WriterError;

impl<B> Sender<B> {
    pub async fn close(&mut self) -> Result<(), SessionError> {
        // We send a ERR down to session and wait for read chan close
        // Client can choose to drain remaining message or DROP handle
        self.msg
            .send(Err(()))
            .await
            .map_err(|_| SessionError::Closed)?;

        self.msg.closed().await;
        Ok(())
    }

    pub async fn msg(&mut self, item: B) -> Result<(), SessionError> {
        self.msg
            .send(Ok(item))
            .await
            .map_err(|_| SessionError::Closed)
    }

    pub async fn poll(&mut self) -> Result<B, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.poll.send(tx).await.map_err(|_| SessionError::Closed)?;
        rx.await.map_err(|_| SessionError::Closed)?
    }
}

//#[cfg(feature = "axum")]
pub mod axum {
    pub use bytes::Bytes;
    use bytes::BytesMut;

    pub type Session<E = Bytes> = super::Session<E>;
    use crate::http_poll::Appendable;

    pub use super::Sender;

    // Allow common response types to be used as aggregate longpoll response bodies
    // Bytes
    impl Appendable for Bytes {
        fn len(&self) -> usize {
            self.len()
        }
        fn append(&mut self, next: Self) {
            todo!()
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn init() {}
}
