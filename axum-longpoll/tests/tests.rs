use axum::{
    body::Body,
    extract::{Path, Request},
};
use axum_longpoll::{self, HTTPLongpoll, Session};
use bytes::Bytes;
use futures::{Future, StreamExt};
use tokio::sync::oneshot;

static SESSION_ID: &str = "sid";

#[tokio::test]
async fn forward_simple_request() {
    let lp = HTTPLongpoll::default();
    let (tx, rx) = oneshot::channel();

    lp.new(SESSION_ID.to_string(), |mut s: Session<Bytes>| async move {
        s.next().await;
        let _ = tx.send(1);
    });

    let _ = lp
        .forward(SESSION_ID.to_string(), Request::new(Body::empty()))
        .await;
    let r = rx
        .await
        .expect("The handler received the forwarded message");
    assert_eq!(1, r);
}
