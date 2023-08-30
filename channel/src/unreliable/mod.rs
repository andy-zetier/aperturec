use async_trait::async_trait;

pub mod udp;

pub trait RawReceiver {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>;
}

pub trait RawSender {
    fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize>;
}

#[async_trait]
pub trait AsyncRawReceiver {
    async fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>;
}

#[async_trait]
pub trait AsyncRawSender {
    async fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize>;
}
