//! Server-side sessions
use super::{Error, Result};
use crate::session;
use crate::transport::{datagram, stream};
use crate::util::Syncify;
use crate::*;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

#[derive(Debug)]
pub struct AsyncSession {
    cc: AsyncServerControl,
    ec: AsyncServerEvent,
    mc: AsyncServerMedia,
    tc: AsyncServerTunnel,
}

impl AsyncSession {
    pub(crate) async fn new(mut connection: s2n_quic::Connection) -> Result<Self> {
        let cc = AsyncServerControl::new(stream::AsyncTransceiver::new(
            connection
                .accept_bidirectional_stream()
                .await
                .map_err(quic::Error::from)?
                .ok_or(Error::MissingCCStream)?,
        ));
        let ec = AsyncServerEvent::new(stream::AsyncTransceiver::new(
            connection
                .accept_bidirectional_stream()
                .await
                .map_err(quic::Error::from)?
                .ok_or(Error::MissingECStream)?,
        ));
        let tc = AsyncServerTunnel::new(stream::AsyncTransceiver::new(
            connection
                .accept_bidirectional_stream()
                .await
                .map_err(quic::Error::from)?
                .ok_or(Error::MissingTCStream)?,
        ));
        let mc = AsyncServerMedia::new(datagram::AsyncTransmitter::new(connection));

        Ok(AsyncSession { cc, ec, mc, tc })
    }

    pub fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
        Ok(self.mc.as_ref().as_ref().remote_addr()?)
    }
}

impl session::Unified for AsyncSession {
    type Control = AsyncServerControl;
    type Event = AsyncServerEvent;
    type Media = AsyncServerMedia;
    type Tunnel = AsyncServerTunnel;

    fn split(self) -> (Self::Control, Self::Event, Self::Media, Self::Tunnel) {
        (self.cc, self.ec, self.mc, self.tc)
    }

    fn unsplit(cc: Self::Control, ec: Self::Event, mc: Self::Media, tc: Self::Tunnel) -> Self {
        AsyncSession { cc, ec, mc, tc }
    }
}

#[derive(Debug)]
pub struct Session {
    cc: ServerControl,
    ec: ServerEvent,
    mc: ServerMedia,
    tc: ServerTunnel,
}

impl Session {
    pub(crate) fn new(
        mut connection: s2n_quic::Connection,
        async_rt: Arc<TokioRuntime>,
    ) -> Result<Self> {
        let cc = ServerControl::new(stream::Transceiver::new(
            connection
                .accept_bidirectional_stream()
                .syncify(&async_rt)
                .map_err(quic::Error::from)?
                .ok_or(Error::MissingCCStream)?,
            async_rt.clone(),
        ));
        let ec = ServerEvent::new(stream::Transceiver::new(
            connection
                .accept_bidirectional_stream()
                .syncify(&async_rt)
                .map_err(quic::Error::from)?
                .ok_or(Error::MissingECStream)?,
            async_rt.clone(),
        ));
        let tc = ServerTunnel::new(stream::Transceiver::new(
            connection
                .accept_bidirectional_stream()
                .syncify(&async_rt)
                .map_err(quic::Error::from)?
                .ok_or(Error::MissingTCStream)?,
            async_rt.clone(),
        ));
        let mc = ServerMedia::new(datagram::Transmitter::new(connection, async_rt));

        Ok(Session { cc, ec, mc, tc })
    }

    pub fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
        Ok(self.mc.as_ref().as_ref().remote_addr()?)
    }
}

impl session::Unified for Session {
    type Control = ServerControl;
    type Event = ServerEvent;
    type Media = ServerMedia;
    type Tunnel = ServerTunnel;

    fn split(self) -> (Self::Control, Self::Event, Self::Media, Self::Tunnel) {
        (self.cc, self.ec, self.mc, self.tc)
    }

    fn unsplit(cc: Self::Control, ec: Self::Event, mc: Self::Media, tc: Self::Tunnel) -> Self {
        Session { cc, ec, mc, tc }
    }
}
