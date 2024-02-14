pub mod udp;

pub trait RawReceiver {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>;
}

pub trait RawSender {
    fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize>;
}

#[trait_variant::make(AsyncRawReceiver: Send)]
pub trait LocalAsyncRawReceiver {
    #[allow(async_fn_in_trait)]
    async fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>;
}

#[trait_variant::make(AsyncRawSender: Send)]
pub trait LocalAsyncRawSender {
    #[allow(async_fn_in_trait)]
    async fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize>;
}
