pub mod provider;

/// Errors that can occur during QUIC operations
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A QUIC connection-level error occurred
    #[error(transparent)]
    Connection(#[from] s2n_quic::connection::Error),

    /// A QUIC stream-level error occurred
    #[error(transparent)]
    Stream(#[from] s2n_quic::stream::Error),

    /// QUIC endpoint failed to start
    #[error(transparent)]
    Start(#[from] s2n_quic::provider::StartError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn test_error_debug() {
        // Create a connection error (using immediate_close as a simple example)
        let conn_err = s2n_quic::connection::Error::immediate_close("test");
        let error = Error::from(conn_err);
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("Connection"));
    }

    #[test]
    fn test_from_connection_error() {
        let conn_err = s2n_quic::connection::Error::immediate_close("test");
        let error: Error = conn_err.into();
        assert!(matches!(error, Error::Connection(_)));
    }

    #[test]
    fn test_from_stream_error() {
        let stream_err = s2n_quic::stream::Error::invalid_stream();
        let error: Error = stream_err.into();
        assert!(matches!(error, Error::Stream(_)));
    }
}
