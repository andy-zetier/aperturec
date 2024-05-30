use crate::transport;

use aperturec_trace::log;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use s2n_quic::connection as s2n_conn;
use s2n_quic::provider::datagram as s2n_dg;
use std::collections::VecDeque;
use std::task::Waker;
use tokio::sync::{mpsc, oneshot, watch};

pub type SizeSubscription = watch::Receiver<usize>;

struct SendRequest {
    data: Bytes,
    result_tx: oneshot::Sender<Result<()>>,
}

impl SendRequest {
    fn new(data: Bytes) -> (Self, oneshot::Receiver<Result<()>>) {
        let (result_tx, result_rx) = oneshot::channel();
        (SendRequest { data, result_tx }, result_rx)
    }
}

struct PendingSendRequest {
    req: SendRequest,
    num_tries: usize,
}

impl PendingSendRequest {
    fn return_failure<E: Into<anyhow::Error>>(self, e: E) {
        self.req
            .result_tx
            .send(Err(e.into()))
            .unwrap_or_else(|res| log::warn!("Failed to notify sender of failure '{:?}'", res));
    }

    fn return_success(self) {
        self.req
            .result_tx
            .send(Ok(()))
            .unwrap_or_else(|_| log::warn!("Failed to notify sender of success"));
    }
}

pub struct Sender {
    send_requests: mpsc::UnboundedReceiver<SendRequest>,
    send_requests_tx: mpsc::UnboundedSender<SendRequest>,
    pending_requests: VecDeque<PendingSendRequest>,
    waker: Waker,
    max_tries: usize,
    error: Option<s2n_conn::Error>,
}

impl s2n_dg::Sender for Sender {
    fn on_transmit<P>(&mut self, packet: &mut P)
    where
        P: s2n_dg::Packet,
    {
        if !packet.datagrams_prioritized() && packet.has_pending_streams() {
            return;
        }

        while let Ok(req) = self.send_requests.try_recv() {
            self.pending_requests
                .push_back(PendingSendRequest { req, num_tries: 0 });
        }

        if let Some(e) = self.error {
            for pending_req in self.pending_requests.drain(..) {
                pending_req.return_failure(e);
            }
            return;
        }

        let mut has_written = false;
        while let Some(pending_req) = self.pending_requests.front_mut() {
            if pending_req.num_tries >= self.max_tries {
                let pending_req = self.pending_requests.pop_front().unwrap();
                let num_tries = pending_req.num_tries;
                pending_req
                    .return_failure(anyhow!("Failed to send datagram after {} tries", num_tries));
                continue;
            }

            match packet.write_datagram(&pending_req.req.data) {
                Ok(()) => {
                    has_written = true;
                    let pending_req = self.pending_requests.pop_front().unwrap();
                    pending_req.return_success();
                }
                Err(s2n_dg::WriteError::ExceedsPacketCapacity) => {
                    if !has_written {
                        log::warn!(
                            "datagram of size {} will not fit in packet with capacity {}",
                            pending_req.req.data.len(),
                            packet.remaining_capacity()
                        );
                        pending_req.num_tries += 1;
                    }
                    break;
                }
                Err(e) => {
                    let pending_req = self.pending_requests.pop_front().unwrap();
                    let dg_len = pending_req.req.data.len();
                    pending_req.return_failure(anyhow!(
                        "unrecoverable error while attempting to send datagram of size {}: {:?}",
                        dg_len,
                        e
                    ));
                }
            }
        }

        if self.has_transmission_interest() {
            self.waker.wake_by_ref();
        }
    }

    fn has_transmission_interest(&self) -> bool {
        !self.pending_requests.is_empty() || !self.send_requests.is_empty()
    }

    fn on_connection_error(&mut self, error: s2n_conn::Error) {
        if let Some(e) = self.error {
            log::warn!("Pre-existing connection-level error being replaced: {}", e);
        }
        self.error = Some(error)
    }
}

impl Sender {
    fn new(waker: &Waker, max_tries: usize) -> Self {
        let (tx_tx, tx_rx) = mpsc::unbounded_channel();
        Sender {
            send_requests: tx_rx,
            send_requests_tx: tx_tx,
            pending_requests: VecDeque::new(),
            waker: waker.clone(),
            max_tries,
            error: None,
        }
    }

    pub fn handle(&self) -> Result<SenderHandle> {
        match self.error {
            Some(e) => Err(e.into()),
            None => Ok(SenderHandle {
                tx: self.send_requests_tx.clone(),
                waker: self.waker.clone(),
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SenderHandle {
    tx: mpsc::UnboundedSender<SendRequest>,
    waker: Waker,
}

impl transport::datagram::Transmit for SenderHandle {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        let (req, result_rx) = SendRequest::new(data);
        self.tx.send(req)?;
        self.waker.wake_by_ref();
        result_rx.blocking_recv()?
    }
}

impl transport::datagram::AsyncTransmit for SenderHandle {
    async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        let (req, result_rx) = SendRequest::new(data);
        self.tx.send(req)?;
        self.waker.wake_by_ref();
        result_rx.await?
    }
}

#[derive(Default)]
pub struct Receiver {
    rx_txs: Vec<mpsc::UnboundedSender<Bytes>>,
    error: Option<s2n_conn::Error>,
}

impl s2n_dg::Receiver for Receiver {
    fn on_datagram(&mut self, _context: &s2n_dg::ReceiveContext<'_>, datagram: &[u8]) {
        let bytes = Bytes::copy_from_slice(datagram);
        self.rx_txs.retain(|tx| tx.send(bytes.clone()).is_ok());
    }

    fn on_connection_error(&mut self, error: s2n_conn::Error) {
        if let Some(e) = self.error {
            log::warn!("Pre-existing connection-level error being replaced: {}", e);
        }
        self.error = Some(error)
    }
}

#[derive(Debug)]
pub struct ReceiverHandle {
    rx: mpsc::UnboundedReceiver<Bytes>,
}

impl Receiver {
    pub fn handle(&mut self) -> Result<ReceiverHandle> {
        match self.error {
            Some(e) => Err(e.into()),
            None => {
                let (tx, rx) = mpsc::unbounded_channel();
                self.rx_txs.push(tx);
                Ok(ReceiverHandle { rx })
            }
        }
    }
}

impl transport::datagram::Receive for ReceiverHandle {
    fn receive(&mut self) -> anyhow::Result<Bytes> {
        self.rx.blocking_recv().ok_or(anyhow!("receiver dropped"))
    }
}

impl transport::datagram::AsyncReceive for ReceiverHandle {
    async fn receive(&mut self) -> anyhow::Result<Bytes> {
        self.rx.recv().await.ok_or(anyhow!("receiver dropped"))
    }
}

const DEFAULT_MAX_SEND_TRIES: usize = 4;

#[derive(derive_builder::Builder)]
pub struct Endpoint {
    #[builder(default = "DEFAULT_MAX_SEND_TRIES")]
    max_send_tries: usize,
}

impl s2n_dg::Endpoint for Endpoint {
    type Sender = Sender;
    type Receiver = Receiver;

    fn create_connection(
        &mut self,
        info: &s2n_dg::ConnectionInfo,
    ) -> (Self::Sender, Self::Receiver) {
        (
            Sender::new(&info.waker, self.max_send_tries),
            Receiver::default(),
        )
    }

    fn max_datagram_frame_size(&self, _info: &s2n_dg::PreConnectionInfo) -> u64 {
        u16::MAX as u64
    }
}
