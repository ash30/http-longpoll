use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::{stream, SinkExt, StreamExt};
use http_longpoll::axum::{Session, SessionHandle};
use std::sync::RwLock;
use uuid7::{uuid7, Uuid};

// Client code is responsible for holding state of long poll tasks
// and directing future request to correct key.
#[derive(Clone, Debug)]
struct LongPollState {
    sessions: Arc<RwLock<HashMap<Uuid, SessionHandle>>>,
}
impl LongPollState {
    fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()).into(),
        }
    }
}

#[tokio::main]
async fn main() {
    // create routes to init and poll longpoll connections
    let app = Router::new()
        .route("/session", post(session_new))
        .route("/session/{id}", get(session_poll))
        .route("/session/{id}", post(session_msg))
        .route("/session/{id}", delete(session_delete))
        .with_state(LongPollState::new());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// setup longpoll task with default settings and store sender for future use
async fn session_new(State(state): State<LongPollState>) -> impl IntoResponse {
    let id = uuid7();
    // client code should clean up sender on task complete
    let (sender, session) = Session::connect(http_longpoll::Config::default());
    let cleanup = (id, state.clone());
    tokio::spawn(async move {
        session_handler(session).await;
        cleanup.1.sessions.write().unwrap().remove(&cleanup.0);
    });

    state.sessions.write().unwrap().insert(id, sender);
    Json(id)
}

// use stored sender to forward request
async fn session_poll(
    State(state): State<LongPollState>,
    Path(session_id): Path<Uuid>,
) -> Response {
    let Some(mut session_handle) = state.sessions.read().unwrap().get(&session_id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    session_handle.poll().await.into_response()
}

async fn session_msg(
    State(state): State<LongPollState>,
    Path(session_id): Path<Uuid>,
    data: Bytes,
) -> Response {
    let Some(mut session_handle) = state.sessions.read().unwrap().get(&session_id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    session_handle.msg(data).await.into_response()
}

async fn session_delete(
    State(state): State<LongPollState>,
    Path(session_id): Path<Uuid>,
) -> Response {
    let Some(mut session_handle) = state.sessions.read().unwrap().get(&session_id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // clients can close long poll session and will still allow x1 poll req to drain
    session_handle.close().await.into_response()
}
// Longpoll task allows consuming code to treat the http requests
// as a continuous stream of messages
async fn session_handler(s: Session) {
    // take batch of messages from session and then send + flush back to http poll request
    let (mut tx, rx) = s.split();
    let stream = tokio_stream::StreamExt::chunks_timeout(rx, 10, Duration::from_secs(2));
    tokio::pin!(stream);

    while let Some(batch) = stream.next().await {
        let mut b = stream::iter(batch).map(Ok);
        if (tx.send_all(&mut b).await).is_err() {
            break;
        }
    }
    // Read loop breaks when session is closed by client
    // we have already flushed reamning data, exit task!
}
