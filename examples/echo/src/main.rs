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
    let app = create_app();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn create_app() -> Router {
    // create routes to init and poll longpoll connections
    Router::new()
        .route("/session", post(session_new))
        .route("/session/{id}", get(session_poll))
        .route("/session/{id}", post(session_msg))
        .route("/session/{id}", delete(session_delete))
        .with_state(LongPollState::new())
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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;
    use hyper::{Method, Request, StatusCode};
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    use serde_json::Value;
    use std::time::Duration;
    use tokio::time::timeout;

    async fn start_test_server() -> (String, u16) {
        use std::net::TcpListener;
        
        // Find an available port
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        drop(listener);
        
        let app = create_app();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        
        // Start server in background
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        
        // Give server time to start
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        (format!("http://127.0.0.1:{}", port), port)
    }

    #[tokio::test]
    async fn test_longpoll_session_e2e() {
        let (_base_url, port) = start_test_server().await;
        let client = Client::builder(TokioExecutor::new()).build_http();
        
        // Step 1: Create a new session
        let uri: hyper::Uri = format!("http://127.0.0.1:{}/session", port)
            .parse()
            .expect("Invalid URI");
        let req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(Full::new(Bytes::new()))
            .expect("Failed to build request");
        
        let response = client.request(req).await.expect("Failed to create session");
        assert_eq!(response.status(), StatusCode::OK);
        
        let body = response.collect().await.expect("Failed to collect body").to_bytes();
        let session_id: Value = serde_json::from_slice(&body).expect("Failed to parse session ID");
        let session_id = session_id.as_str().expect("Session ID should be a string");
        
        // Step 2: Start a long poll request in the background
        let poll_client = client.clone();
        let poll_uri = format!("http://127.0.0.1:{}/session/{}", port, session_id);
        let poll_handle = tokio::spawn(async move {
            let uri: hyper::Uri = poll_uri.parse().expect("Invalid URI");
            let req = Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Full::new(Bytes::new()))
                .expect("Failed to build request");
            
            poll_client.request(req).await
        });
        
        // Step 3: Give the poll request time to register
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        // Step 4: Send a message to the session
        let test_message = "Hello, World!";
        let uri: hyper::Uri = format!("http://127.0.0.1:{}/session/{}", port, session_id)
            .parse()
            .expect("Invalid URI");
        let req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(Full::new(Bytes::from(test_message)))
            .expect("Failed to build request");
        
        let msg_response = client.request(req).await.expect("Failed to send message");
        assert_eq!(msg_response.status(), StatusCode::OK);
        
        // Step 5: Wait for the poll request to complete and verify the response
        let poll_result = timeout(Duration::from_secs(5), poll_handle)
            .await
            .expect("Poll request timed out")
            .expect("Poll task panicked")
            .expect("Poll request failed");
        
        assert_eq!(poll_result.status(), StatusCode::OK);
        let response_body = poll_result.collect().await.expect("Failed to collect body").to_bytes();
        let response_str = String::from_utf8(response_body.to_vec()).expect("Invalid UTF-8");
        
        // The response should contain our test message
        assert_eq!(response_str, test_message);
        
        // Step 6: Clean up - delete the session
        let uri: hyper::Uri = format!("http://127.0.0.1:{}/session/{}", port, session_id)
            .parse()
            .expect("Invalid URI");
        let req = Request::builder()
            .method(Method::DELETE)
            .uri(uri)
            .body(Full::new(Bytes::new()))
            .expect("Failed to build request");
        
        let delete_response = client.request(req).await.expect("Failed to delete session");
        assert_eq!(delete_response.status(), StatusCode::OK);
    }
}
