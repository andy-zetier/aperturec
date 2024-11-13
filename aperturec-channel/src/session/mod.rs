mod client;
mod server;

pub use client::AsyncSession as AsyncClient;
pub use client::Session as Client;
pub use server::AsyncSession as AsyncServer;
pub use server::Session as Server;

/// A trait for a synchronous unified session which can be broken into component channels;
pub trait Unified {
    type Control;
    type Event;
    type Media;
    type Tunnel;

    fn split(self) -> (Self::Control, Self::Event, Self::Media, Self::Tunnel);
    fn unsplit(cc: Self::Control, ec: Self::Event, mc: Self::Media, tc: Self::Tunnel) -> Self;
}
