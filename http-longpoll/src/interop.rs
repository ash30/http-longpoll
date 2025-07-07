use crate::http_poll::Foldable;

#[cfg(feature = "axum")]
pub mod axum {
    use crate::http_poll::{Foldable, TotalSize};
    use crate::session::{self};
    use axum::response::IntoResponse;
    pub use bytes::Bytes;
    use bytes::BytesMut;
    use http::StatusCode;

    pub type Session<T = Bytes> = session::Session<T>;
    pub type SessionHandle<T = Bytes> = session::SessionHandle<T>;

    // Allow common response types to be used
    impl Foldable for Bytes {
        type Start = BytesMut;
        fn append(current: &mut Self::Start, value: Self) -> TotalSize {
            current.extend_from_slice(&value);
            current.len()
        }
    }

    impl IntoResponse for session::SessionError {
        fn into_response(self) -> axum::response::Response {
            match self {
                Self::PollingError => StatusCode::BAD_REQUEST.into_response(),
                Self::Closed => StatusCode::GONE.into_response(),
            }
        }
    }
}

// Simple implementations independant of frameworks
impl<T> Foldable for Vec<T> {
    type Start = Self;
    fn append(current: &mut Self::Start, mut value: Self) -> crate::http_poll::TotalSize {
        current.append(&mut value);
        current.len()
    }
}
