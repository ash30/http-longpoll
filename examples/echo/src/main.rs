use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum_longpoll::{LongPoll, Sender, Session};
use futures_util::{SinkExt, StreamExt};
use std::sync::RwLock;
use uuid7::{uuid7, Uuid};

// Client code is responsible for holding state of long poll tasks
// and directing future request to correct key.
#[derive(Clone, Debug)]
struct LongPollState {
    sessions: Arc<RwLock<HashMap<Uuid, Sender>>>,
}
impl LongPollState {
    fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()).into(),
        }
    }
}

enum Error {
    SessionNotFound,
    SessionError,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        Response::builder()
            .status(match self {
                Error::SessionNotFound => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            })
            .body(().into())
            .unwrap()
    }
}

#[tokio::main]
async fn main() {
    // create routes to init and poll longpoll connections
    let app = Router::new()
        .route("/session", post(session_new))
        .route("/session/{id}", get(session_poll))
        .with_state(LongPollState::new());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// setup longpoll task with default settings and store sender for future use
async fn session_new(State(state): State<LongPollState>) -> impl IntoResponse {
    // key for sender lookup later
    let id = uuid7();
    // client code should clean up sender on task complete
    let cleanup = (id, state.clone());

    let sender = LongPoll::default().connect(move |s| async move {
        session_handler(s).await;
        cleanup.1.sessions.write().unwrap().remove(&cleanup.0);
    });
    state.sessions.write().unwrap().insert(id, sender);
    Json(id)
}

// use stored sender to forward request
async fn session_poll(
    State(state): State<LongPollState>,
    Path(session_id): Path<Uuid>,
    req: Request,
) -> Result<Response, Error> {
    let mut session = state
        .sessions
        .read()
        .unwrap()
        .get(&session_id)
        .cloned()
        .ok_or(Error::SessionNotFound)?;

    // Session errors are fatal, client code should clean up and
    // inform client to init newconnection
    session.send(req).await.map_err(|_| Error::SessionError)
}

// Longpoll task allows consuming code to treat the disparate http requests
// as a continuous stream of messages
async fn session_handler(s: Session) {
    let (mut tx, mut rx) = s.split();
    loop {
        tokio::select! {
            Some(m) = rx.next() => {
                match m {
                    // task will be informed why connection closed
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
}
