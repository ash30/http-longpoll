use axum::body::Body;
use axum::extract::FromRequest;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{extract::Request, response::Response};
use futures::future::BoxFuture;
use futures::{Future, FutureExt, Sink, Stream, TryFutureExt};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use crate::http_poll::{HTTPPollError, Payload, PollReqStream, ResponseCallback};

pub trait FromPollRequest<T>: Sized {
    fn from_poll_req(req: Request<T>) -> impl Future<Output = Result<Self, Response>> + Send;
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

// ========

pin_project! {
    pub struct Session<E:IntoPollResponse<U>,T,U> {
        #[pin]
        inner: PollReqStream<T,U>,
        next: Option<(BoxFuture<'static,Result<E,Response<U>>>, ResponseCallback<U>)>,
        buf: VecDeque<E::Buffered>,
        max_size:usize,
    }
}

impl<T, U, E> Session<E, T, U>
where
    E: IntoPollResponse<U>,
{
    pub fn new(s: PollReqStream<T, U>, max_size: usize) -> Self {
        let mut buf = VecDeque::default();
        buf.make_contiguous();
        Session {
            inner: s,
            next: None,
            buf,
            max_size,
        }
    }
}

impl<T> FromPollRequest<Body> for T
where
    T: FromRequest<()>,
{
    fn from_poll_req(req: Request) -> impl Future<Output = Result<Self, Response>> + Send {
        T::from_request(req, &()).map_err(|e| e.into_response())
    }
}

impl<T, U, E> Stream for Session<E, T, U>
where
    U: From<()>,
    E: FromPollRequest<T> + IntoPollResponse<U>,
{
    type Item = Result<E, HTTPPollError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        Poll::Ready(loop {
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
            } else if let Some(n) = ready!(this.inner.poll_next(cx)) {
                // get next extractor fut
                match n {
                    Err(e) => Err(e),
                    // For now ignore poll requests,
                    // in future, would like to use them within heartbeats
                    Ok(Payload::Poll) => continue,
                    Ok(Payload::Req((req, res))) => {
                        let _ = this.next.replace((
                            E::from_poll_req(req)
                                .map_err(|e| HTTPPollError::PollingError)
                                .boxed(),
                            res,
                        ));
                        continue;
                    }
                }
            } else {
                // inner has finished
                break None;
            }
        })
    }
}

impl<T, U, E> Session<E, T, U>
where
    U: From<()>,
    E: IntoPollResponse<U>,
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
    Send(E),
    SendLimit,
    Polling(HTTPPollError),
}

impl<E> From<HTTPPollError> for FrameError<E> {
    fn from(value: HTTPPollError) -> Self {
        FrameError::Polling(value)
    }
}

impl<T, U, E> Sink<E> for Session<E, T, U>
where
    U: From<()>,
    E: IntoPollResponse<U>,
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
        let n: E::Buffered = item.try_into().map_err(FrameError::Send)?;
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
