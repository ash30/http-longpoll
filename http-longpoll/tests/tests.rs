use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use futures::{Future, SinkExt, StreamExt};
use http::Method;
use http_longpoll::axum::HTTPLongPoll;
use http_longpoll::Config;
use tokio::pin;

#[tokio::test]
async fn forward_test() {
    let c = Config::default();
    let (mut sender, mut ses) = HTTPLongPoll::<Bytes>::connect(&c);

    let req = Request::builder()
        .method(Method::POST)
        .body(Bytes::new().into())
        .unwrap();

    tokio::spawn(async move { sender.send(req).await });

    let b = ses
        .next()
        .await
        .expect("stream should continue")
        .expect("message extraction is ok");

    assert!(b.is_empty())
}

use tokio::spawn;
#[tokio::test]
async fn poll_test() {
    let c = Config::default();
    let (mut sender, mut ses) = HTTPLongPoll::<Bytes>::connect(&c);

    let req = Request::builder().body(Body::empty()).unwrap();

    spawn(async move {
        ses.send(Bytes::from("Hello")).await
        //.expect("sink sends byte")
    });

    sender.send(req).await.expect("a response for poll request");
}
