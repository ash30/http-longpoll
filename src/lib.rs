use axum::response::IntoResponse;
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{ready, Context};
use std::{convert::Infallible, future::Future, task::Poll};

use axum::{
    extract::Request,
    handler::{Handler, HandlerService},
    http::{status, Method, StatusCode},
    response::Response,
};
use tower::Service;

#[derive(Clone)]
pub struct EngineTransport<S> {
    inner: S,
}

// EngineIO needs to be a service ? becaues it handles mutliple methods
pub fn engine_io<T, S: Clone, H: Handler<T, S>>(
    handler: H,
    state: S,
) -> EngineTransport<HandlerService<H, T, S>> {
    EngineTransport {
        inner: handler.with_state(state),
    }
}

impl<S> Service<Request> for EngineTransport<S>
where
    // NOTE: Are we right to bound response type like this?
    S: Service<Request, Error = Infallible>,
    S::Response: IntoResponse + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = EngineFuture<S::Future>;

    fn call(&mut self, req: Request) -> Self::Future {
        // We have to call handler with correct things....
        // IFF its a GET
        // else route to correct thing
        //
        match *req.method() {
            Method::GET => {
                // we need to match over transport query param here
                EngineFuture::new(self.inner.call(req))
            }
            Method::DELETE => todo!(),
            Method::POST => todo!(),
            _ => EngineFuture::err(StatusCode::METHOD_NOT_ALLOWED),
        }
    }

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

pin_project! {
    struct EngineFuture<F> {
        #[pin]
        inner: EngineFutureInner<F>,
    }
}

pin_project! {
    #[project = EngFutProj]
    enum EngineFutureInner<F> {
        HandshakeErr{ code:StatusCode },
        Future { #[pin] future: F },
    }
}

impl<F> EngineFuture<F> {
    fn new(future: F) -> Self {
        Self {
            inner: EngineFutureInner::Future { future },
        }
    }

    fn err(code: StatusCode) -> Self {
        Self {
            inner: EngineFutureInner::HandshakeErr { code },
        }
    }
}

// NOTE to self: The err of F's Result is defined by prev service
impl<F, R, E> Future for EngineFuture<F>
where
    R: IntoResponse + 'static,
    F: Future<Output = Result<R, E>> + Send + 'static,
{
    type Output = Result<Response, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = match self.project().inner.project() {
            EngFutProj::Future { future } => ready!(future.poll(cx)?),
            EngFutProj::HandshakeErr { code } => todo!(),
        };
        Poll::Ready(Ok(res.into_response()))
    }
}

#[cfg(test)]
mod tests {
    use crate::engine_io;
    use axum::Router;

    #[test]
    fn init() {
        async fn test() -> &'static str {
            "Hello World"
        }
        let s = engine_io(test, ());
        let r: Router<()> = Router::new().route_service("/rtc", s); //.with_state(()));
    }
}
