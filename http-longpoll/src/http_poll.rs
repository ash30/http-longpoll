use axum::http::{Method, StatusCode};
use axum::{extract::Request, response::Response};
use futures::{Sink, Stream};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{ready, Context, Poll, Waker};
use tokio::sync::{mpsc, oneshot};

pub type ResponseCallback<U> = oneshot::Sender<Response<U>>;
pub type ForwardedReq<T, U> = (Request<T>, ResponseCallback<U>);

pin_project! {
    #[derive(Debug)]
    pub struct ReqStream<T,U> {
        rx: mpsc::Receiver<ForwardedReq<T,U>>,
    }
}

impl<T, U> ReqStream<T, U> {
    pub fn new(rx: mpsc::Receiver<ForwardedReq<T, U>>) -> Self {
        Self { rx }
    }
}

impl<T, U> ReqStream<T, U> {
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }
    pub fn close(&mut self) {
        self.rx.close()
    }
}

impl<T, U> Stream for ReqStream<T, U> {
    type Item = ForwardedReq<T, U>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_closed() {
            Poll::Ready(None)
        } else {
            self.project().rx.poll_recv(cx)
        }
    }
}

// =======

pub enum Payload<T> {
    Poll,
    Req(T),
}

pin_project! {
    pub struct PollReqStream<T,U> {
        #[pin]
        inner:ReqStream<T,U>,
        next_payload: VecDeque<Payload<ForwardedReq<T,U>>>,
        next_res:Option<ForwardedReq<T,U>>,
        next_send: Option<Response<U>>,
        closed:Option<HTTPPollError>,
        multi_waker: MultiTaskWaker,
    }
}

impl<T, U> PollReqStream<T, U> {
    pub fn new(s: ReqStream<T, U>) -> Self {
        Self {
            inner: s,
            next_payload: VecDeque::new(),
            next_res: None,
            next_send: None,
            closed: None,
            multi_waker: MultiTaskWaker::new(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum HTTPPollError {
    RemoteClose,
    LocalClose,
    AlreadyClosed,
    PollingError,
}

impl<T, U> Stream for PollReqStream<T, U>
where
    U: From<()>,
{
    type Item = Result<Payload<ForwardedReq<T, U>>, HTTPPollError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(p) = self.as_mut().project().next_payload.pop_front() {
                return Poll::Ready(Some(Ok(p)));
            }
            ready!(self.as_mut().poll_inner_read(cx))?;
        }
    }
}

impl<T, U> PollReqStream<T, U>
where
    U: From<()>,
{
    fn close(self: Pin<&mut Self>, err: HTTPPollError) -> HTTPPollError {
        // close existing polling responses
        let this = self.project();
        if let Some((_, polling_callback)) = this.next_res.take() {
            let _ = polling_callback.send(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(().into())
                    .unwrap(),
            );
        }
        if this.closed.is_none() {
            this.closed.replace(err);
        }
        err
    }

    fn poll_inner_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), HTTPPollError>> {
        //let w = self.multi_waker.set_read_waker(cx.waker()).as_waker();
        self.__poll_inner(cx)
    }

    fn poll_inner_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), HTTPPollError>> {
        let w = self.multi_waker.set_write_waker(cx.waker()).as_waker();
        self.__poll_inner(&mut Context::from_waker(&w))
    }

    fn __poll_inner(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), HTTPPollError>> {
        loop {
            if let Some(e) = self.closed {
                return Poll::Ready(Err(e));
            }
            match ready!(self.as_mut().project().inner.poll_next(cx)) {
                // shouldn't really get here...
                None => break Poll::Ready(Err(HTTPPollError::AlreadyClosed)),

                Some((req, res)) => match *req.method() {
                    Method::GET => {
                        // ERROR!
                        if self.next_res.is_some() {
                            // close original poll
                            // send err to new poll
                            let _ = res.send(
                                Response::builder()
                                    .status(StatusCode::BAD_REQUEST)
                                    .body(().into())
                                    .unwrap(),
                            );
                            let e = self.close(HTTPPollError::PollingError);
                            break Poll::Ready(Err(e));
                        } else {
                            let this = self.project();
                            this.next_res.replace((req, res));
                            this.next_payload.push_back(Payload::Poll);
                            break Poll::Ready(Ok(()));
                        }
                    }
                    Method::POST => {
                        self.as_mut()
                            .project()
                            .next_payload
                            .push_back(Payload::Req((req, res)));
                        break Poll::Ready(Ok(()));
                    }
                    Method::DELETE => {
                        let e = self.close(HTTPPollError::RemoteClose);
                        break Poll::Ready(Err(e));
                    }
                    _ => {
                        let _ = res.send(
                            Response::builder()
                                .status(StatusCode::METHOD_NOT_ALLOWED)
                                .body(().into())
                                .unwrap(),
                        );
                    }
                },
            }
        }
    }
}

impl<T, U> Sink<Response<U>> for PollReqStream<T, U>
where
    U: From<()>,
{
    type Error = HTTPPollError;

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.close(HTTPPollError::LocalClose);
        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            if self.next_send.is_none() {
                return Poll::Ready(Ok(()));
            }
            if let Some((_, callback)) = self.next_res.take() {
                // In theory, only get here when not closed...
                if let Err(_) = callback.send(self.as_mut().project().next_send.take().unwrap()) {
                    todo!()
                    //return Poll::Ready(Err(todo!()));
                }
            } else {
                ready!(self.as_mut().poll_inner_write(cx))?;
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Response<U>) -> Result<(), Self::Error> {
        if self.next_send.is_some() {
            todo!()
            // Err(todo!());
        } else {
            self.project().next_send.replace(item);
            Ok(())
        }
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            if self.next_send.is_some() {
                ready!(self.as_mut().poll_flush(cx))?;
            }
            if self.next_res.is_some() {
                return Poll::Ready(Ok(()));
            }
            ready!(self.as_mut().poll_inner_write(cx))?;
        }
    }
}

// =====

use futures::task::AtomicWaker;
use std::sync::Arc;
use std::task::Wake;
struct MultiTaskWaker(Arc<Inner>);
struct Inner {
    r: AtomicWaker,
    w: AtomicWaker,
}

impl MultiTaskWaker {
    pub fn new() -> Self {
        MultiTaskWaker(
            Inner {
                r: AtomicWaker::new(),
                w: AtomicWaker::new(),
            }
            .into(),
        )
    }

    fn set_read_waker(&mut self, w: &Waker) -> &mut Self {
        self.0.r.register(w);
        self
    }
    fn set_write_waker(&mut self, w: &Waker) -> &mut Self {
        self.0.w.register(w);
        self
    }

    fn as_waker(&self) -> Waker {
        Waker::from(self.0.clone())
    }
}

impl Wake for Inner {
    fn wake(self: Arc<Self>) {
        self.r.wake();
        self.w.wake();
    }
}
