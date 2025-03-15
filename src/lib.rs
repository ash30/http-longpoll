use axum::body::Body;
use axum::extract::{FromRequestParts, Query, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::future::BoxFuture;
use futures::{Sink, Stream, StreamExt};
use pin_project_lite::pin_project;
use serde::Deserialize;
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
pub struct EngineService<S> {
    inner: S,
}

// EngineIO needs to be a service ? becaues it handles mutliple methods
pub fn engine_io<T, S: Clone, H: Handler<T, S>>(
    handler: H,
    state: S,
) -> EngineService<HandlerService<H, T, S>> {
    EngineService {
        inner: handler.with_state(state),
    }
}

impl<S> Service<Request> for EngineService<S>
where
    // NOTE: Are we right to bound response type like this?
    // WE want to take a http handler, but it seems to be generic over other service
    S: Service<Request, Error = Infallible>,
    S::Response: IntoResponse + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = EngineServiceFuture<S::Future>;

    fn call(&mut self, req: Request) -> Self::Future {
        // We have to call handler with correct things....
        // IFF its a GET
        // else route to correct thing
        //
        match *req.method() {
            Method::GET => {
                // we need to match over transport query param here
                EngineServiceFuture::new(self.inner.call(req))
            }
            Method::DELETE => todo!(),
            Method::POST => todo!(),
            _ => EngineServiceFuture::err(StatusCode::METHOD_NOT_ALLOWED),
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
    pub struct EngineServiceFuture<F> {
        #[pin]
        inner: EngineFutureInner<F>,
    }
}

// Do we need todo handshake enum ?
//
pin_project! {
    #[project = EngFutProj]
    enum EngineFutureInner<F> {
        HandshakeErr{ code:StatusCode },
        Future { #[pin] future: F },
    }
}

impl<F> EngineServiceFuture<F> {
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
impl<F, R, E> Future for EngineServiceFuture<F>
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

// ====
//

pub enum EngineError {
    Unknown,
    Session,
    QueryMalformed,
}

impl IntoResponse for EngineError {
    fn into_response(self) -> Response {
        let s = match self {
            Self::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
            Self::QueryMalformed => StatusCode::BAD_REQUEST,
            Self::Session => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Response::builder().status(s).body(Body::empty()).unwrap()
    }
}

pub enum Engine {
    Websocket(WebSocketUpgrade),
    Polling,
}

impl Engine {
    fn on_connect<Fut, C>(self, callback: C) -> Response
    where
        C: FnOnce(Box<dyn Stream<Item = EngineMessage>>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        match self {
            Engine::Websocket(ws) => ws.on_upgrade(|w| callback(Box::new(w))),
            Engine::Polling => Response::builder()
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap(),
        }
    }
}

enum EngineMessage {}

#[derive(Deserialize)]
struct EngineQuery {
    transport: String,
    eio: u8,
    sid: Option<String>,
}

impl<S> FromRequestParts<S> for Engine
where
    S: Send + Sync,
{
    type Rejection = EngineError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let Ok(params) = Query::<EngineQuery>::from_request_parts(parts, state).await else {
            return Err(EngineError::QueryMalformed);
        };
        // WE SHOULD NOT have a session at this point ...
        if let Some(sid) = &params.sid {
            return Err(EngineError::Session);
        }
        let inner = match params.transport.as_str() {
            "polling" => TransportInner::Polling(PollingTranport {}),
            "websocket" => TransportInner::Websocket(WebsocketTransport {}),
            _ => return Err(EngineError::Unknown),
        }
        .into();

        Ok(Engine {
            inner: Box::new(|| inner),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{engine_io, Engine};
    use axum::{response::Response, Router};

    #[test]
    fn init() {
        #[axum::debug_handler]
        async fn test(e: Engine) -> Response {
            e.on_connect(|transport| async move {})
        }
        let s = engine_io(test, ());
        //let r: Router<()> = Router::new().route_service("/rtc", s); //.with_state(()));
    }
}
