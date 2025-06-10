//! Server-side endpoint
use aperturec_protocol as protocol;

use crate::endpoint::*;
use crate::quic::provider;
use crate::session;
#[cfg(any(test, debug_assertions))]
use crate::tls;
use crate::util::{Syncify, new_async_rt};

use anyhow::{Result, anyhow};
#[cfg(any(test, debug_assertions))]
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

#[cfg(any(test, debug_assertions))]
use rustls::KeyLogFile;
#[allow(deprecated)]
use s2n_quic::provider::tls::rustls::{
    Server as TlsProvider, rustls, rustls::ServerConfig as TlsConfig,
};
#[cfg(any(test, debug_assertions))]
use tracing::*;

#[derive(Debug)]
/// A synchronous server endpoint
pub struct Server {
    quic_server: s2n_quic::Server,
    async_rt: Arc<TokioRuntime>,
}

impl Server {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.quic_server.local_addr()?)
    }

    pub fn accept(&mut self) -> Result<session::Server> {
        session::Server::new(
            self.quic_server
                .accept()
                .syncify(&self.async_rt)
                .ok_or(anyhow!("no connection"))?,
            self.async_rt.clone(),
        )
    }
}

#[derive(Debug)]
/// An asynchronous server endpoint
pub struct AsyncServer {
    quic_server: s2n_quic::Server,
}

impl AsyncServer {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.quic_server.local_addr()?)
    }

    pub async fn accept(&mut self) -> Result<session::AsyncServer> {
        session::AsyncServer::new(
            self.quic_server
                .accept()
                .await
                .ok_or(anyhow!("no connection"))?,
        )
        .await
    }
}

#[derive(Debug, Default)]
/// A builder for a [`Server`]
pub struct Builder {
    bind_addr: Option<String>,
    tls_pem_cert: Option<String>,
    tls_pem_private_key: Option<String>,
}

impl Builder {
    /// Set the bind address for the server. Defaults to 0.0.0.0 if left unset
    pub fn bind_addr(mut self, bind_addr: &str) -> Self {
        self.bind_addr = Some(bind_addr.to_string());
        self
    }

    /// Set the TLS certificate for the server. This is required.
    pub fn tls_pem_certificate(mut self, cert: &str) -> Self {
        self.tls_pem_cert = Some(cert.to_string());
        self
    }

    /// Set the private key for the server. This is required.
    pub fn tls_pem_private_key(mut self, private_key: &str) -> Self {
        self.tls_pem_private_key = Some(private_key.to_string());
        self
    }

    fn build(self) -> Result<(s2n_quic::Server, Option<TokioRuntime>)> {
        #[cfg(target_os = "linux")]
        super::validate_current_kernel_version()?;

        let bind_addr = self.bind_addr.ok_or(anyhow!("no bind address provided"))?;
        let bind_sa = {
            if let Ok(sa) = bind_addr.parse::<SocketAddr>() {
                sa
            } else {
                SocketAddr::from((bind_addr.parse::<IpAddr>()?, DEFAULT_SERVER_BIND_PORT))
            }
        };

        let cert_der = rustls::pki_types::CertificateDer::from(
            pem::parse(
                self.tls_pem_cert
                    .ok_or(anyhow!("no certificate provided"))?,
            )?
            .into_contents(),
        );
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(
            pem::parse(
                self.tls_pem_private_key
                    .ok_or(anyhow!("no private key provided"))?,
            )?
            .into_contents(),
        )
        .map_err(|s| anyhow!(s))?;

        let mut tls_config = TlsConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)?;
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
        let tls_provider = TlsProvider::new(tls_config);

        let io = s2n_quic::provider::io::tokio::Provider::builder()
            .with_internal_recv_buffer_size(0)?
            .with_internal_send_buffer_size(0)?
            .with_receive_address(bind_sa)?
            .build()?;
        let quic_server_builder = s2n_quic::Server::builder()
            .with_congestion_controller(s2n_quic::provider::congestion_controller::Bbr::default())?
            .with_io(io)?
            .with_tls(tls_provider)?
            .with_limits(provider::limits::default())?
            .with_event(provider::event::MetricsSubscriber)?;

        let mut rt = None;
        let quic_server = if tokio::runtime::Handle::try_current().is_err() {
            let new_rt = new_async_rt()?;
            let _guard = new_rt.enter();
            rt = Some(new_rt);
            quic_server_builder.start()
        } else {
            quic_server_builder.start()
        }?;

        Ok((quic_server, rt))
    }

    /// Build a synchronous [`Server`]
    ///
    /// This will return an error if called from an async runtime
    pub fn build_sync(self) -> Result<Server> {
        let (quic_server, async_rt_opt) = self.build()?;
        let async_rt = async_rt_opt
            .ok_or(anyhow!("building server client within an async runtime"))?
            .into();
        Ok(Server {
            quic_server,
            async_rt,
        })
    }

    /// Build an asynchronous [`Server`]
    ///
    /// This will return an error if called from outside an async runtime
    pub fn build_async(self) -> Result<AsyncServer> {
        let (quic_server, async_rt_opt) = self.build()?;

        if async_rt_opt.is_some() {
            anyhow::bail!("building async server within a sync runtime");
        }

        Ok(AsyncServer { quic_server })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use test_log::test;

    fn builder() -> Builder {
        Builder::default()
            .bind_addr("127.0.0.1:0")
            .tls_pem_certificate(&tls::test_material::PEM.certificate)
            .tls_pem_private_key(&tls::test_material::PEM.pkey)
    }

    fn builder_ipv6() -> Builder {
        Builder::default()
            .bind_addr("[::1]:0")
            .tls_pem_certificate(&tls::test_material::PEM.certificate)
            .tls_pem_private_key(&tls::test_material::PEM.pkey)
    }

    #[test]
    fn sync_build() {
        builder().build_sync().expect("server build");
    }

    #[test(tokio::test)]
    async fn async_build() {
        builder().build_async().expect("server build");
    }

    #[test]
    #[should_panic]
    fn async_build_from_sync() {
        builder().build_async().expect("server build");
    }

    #[test(tokio::test)]
    #[should_panic]
    async fn sync_build_from_async() {
        builder().build_sync().expect("server build");
    }

    #[test]
    fn sync_build_ipv6() {
        builder_ipv6().build_sync().expect("server build");
    }

    #[tokio::test]
    async fn async_build_ipv6() {
        builder_ipv6().build_async().expect("server build");
    }

    #[test]
    #[should_panic]
    fn async_build_from_sync_ipv6() {
        builder_ipv6().build_async().expect("server build");
    }

    #[tokio::test]
    #[should_panic]
    async fn sync_build_from_async_ipv6() {
        builder_ipv6().build_sync().expect("server build");
    }
}
