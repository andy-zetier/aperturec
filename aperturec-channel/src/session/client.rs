use crate::transport::{datagram, stream};
use crate::util::Syncify;
use crate::*;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

pub struct AsyncSession {
    cc: AsyncClientControl,
    ec: AsyncClientEvent,
    mc: AsyncClientMedia,
    tc: AsyncClientTunnel,
}

impl session::Unified for AsyncSession {
    type Control = AsyncClientControl;
    type Event = AsyncClientEvent;
    type Media = AsyncClientMedia;
    type Tunnel = AsyncClientTunnel;

    fn split(self) -> (Self::Control, Self::Event, Self::Media, Self::Tunnel) {
        (self.cc, self.ec, self.mc, self.tc)
    }

    fn unsplit(cc: Self::Control, ec: Self::Event, mc: Self::Media, tc: Self::Tunnel) -> Self {
        AsyncSession { cc, ec, mc, tc }
    }
}

impl AsyncSession {
    pub(crate) async fn new(mut connection: s2n_quic::Connection) -> Result<Self> {
        let cc = AsyncClientControl::new(stream::AsyncTransceiver::new(
            connection.open_bidirectional_stream().await?,
        ));
        let ec = AsyncClientEvent::new(stream::AsyncTransceiver::new(
            connection.open_bidirectional_stream().await?,
        ));
        let tc = AsyncClientTunnel::new(stream::AsyncTransceiver::new(
            connection.open_bidirectional_stream().await?,
        ));
        let mc = AsyncClientMedia::new(datagram::AsyncReceiver::new(connection));

        Ok(AsyncSession { cc, ec, tc, mc })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.mc.as_ref().as_ref().local_addr()?)
    }

    pub fn remote_addr(&self) -> Result<SocketAddr> {
        Ok(self.mc.as_ref().as_ref().remote_addr()?)
    }
}

pub struct Session {
    cc: ClientControl,
    ec: ClientEvent,
    mc: ClientMedia,
    tc: ClientTunnel,
}

impl session::Unified for Session {
    type Control = ClientControl;
    type Event = ClientEvent;
    type Media = ClientMedia;
    type Tunnel = ClientTunnel;

    fn split(self) -> (Self::Control, Self::Event, Self::Media, Self::Tunnel) {
        (self.cc, self.ec, self.mc, self.tc)
    }

    fn unsplit(cc: Self::Control, ec: Self::Event, mc: Self::Media, tc: Self::Tunnel) -> Self {
        Session { cc, ec, mc, tc }
    }
}

impl Session {
    pub(crate) fn new(
        mut connection: s2n_quic::Connection,
        async_rt: Arc<TokioRuntime>,
    ) -> Result<Self> {
        let cc = ClientControl::new(stream::Transceiver::new(
            connection.open_bidirectional_stream().syncify(&async_rt)?,
            async_rt.clone(),
        ));
        let ec = ClientEvent::new(stream::Transceiver::new(
            connection.open_bidirectional_stream().syncify(&async_rt)?,
            async_rt.clone(),
        ));
        let tc = ClientTunnel::new(stream::Transceiver::new(
            connection.open_bidirectional_stream().syncify(&async_rt)?,
            async_rt.clone(),
        ));
        let mc = ClientMedia::new(datagram::Receiver::new(connection, async_rt));

        Ok(Session { cc, ec, tc, mc })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.mc.as_ref().as_ref().local_addr()?)
    }

    pub fn remote_addr(&self) -> Result<SocketAddr> {
        Ok(self.mc.as_ref().as_ref().remote_addr()?)
    }
}
