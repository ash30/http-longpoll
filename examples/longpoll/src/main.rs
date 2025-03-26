use axum::{
    extract::{Path, Request},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use axum_longpoll::HTTPLongpoll;
use uuid7::{uuid7, Uuid};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/session", post(session_new))
        .route("/session/{id}", get(session_poll))
        .layer(HTTPLongpoll::<Uuid>::new_layer());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn session_new(polling: HTTPLongpoll<Uuid>) -> impl IntoResponse {
    // we choose a UUID type to act as a key for new session
    let id = uuid7();
    polling.new(id, |s| async move {});
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
