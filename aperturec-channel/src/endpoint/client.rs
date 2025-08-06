//! Client-side endpoint
use aperturec_protocol as protocol;

use crate::quic::provider;
use crate::util::{SyncifyLazy, new_async_rt};
use crate::*;

use anyhow::{Result, anyhow};
#[cfg(any(test, debug_assertions))]
use std::env;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;
use tracing::*;

#[cfg(any(test, debug_assertions))]
use rustls::KeyLogFile;
#[allow(deprecated)]
use s2n_quic::provider::tls::rustls::{
    Client as TlsProvider, rustls, rustls::client::ClientConfig as TlsConfig,
};

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
    ) -> Result<session::Client> {
        let tls_provider = &self.tls_provider;
        let connection =
            (|| async move { do_connect(tls_provider, server_addr, server_port).await })
                .syncify_lazy(&self.async_rt)?;
        session::Client::new(connection, self.async_rt.clone())
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
    ) -> Result<session::AsyncClient> {
        session::AsyncClient::new(do_connect(&self.tls_provider, server_addr, server_port).await?)
            .await
    }
}

async fn do_connect<P: Into<Option<u16>>>(
    tls_provider: &TlsProvider,
    server_addr: &str,
    server_port: P,
) -> Result<s2n_quic::Connection> {
    let mut connection: Option<s2n_quic::Connection> = None;
    let mut failure_reason = String::new();

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
            .start()?;

        let connect = s2n_quic::client::Connect::new(socket_addr).with_server_name(server_addr);

        match client.connect(connect).await {
            Ok(conn) => {
                connection = Some(conn);
                break;
            }
            Err(error) => {
                let reason =
                    format!("Failed to connect to {server_addr} ({socket_addr}): {error}\n",);
                debug!(reason);
                failure_reason.push_str(&reason);
            }
        }
    }

    let mut connection = connection.ok_or(anyhow!(failure_reason))?;
    connection.keep_alive(true)?;
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

    fn build(self) -> Result<(TlsProvider, Option<TokioRuntime>)> {
        #[cfg(target_os = "linux")]
        super::validate_current_kernel_version()?;

        let async_rt = if tokio::runtime::Handle::try_current().is_err() {
            Some(new_async_rt()?)
        } else {
            None
        };

        let _guard = async_rt.as_ref().map(TokioRuntime::enter);

        let mut cert_verifier = tls::CertVerifier::default();
        if !self.additional_pem_certs.is_empty() {
            let mut roots = rustls::RootCertStore::empty();
            for pem in self.additional_pem_certs {
                let certs =
                    rustls_pemfile::certs(&mut pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
                roots.add_parsable_certificates(certs);
            }
            cert_verifier.user_provided =
                Some(rustls::client::WebPkiServerVerifier::builder(Arc::new(roots)).build()?);
        }
        cert_verifier.allow_insecure_connection = self.allow_insecure_connection;
        let mut tls_config = TlsConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(cert_verifier))
            .with_no_client_auth();
        tls_config
            .alpn_protocols
            .push(protocol::MAGIC.as_bytes().to_vec());
        #[cfg(any(test, debug_assertions))]
        {
            tls_config.key_log = Arc::new(KeyLogFile::new());
            if env::var(tls::SSLKEYLOGFILE_VAR).is_ok() {
                warn!("Key logging enabled! This should never happen in production!");
            }
        }

        #[allow(deprecated)]
        Ok((TlsProvider::new(tls_config), async_rt))
    }

    /// Build a synchronous variant of the client.
    ///
    /// This will return an error if called from within an async runtime
    pub fn build_sync(self) -> Result<Client> {
        let (tls_provider, async_rt_opt) = self.build()?;

        let async_rt = async_rt_opt
            .ok_or(anyhow!("building sync client within an async runtime"))?
            .into();

        Ok(Client {
            tls_provider,
            async_rt,
        })
    }

    /// Build a asynchronous variant of the client
    ///
    /// This will return an error if not called within an async runtime
    pub fn build_async(self) -> Result<AsyncClient> {
        let (tls_provider, async_rt_opt) = self.build()?;

        if async_rt_opt.is_some() {
            anyhow::bail!("building async client within a sync runtime");
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
}
