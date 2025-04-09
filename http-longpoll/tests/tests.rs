use axum::http::Request;
use bytes::Bytes;
use futures::StreamExt;
use http_longpoll::axum::HTTPLongPoll;
use http_longpoll::Config;

#[tokio::test]
async fn forward_test() {
    let c = Config::default();
    let (mut sender, mut ses) = HTTPLongPoll::<Bytes>::connect(&c);

    let req = Request::new(Bytes::new().into());
    let b = ses.next().await.expect("").expect("");
    sender.send(req).await;
}
