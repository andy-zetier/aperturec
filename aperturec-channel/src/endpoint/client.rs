//! Client-side endpoint
use aperturec_protocol as protocol;

use super::BuildError;
use crate::quic::{self, provider};
use crate::session;
use crate::util::{SyncifyLazy, new_async_rt};
use crate::*;

use std::convert::Infallible;
#[cfg(any(test, debug_assertions))]
use std::env;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

#[cfg(any(test, debug_assertions))]
use rustls::KeyLogFile;
#[allow(deprecated)]
use s2n_quic::provider::tls::rustls::{
    Client as TlsProvider, rustls, rustls::client::ClientConfig as TlsConfig,
};

/// Errors that occur when connecting a client to an ApertureC server.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Wraps session layer errors
    #[error(transparent)]
    Session(#[from] session::Error),

    /// An I/O error occurred during connection
    #[error(transparent)]
    IO(#[from] io::Error),

    /// A QUIC error occurred during connection
    #[error(transparent)]
    Quic(#[from] quic::Error),

    /// Connection attempts to all server addresses failed.
    ///
    /// The client tried both IPv4 and IPv6 addresses (if available) and
    /// all attempts failed. The error message contains details about the
    /// failure reasons.
    #[error("failed all attempts to connect to server: {failed_attempts:#?}")]
    AllConnectionAttemptsFailed {
        failed_attempts: Vec<s2n_quic::connection::Error>,
    },
}

// Implemented to allow `?` while configuring the Client
impl From<Infallible> for ConnectError {
    fn from(_: Infallible) -> Self {
        unreachable!("convert infallible to error");
    }
}

/// A synchronous client endpoint
pub struct Client {
    tls_provider: TlsProvider,
    async_rt: Arc<TokioRuntime>,
}

impl Client {
    pub fn connect<P: Into<Option<u16>>>(
        &mut self,
        server_addr: &str,
        server_port: P,
    ) -> Result<session::Client, ConnectError> {
        let tls_provider = &self.tls_provider;
        let connection =
            (|| async move { do_connect(tls_provider, server_addr, server_port).await })
                .syncify_lazy(&self.async_rt)?;
        Ok(session::Client::new(connection, self.async_rt.clone())?)
    }
}

/// An asynchronous client endpoint
pub struct AsyncClient {
    tls_provider: TlsProvider,
}

impl AsyncClient {
    pub async fn connect<P: Into<Option<u16>>>(
        &mut self,
        server_addr: &str,
        server_port: P,
    ) -> Result<session::AsyncClient, ConnectError> {
        Ok(session::AsyncClient::new(
            do_connect(&self.tls_provider, server_addr, server_port).await?,
        )
        .await?)
    }
}

async fn do_connect<P: Into<Option<u16>>>(
    tls_provider: &TlsProvider,
    server_addr: &str,
    server_port: P,
) -> Result<s2n_quic::Connection, ConnectError> {
    let mut connection: Option<s2n_quic::Connection> = None;
    let mut failed_attempts = vec![];

    let server_port = server_port
        .into()
        .unwrap_or(super::DEFAULT_SERVER_BIND_PORT);
    for socket_addr in tokio::net::lookup_host((server_addr, server_port)).await? {
        let bind_addr = match socket_addr {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };

        let io = s2n_quic::provider::io::tokio::Provider::builder()
            .with_internal_recv_buffer_size(0)?
            .with_internal_send_buffer_size(0)?
            .with_receive_address((bind_addr, super::DEFAULT_CLIENT_BIND_PORT).into())?
            .build()?;

        let client = s2n_quic::Client::builder()
            .with_congestion_controller(s2n_quic::provider::congestion_controller::Bbr::default())?
            .with_event(provider::event::MetricsSubscriber)?
            .with_tls(tls_provider.clone())?
            .with_io(io)?
            .with_limits(provider::limits::default())?
            .start()
            .map_err(quic::Error::from)?;

        let connect = s2n_quic::client::Connect::new(socket_addr).with_server_name(server_addr);

        match client.connect(connect).await {
            Ok(conn) => {
                connection = Some(conn);
                break;
            }
            Err(error) => {
                failed_attempts.push(error);
            }
        }
    }

    let mut connection =
        connection.ok_or(ConnectError::AllConnectionAttemptsFailed { failed_attempts })?;
    connection
        .keep_alive(true)
        .map_err(quic::Error::from)
        .map_err(session::Error::from)?;
    Ok(connection)
}

#[derive(Default)]
/// A builder for a [`Client`]
pub struct Builder {
    additional_pem_certs: Vec<String>,
    allow_insecure_connection: bool,
}

impl Builder {
    /// Add additional acceptable TLS certificates, beyond those installed on the system
    pub fn additional_tls_pem_certificate(mut self, cert: &str) -> Self {
        self.additional_pem_certs.push(cert.to_string());
        self
    }

    pub fn allow_insecure_connection(mut self) -> Self {
        self.allow_insecure_connection = true;
        self
    }

    fn build(self) -> Result<(TlsProvider, Option<TokioRuntime>), BuildError> {
        #[cfg(target_os = "linux")]
        super::validate_current_kernel_version()?;

        let async_rt = if tokio::runtime::Handle::try_current().is_err() {
            Some(new_async_rt()?)
        } else {
            None
        };

        let _guard = async_rt.as_ref().map(TokioRuntime::enter);

        let mut cert_verifier = tls::CertVerifier::new()?;
        if !self.additional_pem_certs.is_empty() {
            let mut roots = rustls::RootCertStore::empty();
            for pem in self.additional_pem_certs {
                let certs = rustls_pemfile::certs(&mut pem.as_bytes())
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                roots.add_parsable_certificates(certs);
            }
            cert_verifier.user_provided = Some(
                rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
                    .build()
                    .map_err(tls::Error::from)?,
            );
        }
        cert_verifier.allow_insecure_connection = self.allow_insecure_connection;
        let mut tls_config = TlsConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(cert_verifier))
            .with_no_client_auth();
        tls_config
            .alpn_protocols
            .push(protocol::MAGIC.as_bytes().to_vec());
        // Servers have been deployed which only have the legacy ALPN, so we need to the client
        // to support this ALPN
        tls_config
            .alpn_protocols
            .push(protocol::LEGACY_ALPN.as_bytes().to_vec());
        #[cfg(any(test, debug_assertions))]
        {
            tls_config.key_log = Arc::new(KeyLogFile::new());
            if env::var(tls::SSLKEYLOGFILE_VAR).is_ok() {
                tracing::warn!("Key logging enabled! This should never happen in production!");
            }
        }

        #[allow(deprecated)]
        Ok((TlsProvider::new(tls_config), async_rt))
    }

    /// Build a synchronous variant of the client.
    ///
    /// This will return an error if called from within an async runtime
    pub fn build_sync(self) -> Result<Client, BuildError> {
        let (tls_provider, async_rt_opt) = self.build()?;

        let async_rt = async_rt_opt.ok_or(BuildError::SyncInAsync)?.into();
        Ok(Client {
            tls_provider,
            async_rt,
        })
    }

    /// Build a asynchronous variant of the client
    ///
    /// This will return an error if not called within an async runtime
    pub fn build_async(self) -> Result<AsyncClient, BuildError> {
        let (tls_provider, async_rt_opt) = self.build()?;

        if async_rt_opt.is_some() {
            return Err(BuildError::AsyncInSync);
        };

        Ok(AsyncClient { tls_provider })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use test_log::test;

    fn builder() -> Builder {
        Builder::default().additional_tls_pem_certificate(&tls::test_material::PEM.certificate)
    }

    #[test]
    fn sync_build() {
        builder().build_sync().expect("client build");
    }

    #[test(tokio::test)]
    async fn async_build() {
        builder().build_async().expect("client build");
    }

    #[test]
    #[should_panic]
    fn async_build_from_sync() {
        builder().build_async().expect("client build");
    }

    #[test(tokio::test)]
    #[should_panic]
    async fn sync_build_from_async() {
        builder().build_sync().expect("client build");
    }

    #[test]
    fn test_connect_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ConnectError>();
    }

    #[test]
    fn test_connect_error_from_session() {
        let session_err = session::Error::MissingCCStream;
        let error: ConnectError = session_err.into();
        assert!(matches!(error, ConnectError::Session(_)));
    }

    #[test]
    fn test_connect_error_from_io() {
        let io_err = io::Error::new(io::ErrorKind::ConnectionRefused, "test");
        let error: ConnectError = io_err.into();
        assert!(matches!(error, ConnectError::IO(_)));
    }

    #[test]
    fn test_connect_error_from_quic() {
        let quic_err = quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let error: ConnectError = quic_err.into();
        assert!(matches!(error, ConnectError::Quic(_)));
    }

    #[test]
    fn test_connect_error_all_connection_attempts_failed() {
        let failed_attempts = vec![
            s2n_quic::connection::Error::immediate_close("attempt1"),
            s2n_quic::connection::Error::immediate_close("attempt2"),
        ];
        let error = ConnectError::AllConnectionAttemptsFailed {
            failed_attempts: failed_attempts.clone(),
        };
        let display_str = format!("{}", error);
        assert!(display_str.contains("failed all attempts to connect to server"));
    }

    #[test]
    fn test_connect_error_display() {
        let session_err = session::Error::MissingCCStream;
        let error = ConnectError::from(session_err);
        let display_str = format!("{}", error);
        assert!(display_str.contains("no control channel stream"));
    }

    #[test]
    fn test_connect_error_debug() {
        let io_err = io::Error::new(io::ErrorKind::ConnectionRefused, "test");
        let error = ConnectError::from(io_err);
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("IO"));
    }
}
