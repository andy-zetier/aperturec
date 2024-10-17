//! QUIC client types
use aperturec_protocol as protocol;
use aperturec_state_machine::*;

use super::*;
use crate::transport::{datagram, stream};
use crate::util::{new_async_rt, Syncify};
use crate::*;

use anyhow::{anyhow, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;
use tracing::*;

#[cfg(any(test, debug_assertions))]
use rustls::KeyLogFile;
#[allow(deprecated)]
use s2n_quic::provider::tls::rustls::{
    rustls, rustls::client::ClientConfig as TlsConfig, Client as TlsProvider,
};

#[derive(Stateful, Debug, SelfTransitionable)]
#[state(S)]
/// A [`Stateful`] QUIC client
pub struct Client<S: State> {
    state: S,
}

/// Various states the [`Client`] may take
pub mod states {
    use super::*;

    #[derive(State)]
    /// The state for a synchronous client which is closed
    pub struct Closed {
        pub(super) server_addr: String,
        pub(super) server_port: u16,
        pub(super) tls_config: TlsConfig,
        pub(super) async_rt: Arc<TokioRuntime>,
    }

    #[derive(State)]
    /// The state for an asynchronous client which is closed
    pub struct AsyncClosed {
        pub(super) server_addr: String,
        pub(super) server_port: u16,
        pub(super) tls_config: TlsConfig,
    }

    #[derive(State, Debug)]
    /// The state for a synchronous client which has been connected to the server
    pub struct Connected {
        pub(super) connection: s2n_quic::Connection,
        pub(super) async_rt: Arc<TokioRuntime>,
    }

    #[derive(State, Debug)]
    /// The state for an asynchronous client which has been connected to the server
    pub struct AsyncConnected {
        pub(super) connection: s2n_quic::Connection,
    }

    #[derive(State, Debug)]
    /// The state for a synchronous client which has all channels initialized
    pub struct Ready {
        pub(super) cc: ClientControl,
        pub(super) ec: ClientEvent,
        pub(super) mc: ClientMedia,
        pub(super) tc: ClientTunnel,
    }

    #[derive(State, Debug)]
    /// The state for an asynchronous client which has all channels initialized
    pub struct AsyncReady {
        pub(super) cc: AsyncClientControl,
        pub(super) ec: AsyncClientEvent,
        pub(super) mc: AsyncClientMedia,
        pub(super) tc: AsyncClientTunnel,
    }
}
use states::*;

#[derive(Default)]
/// A builder for a [`Client`]
pub struct Builder {
    server_addr: Option<String>,
    server_port: Option<u16>,
    additional_pem_certs: Vec<String>,
    allow_insecure_connection: bool,
}

impl Builder {
    /// Set the server IP or domain name address. This is required.
    pub fn server_addr(mut self, server_addr: &str) -> Self {
        self.server_addr = Some(server_addr.to_string());
        self
    }

    /// Set the server port. This will default to [`DEFAULT_SERVER_BIND_PORT`] if left unspecified
    pub fn server_port(mut self, port: u16) -> Self {
        self.server_port = Some(port);
        self
    }

    /// Add additional acceptable TLS certificates, beyond those installed on the system
    pub fn additional_tls_pem_certificate(mut self, cert: &str) -> Self {
        self.additional_pem_certs.push(cert.to_string());
        self
    }

    pub fn allow_insecure_connection(mut self) -> Self {
        self.allow_insecure_connection = true;
        self
    }

    fn build(self) -> Result<(TlsConfig, String, u16, Option<TokioRuntime>)> {
        let server_addr = self.server_addr.ok_or(anyhow!("no server address"))?;
        let server_port = self.server_port.unwrap_or(DEFAULT_SERVER_BIND_PORT);

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
            if tls_config.key_log.will_log("") {
                warn!("Key logging enabled! This should never happen in production!");
            }
        }

        Ok((tls_config, server_addr, server_port, async_rt))
    }

    /// Build a synchronous variant of the client.
    ///
    /// This will return an error if called from within an async runtime
    pub fn build_sync(self) -> Result<Client<Closed>> {
        let (tls_config, server_addr, server_port, async_rt_opt) = self.build()?;

        let async_rt = async_rt_opt
            .ok_or(anyhow!("building sync client within an async runtime"))?
            .into();

        Ok(Client {
            state: Closed {
                server_addr,
                server_port,
                tls_config,
                async_rt,
            },
        })
    }

    /// Build a asynchronous variant of the client
    ///
    /// This will return an error if not called within an async runtime
    pub fn build_async(self) -> Result<Client<AsyncClosed>> {
        let (tls_config, server_addr, server_port, async_rt_opt) = self.build()?;

        if async_rt_opt.is_some() {
            anyhow::bail!("building async client within a sync runtime");
        };

        Ok(Client {
            state: AsyncClosed {
                server_addr,
                server_port,
                tls_config,
            },
        })
    }
}

impl Client<Connected> {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.state.connection.local_addr()?)
    }
}

impl Client<AsyncConnected> {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.state.connection.local_addr()?)
    }
}

impl TryTransitionable<Connected, Closed> for Client<Closed> {
    type SuccessStateful = Client<Connected>;
    type FailureStateful = Client<Closed>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let server_addr = self.state.server_addr.clone();
        let server_port = self.state.server_port;

        let server_socket_addrs: Vec<_> = try_recover!(
            (&*self.state.server_addr, self.state.server_port).to_socket_addrs(),
            self
        )
        .collect();

        if server_socket_addrs.is_empty() {
            return_recover!(
                self,
                "{}:{} did not resolve to any socket addresses",
                server_addr,
                server_port
            );
        }

        let make_client = |tls, local_ip, async_rt: &TokioRuntime| {
            let _guard = async_rt.enter();
            let io = s2n_quic::provider::io::tokio::Provider::builder()
                .with_gro_disabled()?
                .with_gso_disabled()?
                .with_receive_address((local_ip, 0).into())?
                .build()?;
            Ok::<_, anyhow::Error>(
                s2n_quic::Client::builder()
                    .with_congestion_controller(
                        s2n_quic::provider::congestion_controller::Bbr::default(),
                    )?
                    .with_event(provider::event::MetricsSubscriber)?
                    .with_tls(tls)?
                    .with_io(io)?
                    .with_limits(provider::limits::default())?
                    .start()?,
            )
        };

        let mut connection: Option<s2n_quic::Connection> = None;
        for socket_addr in server_socket_addrs {
            let local_ip = match socket_addr {
                SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            };

            let client = try_recover!(
                make_client(
                    #[allow(deprecated)]
                    TlsProvider::new(self.state.tls_config.clone()),
                    local_ip,
                    &self.state.async_rt
                ),
                self
            );
            let connect = s2n_quic::client::Connect::new(socket_addr)
                .with_server_name(&*self.state.server_addr);
            let conn_res = client.connect(connect).syncify(&self.state.async_rt);
            match conn_res {
                Ok(conn) => {
                    connection = Some(conn);
                    break;
                }
                Err(e) => {
                    debug!(
                        "Failed to connect to {}:{} ({}): {}",
                        self.state.server_addr, self.state.server_port, socket_addr, e
                    );
                }
            }
        }

        let mut connection = match connection {
            None => return_recover!(self, "Could not create connection to {}", server_addr),
            Some(connection) => connection,
        };
        try_recover!(connection.keep_alive(true), self);

        Ok(Client {
            state: Connected {
                connection,
                async_rt: self.state.async_rt,
            },
        })
    }
}

impl TryTransitionable<Ready, Connected> for Client<Connected> {
    type SuccessStateful = Client<Ready>;
    type FailureStateful = Client<Connected>;
    type Error = anyhow::Error;

    fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let cc = ClientControl::new(stream::Transceiver::new(
            try_recover!(
                self.state
                    .connection
                    .open_bidirectional_stream()
                    .syncify(&self.state.async_rt),
                self
            ),
            self.state.async_rt.clone(),
        ));

        let ec = ClientEvent::new(stream::Transceiver::new(
            try_recover!(
                self.state
                    .connection
                    .open_bidirectional_stream()
                    .syncify(&self.state.async_rt),
                self
            ),
            self.state.async_rt.clone(),
        ));

        let tc = ClientTunnel::new(stream::Transceiver::new(
            try_recover!(
                self.state
                    .connection
                    .open_bidirectional_stream()
                    .syncify(&self.state.async_rt),
                self
            ),
            self.state.async_rt.clone(),
        ));

        let mc = ClientMedia::new(datagram::Receiver::new(
            self.state.connection,
            self.state.async_rt.clone(),
        ));

        Ok(Client {
            state: Ready { cc, ec, mc, tc },
        })
    }
}

impl UnifiedClient for Client<Ready> {
    type Control = ClientControl;
    type Event = ClientEvent;
    type Media = ClientMedia;
    type Tunnel = ClientTunnel;
    type Residual = ();

    fn split(
        self,
    ) -> (
        Self::Control,
        Self::Event,
        Self::Media,
        Self::Tunnel,
        Self::Residual,
    ) {
        (
            self.state.cc,
            self.state.ec,
            self.state.mc,
            self.state.tc,
            (),
        )
    }

    fn unsplit(
        cc: Self::Control,
        ec: Self::Event,
        mc: Self::Media,
        tc: Self::Tunnel,
        _: Self::Residual,
    ) -> Self {
        Client {
            state: Ready { cc, ec, mc, tc },
        }
    }
}

impl AsyncTryTransitionable<AsyncConnected, AsyncClosed> for Client<AsyncClosed> {
    type SuccessStateful = Client<AsyncConnected>;
    type FailureStateful = Client<AsyncClosed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let server_addr = self.state.server_addr.clone();
        let server_port = self.state.server_port;

        let server_socket_addrs: Vec<_> = try_recover!(
            tokio::net::lookup_host((&*server_addr, server_port)).await,
            self
        )
        .collect();

        if server_socket_addrs.is_empty() {
            return_recover!(
                self,
                "{}:{} did not resolve to any socket addresses",
                server_addr,
                server_port
            );
        }

        let make_client = |tls, io| {
            Ok::<_, anyhow::Error>(
                s2n_quic::Client::builder()
                    .with_congestion_controller(
                        s2n_quic::provider::congestion_controller::Bbr::default(),
                    )?
                    .with_event(provider::event::MetricsSubscriber)?
                    .with_tls(tls)?
                    .with_io(io)?
                    .with_limits(provider::limits::default())?
                    .start()?,
            )
        };

        let mut connection: Option<s2n_quic::Connection> = None;
        for socket_addr in server_socket_addrs {
            let local_ip = match socket_addr {
                SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            };

            let client = try_recover!(
                make_client(
                    #[allow(deprecated)]
                    TlsProvider::new(self.state.tls_config.clone()),
                    (local_ip, 0)
                ),
                self
            );
            let connect = s2n_quic::client::Connect::new(socket_addr)
                .with_server_name(&*self.state.server_addr);
            let conn_res = client.connect(connect).await;
            match conn_res {
                Ok(conn) => {
                    connection = Some(conn);
                    break;
                }
                Err(e) => {
                    debug!(
                        "Failed to connect to {}:{} ({}): {}",
                        self.state.server_addr, self.state.server_port, socket_addr, e
                    );
                }
            }
        }

        let mut connection = match connection {
            None => return_recover!(self, "Could not create connection to {}", server_addr),
            Some(connection) => connection,
        };

        try_recover!(connection.keep_alive(true), self);

        Ok(Client {
            state: AsyncConnected { connection },
        })
    }
}

impl AsyncTryTransitionable<AsyncReady, AsyncConnected> for Client<AsyncConnected> {
    type SuccessStateful = Client<AsyncReady>;
    type FailureStateful = Client<AsyncConnected>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let cc = AsyncClientControl::new(stream::AsyncTransceiver::new(try_recover!(
            self.state.connection.open_bidirectional_stream().await,
            self
        )));

        let ec = AsyncClientEvent::new(stream::AsyncTransceiver::new(try_recover!(
            self.state.connection.open_bidirectional_stream().await,
            self
        )));

        let tc = AsyncClientTunnel::new(stream::AsyncTransceiver::new(try_recover!(
            self.state.connection.open_bidirectional_stream().await,
            self
        )));

        let mc = AsyncClientMedia::new(datagram::AsyncReceiver::new(self.state.connection));

        Ok(Client {
            state: AsyncReady { cc, ec, mc, tc },
        })
    }
}

impl AsyncUnifiedClient for Client<AsyncReady> {
    type Control = AsyncClientControl;
    type Event = AsyncClientEvent;
    type Media = AsyncClientMedia;
    type Tunnel = AsyncClientTunnel;
    type Residual = ();

    fn split(
        self,
    ) -> (
        Self::Control,
        Self::Event,
        Self::Media,
        Self::Tunnel,
        Self::Residual,
    ) {
        (
            self.state.cc,
            self.state.ec,
            self.state.mc,
            self.state.tc,
            (),
        )
    }

    fn unsplit(
        cc: Self::Control,
        ec: Self::Event,
        mc: Self::Media,
        tc: Self::Tunnel,
        _: Self::Residual,
    ) -> Self {
        Client {
            state: AsyncReady { cc, ec, mc, tc },
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use test_log::test;

    fn builder() -> Builder {
        Builder::default()
            .server_addr("localhost")
            .server_port(10000)
            .additional_tls_pem_certificate(&tls::test_material::PEM.certificate)
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
