//#[cfg(feature = "axum")]
pub mod axum {
    use crate::http_poll::Appendable;
    use crate::session;
    use axum::response::IntoResponse;
    pub use bytes::Bytes;

    pub type Session<T = Bytes> = session::Session<T>;
    pub type SessionHandle<T = Bytes> = session::SessionHandle<T>;

    // Allow common response types to be used
    impl Appendable for Bytes {
        fn unit() -> Self {
            Bytes::new()
        }
        fn len(&self) -> usize {
            self.len()
        }
        fn append(&mut self, next: Self) {
            todo!()
        }
    }

    // Calling code can just handle method results if desired
    impl IntoResponse for session::SessionError {
        fn into_response(self) -> axum::response::Response {
            todo!()
        }
    }
}
