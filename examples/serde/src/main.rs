use axum::{
    extract::{Path, Request},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use axum_longpoll::{HTTPLongpoll, Session};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use uuid7::{uuid7, Uuid};

#[tokio::main]
async fn main() {
    // create routes to init and poll
    let app = Router::new()
        .route("/session", post(session_new))
        .route("/session/{id}", get(session_poll))
        .layer(HTTPLongpoll::<Uuid>::new_layer());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    payload: String,
    from: String,
}

async fn session_new(polling: HTTPLongpoll<Uuid>) -> impl IntoResponse {
    // we choose a UUID type to act as a key for new session
    let id = uuid7();
    polling.new(id, |s| async move { session_handler(s).await });
    // return key to clients so they can poll using it
    Json(id)
}

async fn session_poll(
    polling: HTTPLongpoll<Uuid>,
    Path(session_id): Path<Uuid>,
    req: Request,
) -> impl IntoResponse {
    //  We extract the session key using dynamic path extractor
    // and forward req to existing session
    polling.forward(session_id, req).await
}

// We can Deserialize incoming poll message using arbitrary axum extractors
// The default is to collect incoming request into Bytes
// but by specialing we can provide BAD REQUEST responses to clients sending the invalid data
async fn session_handler(mut s: Session<Json<Message>>) {
    loop {
        let Some(Ok(msg)) = s.next().await else { break };
        // todo: best way to send back ...
    }
}
