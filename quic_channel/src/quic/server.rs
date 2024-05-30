//! QUIC server types
use aperturec_state_machine::*;

use crate::quic::*;
use crate::transport::{datagram, stream};
use crate::util::{common_tls_config_builder, new_async_rt, Syncify};
use crate::*;

use anyhow::{anyhow, Result};
use s2n_quic::provider::tls::default::Server as TlsProvider;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

#[derive(Stateful, Debug, SelfTransitionable)]
#[state(S)]
/// A [`Stateful`] QUIC server
pub struct Server<S: State> {
    state: S,
}

/// Various states the [`Server`] may take
pub mod states {
    use super::*;

    #[derive(State, Debug)]
    /// The state for a synchronous server which is listening for connections
    pub struct Listening {
        pub(super) quic_server: s2n_quic::Server,
        pub(super) async_rt: Arc<TokioRuntime>,
    }

    #[derive(State, Debug)]
    /// The state for an asynchronous server which is listening for connections
    pub struct AsyncListening {
        pub(super) quic_server: s2n_quic::Server,
    }

    #[derive(State, Debug)]
    /// The state for a synchronous server which has accepted a connection from a client
    pub struct Accepted {
        pub(super) connection: s2n_quic::Connection,
        pub(super) quic_server: s2n_quic::Server,
        pub(super) async_rt: Arc<TokioRuntime>,
    }

    #[derive(State, Debug)]
    /// The state for an asynchronous server which has accepted a connection from a client
    pub struct AsyncAccepted {
        pub(super) quic_server: s2n_quic::Server,
        pub(super) connection: s2n_quic::Connection,
    }

    #[derive(State, Debug)]
    /// The state for a synchronous server which has all channels initialized
    pub struct Ready {
        pub(super) cc: ServerControl,
        pub(super) ec: ServerEvent,
        pub(super) mc: ServerMedia,
        pub(super) quic_server: s2n_quic::Server,
        pub(super) async_rt: Arc<TokioRuntime>,
    }

    #[derive(State, Debug)]
    /// The state for an asynchronous server which has all channels initialized
    pub struct AsyncReady {
        pub(super) cc: AsyncServerControl,
        pub(super) ec: AsyncServerEvent,
        pub(super) mc: AsyncServerMedia,
        pub(super) quic_server: s2n_quic::Server,
    }
}
use states::*;

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
        let bind_addr = self.bind_addr.ok_or(anyhow!("no bind address provided"))?;
        let (bind_addr, bind_port) = match bind_addr.rsplit_once(':') {
            Some((addr, port)) => (addr, port.parse()?),
            None => (&*bind_addr, DEFAULT_SERVER_BIND_PORT),
        };

        let mut tls_config_builder = common_tls_config_builder()?;
        tls_config_builder.load_pem(
            self.tls_pem_cert
                .ok_or(anyhow!("no cert provided"))?
                .as_bytes(),
            self.tls_pem_private_key
                .ok_or(anyhow!("no private key provided"))?
                .as_bytes(),
        )?;

        let tls_provider = TlsProvider::from_loader(tls_config_builder.build()?);

        let datagram_endpoint = provider::datagram::EndpointBuilder::default().build()?;
        let quic_server_builder = s2n_quic::Server::builder()
            .with_io((bind_addr, bind_port))?
            .with_tls(tls_provider)?
            .with_datagram(datagram_endpoint)?
            .with_event((
                provider::event::TrxSubscriber,
                provider::event::MtuSubscriber,
            ))?;

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
    pub fn build_sync(self) -> Result<Server<Listening>> {
        let (quic_server, async_rt_opt) = self.build()?;
        let async_rt = async_rt_opt
            .ok_or(anyhow!("building server client within an async runtime"))?
            .into();
        Ok(Server {
            state: Listening {
                quic_server,
                async_rt,
            },
        })
    }

    /// Build an asynchronous [`Server`]
    ///
    /// This will return an error if called from outside an async runtime
    pub fn build_async(self) -> Result<Server<AsyncListening>> {
        let (quic_server, async_rt_opt) = self.build()?;

        if async_rt_opt.is_some() {
            anyhow::bail!("building async server within a sync runtime");
        }

        Ok(Server {
            state: AsyncListening { quic_server },
        })
    }
}

impl Server<Listening> {
    /// Get the address the server is bound and listening on
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.state.quic_server.local_addr()?)
    }
}

impl Server<AsyncListening> {
    /// Get the address the server is bound and listening on
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.state.quic_server.local_addr()?)
    }
}

impl TryTransitionable<Accepted, Listening> for Server<Listening> {
    type SuccessStateful = Server<Accepted>;
    type FailureStateful = Server<Listening>;
    type Error = anyhow::Error;

    fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut connection = try_recover!(
            self.state
                .quic_server
                .accept()
                .syncify(&self.state.async_rt)
                .ok_or(anyhow!("no connection")),
            self,
            Listening
        );

        try_recover!(connection.keep_alive(true), self);

        Ok(Server {
            state: Accepted {
                connection,
                quic_server: self.state.quic_server,
                async_rt: self.state.async_rt.clone(),
            },
        })
    }
}

impl Transitionable<Listening> for Server<Accepted> {
    type NextStateful = Server<Listening>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: Listening {
                quic_server: self.state.quic_server,
                async_rt: self.state.async_rt,
            },
        }
    }
}

impl TryTransitionable<Ready, Accepted> for Server<Accepted> {
    type SuccessStateful = Server<Ready>;
    type FailureStateful = Server<Accepted>;
    type Error = anyhow::Error;

    fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let cc = ServerControl::new(stream::Transceiver::new(
            try_recover!(
                try_recover!(
                    self.state
                        .connection
                        .accept_bidirectional_stream()
                        .syncify(&self.state.async_rt),
                    self,
                    Accepted
                )
                .ok_or(anyhow!("cc closed")),
                self,
                Accepted
            ),
            self.state.async_rt.clone(),
        ));
        let ec = ServerEvent::new(stream::Receiver::new(
            try_recover!(
                try_recover!(
                    self.state
                        .connection
                        .accept_receive_stream()
                        .syncify(&self.state.async_rt),
                    self,
                    Accepted
                )
                .ok_or(anyhow!("no event channel")),
                self,
                Accepted
            ),
            self.state.async_rt.clone(),
        ));
        let dg_tx = try_recover!(
            try_recover!(
                self.state
                    .connection
                    .datagram_mut(|dg_sender: &mut provider::datagram::Sender| dg_sender.handle()),
                self,
                Accepted
            ),
            self,
            Accepted
        );
        let mc = ServerMedia::new(datagram::Transmitter::new(dg_tx));

        Ok(Server {
            state: Ready {
                cc,
                ec,
                mc,
                quic_server: self.state.quic_server,
                async_rt: self.state.async_rt,
            },
        })
    }
}

impl Transitionable<Listening> for Server<Ready> {
    type NextStateful = Server<Listening>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: Listening {
                quic_server: self.state.quic_server,
                async_rt: self.state.async_rt,
            },
        }
    }
}

impl UnifiedServer for Server<Ready> {
    type Control = ServerControl;
    type Event = ServerEvent;
    type Media = ServerMedia;
    type Residual = Server<Listening>;

    fn split(self) -> (Self::Control, Self::Event, Self::Media, Self::Residual) {
        (
            self.state.cc,
            self.state.ec,
            self.state.mc,
            Server {
                state: Listening {
                    quic_server: self.state.quic_server,
                    async_rt: self.state.async_rt,
                },
            },
        )
    }

    fn unsplit(
        cc: Self::Control,
        ec: Self::Event,
        mc: Self::Media,
        residual: Self::Residual,
    ) -> Self {
        Server {
            state: Ready {
                cc,
                ec,
                mc,
                quic_server: residual.state.quic_server,
                async_rt: residual.state.async_rt,
            },
        }
    }
}

impl AsyncTryTransitionable<AsyncAccepted, AsyncListening> for Server<AsyncListening> {
    type SuccessStateful = Server<AsyncAccepted>;
    type FailureStateful = Server<AsyncListening>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut connection = try_recover!(
            self.state
                .quic_server
                .accept()
                .await
                .ok_or(anyhow!("no connection")),
            self
        );

        try_recover!(connection.keep_alive(true), self);

        Ok(Server {
            state: AsyncAccepted {
                connection,
                quic_server: self.state.quic_server,
            },
        })
    }
}

impl Transitionable<AsyncListening> for Server<AsyncAccepted> {
    type NextStateful = Server<AsyncListening>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: AsyncListening {
                quic_server: self.state.quic_server,
            },
        }
    }
}

impl AsyncTryTransitionable<AsyncReady, AsyncAccepted> for Server<AsyncAccepted> {
    type SuccessStateful = Server<AsyncReady>;
    type FailureStateful = Server<AsyncAccepted>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let cc = AsyncServerControl::new(stream::AsyncTransceiver::new(try_recover!(
            try_recover!(
                self.state.connection.accept_bidirectional_stream().await,
                self,
                AsyncAccepted
            )
            .ok_or(anyhow!("cc closed")),
            self,
            AsyncAccepted
        )));
        let ec = AsyncServerEvent::new(stream::AsyncReceiver::new(try_recover!(
            try_recover!(
                self.state.connection.accept_receive_stream().await,
                self,
                AsyncAccepted
            )
            .ok_or(anyhow!("no ec stream")),
            self,
            AsyncAccepted
        )));
        let dg_tx = try_recover!(
            try_recover!(
                self.state
                    .connection
                    .datagram_mut(|dg_sender: &mut provider::datagram::Sender| dg_sender.handle()),
                self,
                AsyncAccepted
            ),
            self,
            AsyncAccepted
        );
        let mc = AsyncServerMedia::new(datagram::AsyncTransmitter::new(dg_tx));

        Ok(Server {
            state: AsyncReady {
                cc,
                ec,
                mc,
                quic_server: self.state.quic_server,
            },
        })
    }
}

impl Transitionable<AsyncListening> for Server<AsyncReady> {
    type NextStateful = Server<AsyncListening>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: AsyncListening {
                quic_server: self.state.quic_server,
            },
        }
    }
}

impl AsyncUnifiedServer for Server<AsyncReady> {
    type Control = AsyncServerControl;
    type Event = AsyncServerEvent;
    type Media = AsyncServerMedia;
    type Residual = Server<AsyncListening>;

    fn split(self) -> (Self::Control, Self::Event, Self::Media, Self::Residual) {
        (
            self.state.cc,
            self.state.ec,
            self.state.mc,
            Server {
                state: AsyncListening {
                    quic_server: self.state.quic_server,
                },
            },
        )
    }

    fn unsplit(
        cc: Self::Control,
        ec: Self::Event,
        mc: Self::Media,
        residual: Self::Residual,
    ) -> Self {
        Server {
            state: AsyncReady {
                cc,
                ec,
                mc,
                quic_server: residual.state.quic_server,
            },
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn builder() -> Builder {
        Builder::default()
            .bind_addr("127.0.0.1:0")
            .tls_pem_certificate(&tls::test_material::PEM.certificate)
            .tls_pem_private_key(&tls::test_material::PEM.pkey)
    }

    #[test]
    fn sync_build() {
        builder().build_sync().expect("server build");
    }

    #[tokio::test]
    async fn async_build() {
        builder().build_async().expect("server build");
    }

    #[test]
    #[should_panic]
    fn async_build_from_sync() {
        builder().build_async().expect("server build");
    }

    #[tokio::test]
    #[should_panic]
    async fn sync_build_from_async() {
        builder().build_sync().expect("server build");
    }
}
