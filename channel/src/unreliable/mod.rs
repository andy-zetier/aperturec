pub mod udp;

pub trait RawReceiver {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>;
}

pub trait RawSender {
    fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize>;
}

mod async_variants {
    #[trait_variant::make(AsyncRawReceiver: Send + Sync)]
    pub trait LocalAsyncRawReceiver {
        async fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>;
    }

    #[trait_variant::make(AsyncRawSender: Send + Sync)]
    pub trait LocalAsyncRawSender {
        async fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize>;
    }
}
pub use async_variants::{AsyncRawReceiver, AsyncRawSender};
