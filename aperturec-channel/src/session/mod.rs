use crate::quic;

pub mod client;
pub mod server;

pub use client::AsyncSession as AsyncClient;
pub use client::Session as Client;
pub use server::AsyncSession as AsyncServer;
pub use server::Session as Server;

/// Errors that occur during session creation and management
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to open or accept the control channel stream
    #[error("no control channel stream")]
    MissingCCStream,

    /// Failed to open or accept the event channel stream
    #[error("no event channel stream")]
    MissingECStream,

    /// Failed to open or accept the tunnel channel stream
    #[error("no tunnel channel stream")]
    MissingTCStream,

    /// QUIC connection error during session setup
    #[error(transparent)]
    Quic(#[from] quic::Error),
}

#[derive(Debug, Clone)]
pub struct Handle(s2n_quic::connection::Handle);

impl Handle {
    pub fn close(&self) {
        let explicit_close =
            s2n_quic::application::Error::new(0).expect("create explicit close error");
        self.0.close(explicit_close);
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A trait for a synchronous unified session which can be broken into component channels;
pub trait Unified {
    type Control;
    type Event;
    type Media;
    type Tunnel;

    fn split(self) -> (Self::Control, Self::Event, Self::Media, Self::Tunnel);
    fn unsplit(cc: Self::Control, ec: Self::Event, mc: Self::Media, tc: Self::Tunnel) -> Self;
}
