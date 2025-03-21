#![feature(trace_macros)]

//trace_macros!(true);

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, Utf8Bytes};
use axum::extract::{FromRequestParts, Query, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Extension;
use futures::Sink;
use pin_project_lite::pin_project;
use serde::Deserialize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context};
use std::{convert::Infallible, future::Future, task::Poll};
use uuid7::Uuid;

use axum::{
    extract::Request,
    handler::{Handler, HandlerService},
    http::{status, Method, StatusCode},
    response::Response,
};
use tokio::sync::{self, mpsc};
use tower::Service;

type PollingSessionRef = mpsc::Sender<PollingMessage>;
type PollingSessionStore = Arc<papaya::HashMap<Uuid, PollingSessionRef>>;

enum PollingMessage {
    Poll(sync::oneshot::Sender<Result<EngineMessage,EngineError>>),
    Message(EngineMessage),
    Close
}

#[derive(Clone)]
pub struct EngineService<S> {
    inner: S,
    store: PollingSessionStore,
}

#[derive(Clone)]
struct EngineReqExtension {
    store: PollingSessionStore,
}

// EngineIO needs to be a service ? becaues it handles mutliple methods
pub fn engine_io<T, S: Clone, H: Handler<T, S>>(
    handler: H,
    state: S,
) -> EngineService<HandlerService<H, T, S>> {
    EngineService {
        inner: handler.with_state(state),
        store: papaya::HashMap::new().into(),
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

    fn call(&mut self, mut req: Request) -> Self::Future {
        // We have to call handler with correct things....
        // IFF its a GET
        // else route to correct thing

        // Set this up for *future* use
        req.extensions_mut().insert(EngineReqExtension {
            store: self.store.clone(),
        });

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

pub mod transport {
    use crate::EngineMessage;
    use axum::response::Response;
    use axum::Error;
    use futures::{Sink, Stream};
    use std::{future::Future, process::Output};

    pub trait Transport {
        type Socket: Send + 'static + Stream<Item = Result<EngineMessage, Error>>;

        fn on_connect<C, Fut>(self, callback: C) -> Response
        where
            C: FnOnce(Self::Socket) -> Fut + Send + 'static,
            Fut: Future<Output = ()> + Send + 'static;
    }

    pub mod polling {
        use futures::{Future, FutureExt, Sink, Stream};
        use pin_project_lite::pin_project;
        use uuid7::uuid7;
        use std::{
            collections::VecDeque,
            pin::Pin,
            sync::Arc,
            sync::Mutex,
            task::{Context, Poll},
        };
        use tokio::{
            pin,
            sync::{
                self, futures::Notified, mpsc::{error::TrySendError, Permit}, oneshot::error::TryRecvError, Notify
            },
        };

        use crate::{EngineMessage, PollingMessage, PollingSessionStore};

        use super::Transport;
        use futures::future::BoxFuture;

        pin_project! {
            pub struct Socket {
                tx: sync::mpsc::Sender<EngineMessage>,
                pub notify: Arc<Notify>,
                ready: Option<BoxFuture<Permit<'_,EngineMessage>>>,
                #[pin]
                flush: Option<Notified>,
            }
        }

        impl Socket {
            fn new(tx:sync::mpsc::Sender<EngineMessage>) -> Self {
                let notify:Arc<Notify> = Notify::new().into();
                Self {
                    tx, notify: notify.clone(), ready:None, flush:None
                }
            }
        }

        impl Stream for Socket {
            type Item = Result<super::EngineMessage, super::Error>;
            fn size_hint(&self) -> (usize, Option<usize>) {
                todo!()
            }
            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                todo!()
            }
        }

        impl Sink<super::EngineMessage> for Socket {
            type Error = axum::Error;

            fn poll_close(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                // WHAT ?
                todo!()
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                let this = self.project();
                if let Some(f) = this.flush.as_pin_mut() {
                    match f.poll(cx) {
                        Poll::Ready(_) => { self.flush = None; Poll::Ready(Ok(())) },
                        Poll::Pending => Poll::Pending
                    }
                }
                else {
                    this.flush = Some(this.notify.notified());
                    self.poll_flush(cx)
                }
            }

            fn start_send(
                self: Pin<&mut Self>,
                item: super::EngineMessage,
            ) -> Result<(), Self::Error> {
                // Decide Error please!
                self.tx.try_send(item).map_err(|e|todo!())
            }

            fn poll_ready(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                
                // DONE 
                if let Some(f) = self.ready {
                    f.as_mut().poll(cx).map(|a|Ok(()))
                } else {
                    self.ready = Some(self.tx.reserve());
                    self.poll_ready(cx)
                }
            }
        }

        pub struct HttpPolling {
            store: PollingSessionStore,
        }

        impl HttpPolling {
            pub(crate) fn new(store: PollingSessionStore) -> Self {
                HttpPolling { store }
            }
        }

        impl Transport for HttpPolling {
            type Socket = Socket;
            fn on_connect<C, Fut>(self, callback: C) -> axum::response::Response
            where
                C: FnOnce(Self::Socket) -> Fut + Send + 'static,
                Fut: futures::prelude::Future<Output = ()> + Send + 'static,
            {
                let (ses_tx,ses_rx) = sync::mpsc::channel(32);
                let socket = Socket::new(ses_tx);
                let n = socket.notify.clone();


                let store = self.store.clone();
                let store = store.pin();
                let id = uuid7();
                let (req_tx,req_rx) = sync::mpsc::channel(32);
                store.insert(id.clone(),req_tx);


                tokio::spawn(move || {
                    let user_session = callback(socket);
                    pin!(user_session);
                    let p = None;
                    loop {
                        tokio::select! {
                            _ = &mut user_session => break,
                            Some(req) = req_rx.recv() => {
                                match req {
                                    PollingMessage::Poll(tx) => { p = Some(tx); n.notify_one(); }
                                    PollingMessage::Close() => { 
                                        ses_rx.close();
                                        // How to drain ...
                                    }
                                    PollingMessage::Message(m) => {
                                        // is there away to avoid all this channel stuff ?
                                    }
                                }

                            }
                        }
                    }
                    // After loop clean up store? 
                    store.remove(&id)

                })

                // The reponse needs to include the SIO id ...
            }
        }
    }

    pub mod ws {
        use axum::extract::ws::WebSocketUpgrade;
        use futures::{Sink, Stream};
        use pin_project_lite::pin_project;
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };

        pin_project! {
            pub struct SocketAdapter {
                #[pin]
                pub inner: axum::extract::ws::WebSocket,
            }
        }

        impl Stream for SocketAdapter {
            type Item = Result<super::EngineMessage, super::Error>;

            fn size_hint(&self) -> (usize, Option<usize>) {
                self.inner.size_hint()
            }

            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                let this = self.project();
                match this.inner.poll_next(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Ready(Some(res)) => {
                        Poll::Ready(Some(res.map(super::EngineMessage::from)))
                    }
                }
            }
        }
        impl Sink<super::EngineMessage> for SocketAdapter {
            type Error = axum::Error;

            fn poll_close(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                todo!()
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                todo!()
            }
            fn start_send(
                self: Pin<&mut Self>,
                item: super::EngineMessage,
            ) -> Result<(), Self::Error> {
                todo!()
            }
            fn poll_ready(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                todo!()
            }
        }

        pub struct WebSocket {
            pub inner: WebSocketUpgrade,
        }

        impl super::Transport for WebSocket {
            type Socket = SocketAdapter;

            fn on_connect<C, Fut>(self, callback: C) -> super::Response
            where
                C: FnOnce(Self::Socket) -> Fut + Send + 'static,
                Fut: super::Future<Output = ()> + Send + 'static,
            {
                self.inner
                    .on_upgrade(|s| callback(SocketAdapter { inner: s }))
            }
        }
    }
}

impl From<EngineMessage> for Message {
    fn from(value: EngineMessage) -> Self {
        todo!()
    }
}

macro_rules! engine_define {
    (enum $name:ident {$($var:ident($p:path)),*}) => {
        pub enum $name {
            $(
                $var($p),
            )*
        }

        pub enum Socket {
            $(
                $var(<$p as transport::Transport>::Socket),
            )*
        }

        $(
            impl std::convert::From<<$p as transport::Transport>::Socket> for Socket {
                fn from(value: <$p as transport::Transport>::Socket) -> Socket {
                    Socket::$var(value)
                }
            }
        )*

        use futures::stream::{self, StreamExt};
        impl Socket {
             pub async fn recv(&mut self) -> Option<Result<crate::EngineMessage, axum::Error>> {
                self.next().await
            }

             pub async fn send<T>(&mut self, msg:T) -> Result<(),axum::Error> where T:Into<EngineMessage> {
                todo!()
             }

        }

        impl futures::Stream for Socket {
            type Item = Result<crate::EngineMessage,axum::Error>;

            fn size_hint(&self) -> (usize, Option<usize>) {
                todo!()
            }

            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                todo!()
            }
        }
        impl Sink<EngineMessage> for Socket {
            type Error = axum::Error;

            fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                todo!()
            }
            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                todo!()
            }
            fn start_send(self: Pin<&mut Self>, item: EngineMessage) -> Result<(), Self::Error> {
                todo!()
            }
            fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                todo!()
            }
        }


        impl transport::Transport for $name {
            type Socket = Socket;

            fn on_connect<C, Fut>(self, callback: C) -> Response
            where
                C: FnOnce(Self::Socket) -> Fut + Send + 'static,
                Fut: Future<Output = ()> + Send + 'static,
            {
                match self {
                    $(Self::$var(t) => t.on_connect(|s|callback(Socket::from(s))),)*
                }
            }
        }

        $(
            impl std::convert::From<$p> for $name {
                fn from(value: $p) -> $name {
                   Self::$var(value)
                }
            }
        )*
    };
}

engine_define! {
    enum Engine {
        WebSocket(transport::ws::WebSocket),
        Polling(transport::polling::HttpPolling)
    }
}

#[derive(Debug)]
pub enum EngineMessage {
    Open(Utf8Bytes),
    Close(Utf8Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Msg(Bytes),
    Upgrade,
    Noop,
}

impl From<Message> for EngineMessage {
    fn from(value: Message) -> Self {
        todo!()
    }
}

#[derive(Deserialize)]
struct EngineQuery {
    transport: String,
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
        if params.sid.is_some() {
            return Err(EngineError::Session);
        }

        let e = match params.transport.as_str() {
            "websocket" => {
                let Ok(upgrade) =
                    axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, state).await
                else {
                    return Err(EngineError::Session);
                };
                Engine::from(transport::ws::WebSocket { inner: upgrade })
            }
            "polling" => {
                let Ok(ext) =
                    Extension::<EngineReqExtension>::from_request_parts(parts, state).await
                else {
                    return Err(EngineError::Session);
                };
                Engine::from(transport::polling::HttpPolling::new(ext.store.clone()))
            }
            _ => return Err(EngineError::Unknown),
        };
        Ok(e)
    }
}

// ====

#[cfg(test)]
mod tests {
    use crate::{engine_io, transport::Transport, Engine};
    use axum::{response::Response, Router};
    use futures::StreamExt;

    #[test]
    fn init() {
        #[axum::debug_handler]
        async fn test(e: Engine) -> Response {
            e.on_connect(|mut socket| async move {
                let m = socket.next().await;
            })
        }
        let s = engine_io(test, ());
        //let r: Router<()> = Router::new().route_service("/rtc", s); //.with_state(()));
    }
}
