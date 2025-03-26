use axum_longpoll::HTTPLongpollService;

use axum::{
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};

#[tokio::main]
async fn main() {
    // build our application with a route
    let app = Router::new()
        .route("/session", post(session_new))
        .route("/session/$id", get(session_poll));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn session_new() -> &'static str {
    "Hello, World!"
}

async fn session_poll() -> &'static str {
    "Hello, World!"
}
