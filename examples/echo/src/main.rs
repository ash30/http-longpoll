use std::time::Duration;

use axum::{
    extract::{Path, Request},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use axum_longpoll::{Bytes, HTTPLongpoll, Session};
use futures_util::{SinkExt, StreamExt};
use tokio::{pin, time::Instant};
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

// Example 1
async fn session_handler(s: Session) {
    let (mut tx, mut rx) = s.split();
    let client_timeout = tokio::time::sleep(Duration::from_secs(60));
    pin!(client_timeout);

    loop {
        tokio::select! {
            _ = &mut client_timeout => break,

            Some(m) = rx.next() => {
                client_timeout.as_mut().reset(Instant::now() + Duration::from_secs(60));
                match m {
                    Err(reason) => {
                        break
                    }
                    Ok(bytes) => {
                        // echo
                       let Ok(_) = tx.send(bytes).await else { break };
                    }
                }
            }
            else => {
                //
                break
            }
        }
    }
    // returning from the task will cleanup internal storage,
    // if spawning off tasks, you'll need to await them
}
